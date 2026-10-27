//! The compacted control backlog ([`twr_core::compact`]) folds into the TUI's state as the full
//! one does: what the TUI keeps of the control stream (threads, deleted ones too, attention
//! items, usage, decisions, what the notifier remembers, the turns its messages started, the
//! `!` commands that ended, the stalls that went on) and where it stands (`last_gseq`, what it
//! has seen). Thread views leave out what no client reads of the control stream:
//! `last_activity`, `worked_at`, `last_seq`. The decision table counts the same: every
//! `decision.made` is kept.
//!
//! On made-up logs here; on a real hub's stream lines by hand (`TWR_BENCH_CONTROL`, as for the
//! bench: `crates/twr-tui/src/bench.rs`):
//!
//! ```sh
//! TWR_BENCH_CONTROL=control.ndjson cargo test -p twr-tui --lib -- --ignored compacted_real
//! ```

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use twr_core::config::TuiNotifyConfig;
use twr_core::event::Event;
use twr_core::notify::{NotifyEvent, Policy};
use twr_core::proto::{StreamItem, StreamKind};
use twr_core::reduce::{AttentionStack, ThreadView};
use twr_core::usage::UsageView;

use crate::app::{AppState, DecisionFilter, DecisionRow};
use crate::notify::{Multiplexer, Notifier};

/// What the TUI keeps of the control stream, to compare.
#[derive(Debug, PartialEq)]
pub(crate) struct Kept {
    pub(crate) views: Vec<ThreadView>,
    pub(crate) deleted: Vec<ThreadView>,
    pub(crate) attention: AttentionStack,
    pub(crate) usage: UsageView,
    pub(crate) decisions: Vec<DecisionRow>,
    pub(crate) decisions_seen: u64,
    pub(crate) policy: Policy,
    pub(crate) last_gseq: u64,
    pub(crate) seen_gseq: u64,
    pub(crate) started_refs: VecDeque<(String, i64, u64)>,
    pub(crate) acted_turns: VecDeque<(String, u64)>,
    pub(crate) shell_done: VecDeque<String>,
    pub(crate) went_on: BTreeSet<String>,
    pub(crate) incomplete: Vec<(String, u64)>,
    pub(crate) delete_cards: BTreeMap<String, Vec<String>>,
}

pub(crate) fn kept(st: &AppState, notifier: &Notifier) -> Kept {
    let strip = |v: &ThreadView| ThreadView {
        last_activity: None,
        worked_at: None,
        last_seq: 0,
        ..v.clone()
    };
    let mut deleted: Vec<ThreadView> = st.registry.deleted_views().map(strip).collect();
    deleted.sort_by(|a, b| a.id.cmp(&b.id));
    Kept {
        views: st.registry.list().into_iter().map(strip).collect(),
        deleted,
        attention: st.attention.clone(),
        usage: st.usage.clone(),
        decisions: st
            .decisions
            .newest_first(&DecisionFilter::default())
            .into_iter()
            .cloned()
            .collect(),
        decisions_seen: st.decisions.seen(),
        policy: notifier.policy().clone(),
        last_gseq: st.last_gseq,
        seen_gseq: st.seen_gseq(),
        started_refs: st.started_refs.clone(),
        acted_turns: st.acted_turns.clone(),
        shell_done: st.shell_done.clone(),
        went_on: st.summary.went_on.iter().cloned().collect(),
        incomplete: st
            .incomplete
            .iter()
            .map(|(id, i)| (id.clone(), i.revision))
            .collect(),
        delete_cards: st.delete_cards.clone(),
    }
}

/// A notifier that wants every event (what it remembers depends on what it wants).
pub(crate) fn notifier() -> Notifier {
    let cfg = TuiNotifyConfig {
        events: vec![
            NotifyEvent::Permission,
            NotifyEvent::Question,
            NotifyEvent::Confirm,
            NotifyEvent::Error,
            NotifyEvent::Review,
            NotifyEvent::Hung,
            NotifyEvent::Stalled,
            NotifyEvent::Report,
            NotifyEvent::Reply,
        ],
        ..Default::default()
    };
    Notifier::new(&cfg, Multiplexer::None)
}

/// Replays `items` as the TUI does at start, in full or compacted (`kept`: the gseqs the hub
/// keeps); returns what it keeps of them.
fn replay(items: &[StreamItem], compacted: Option<&[u64]>) -> Kept {
    let head = items.iter().filter_map(|i| i.gseq).max().unwrap_or(0);
    let mut st = AppState::new();
    let mut n = notifier();
    let now = Instant::now();
    match compacted {
        Some(kept) => {
            st.begin_compacted_catch_up(head, kept.len() as u64);
            for item in items {
                if item.gseq.is_some_and(|g| kept.binary_search(&g).is_ok()) {
                    assert!(
                        st.catch_up.is_some(),
                        "the replay is not over before its end"
                    );
                    crate::apply_control(&mut st, &mut n, item, now);
                }
            }
        }
        None => {
            st.begin_catch_up(head);
            for item in items {
                crate::apply_control(&mut st, &mut n, item, now);
            }
        }
    }
    assert_eq!(st.catch_up, None, "the replay is applied");
    assert_eq!(st.catch_up_items, None);
    kept(&st, &n)
}

/// What a hub keeps of `items` ([`twr_core::compact::select_events`]).
fn selection(items: &[StreamItem]) -> Vec<u64> {
    twr_core::compact::select_events(items.iter().map(|i| (i.gseq.unwrap(), &i.event)), 0)
}

/// `k` without what the compacted backlog leaves out of the threads purged from the trash in
/// `items` ([`twr_core::compact`]): of their deleted views, who they were only, and none of
/// the notifier's memory of their reports and replies.
fn without_purged(mut k: Kept, items: &[StreamItem]) -> Kept {
    let purged = twr_core::compact::purged_threads(items.iter().map(|i| &i.event));
    for v in &mut k.deleted {
        if purged.contains(&v.id) {
            let mut t = ThreadView::new(v.id.clone());
            t.name.clone_from(&v.name);
            t.aliases.clone_from(&v.aliases);
            t.former_names.clone_from(&v.former_names);
            t.role.clone_from(&v.role);
            t.color = v.color;
            t.created_at = v.created_at;
            *v = t;
        }
    }
    k.policy = k.policy.without_threads(&purged);
    k
}

/// Full and compacted replays of `items` keep the same (but what goes of purged threads,
/// [`without_purged`]); returns how many items the compacted one has.
fn check(what: &str, items: &[StreamItem]) -> usize {
    let kept_gseqs = selection(items);
    let full = without_purged(replay(items, None), items);
    let compacted = without_purged(replay(items, Some(&kept_gseqs)), items);
    if full != compacted {
        // The first field that differs, for a readable failure.
        assert_eq!(full.views, compacted.views, "{what}: views");
        assert_eq!(full.deleted, compacted.deleted, "{what}: deleted views");
        assert_eq!(full.attention, compacted.attention, "{what}: attention");
        assert_eq!(full.usage, compacted.usage, "{what}: usage");
        assert_eq!(full.policy, compacted.policy, "{what}: notifier");
        assert_eq!(full, compacted, "{what}");
    }
    kept_gseqs.len()
}

pub(crate) fn control_items(events: Vec<Event>) -> Vec<StreamItem> {
    events
        .into_iter()
        .zip(1..)
        .map(|(event, g)| StreamItem {
            stream: StreamKind::Control,
            gseq: Some(g),
            event,
        })
        .collect()
}

#[test]
fn compacted_replays_keep_what_full_ones_do() {
    for (name, log) in twr_core::testkit::compaction_cases() {
        check(name, &control_items(log));
    }
    let (mut all, mut compacted) = (0, 0);
    for seed in 0..60 {
        let items = control_items(twr_core::testkit::compaction_log(seed, 500));
        all += items.len();
        compacted += check(&format!("seed {seed}"), &items);
    }
    assert!(compacted < all * 9 / 10, "{compacted} of {all}");
    let bench = crate::bench::made_up_control();
    let n = check("the bench's log", &bench);
    assert!(n < bench.len() / 3, "{n} of {}", bench.len());
}

/// A compacted replay ends after its last item, which may come before the head; with none,
/// at once. The TUI then stands at the head either way, and says how far it is in items.
#[test]
fn a_compacted_replay_ends_with_its_last_item() {
    let mut st = AppState::new();
    st.begin_compacted_catch_up(10, 0);
    assert_eq!((st.catch_up, st.last_gseq), (None, 10));

    let items = control_items(twr_core::testkit::compaction_log(3, 40));
    let kept = selection(&items);
    let mut st = AppState::new();
    st.begin_compacted_catch_up(40, kept.len() as u64);
    let screen = |st: &AppState| {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 3)).unwrap();
        term.draw(|f| crate::ui::render_loading(f, st)).unwrap();
        format!("{:?}", term.backend().buffer())
    };
    assert!(screen(&st).contains(&format!("Loading… 0 of {}", kept.len())));
    for (i, g) in kept.iter().enumerate() {
        assert!(st.catch_up.is_some());
        st.apply_event(&items[*g as usize - 1]);
        if i == 0 {
            assert!(screen(&st).contains(&format!("Loading… 1 of {}", kept.len())));
        }
    }
    assert_eq!(
        (st.catch_up, st.catch_up_items, st.last_gseq),
        (None, None, 40)
    );
}

/// By hand, on a real hub's control stream (`TWR_BENCH_CONTROL`).
#[test]
#[ignore = "by hand: TWR_BENCH_CONTROL names a file of a real hub's control stream lines"]
fn compacted_real_log_keeps_what_the_full_one_does() {
    let path = std::env::var_os("TWR_BENCH_CONTROL").expect("TWR_BENCH_CONTROL");
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let items = crate::bench::decode(&lines);
    let t = Instant::now();
    let n = check("the real log", &items);
    eprintln!(
        "{} control items, {n} compacted; both replays compared in {:.1} s",
        items.len(),
        t.elapsed().as_secs_f64()
    );
    let kept = selection(&items);
    let mut kinds: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for i in &items {
        let k = kinds.entry(i.event.kind()).or_default();
        k.0 += 1;
        if kept.binary_search(&i.gseq.unwrap()).is_ok() {
            k.1 += 1;
        }
    }
    for (kind, (all, kept)) in kinds {
        eprintln!("{kind:24} {all:6} {kept:6}");
    }
}

//! What a client keeps of the control stream, folded in one place: the state a TUI builds by
//! folding every control item from the start, which the hub folds as it commits them and hands a
//! client that subscribes (`control.subscribe` with `snapshot`) instead of the items themselves.
//!
//! A client that attaches then starts from the state at the control head, whatever the length of
//! the log, and folds only the live items after it. [`ClientFold::apply`] folds an item as the
//! TUI's replay does (`AppState::apply_control_at`, then its notifier's recall): the registry,
//! the attention stack and the usage view, then what the TUI derives of the items and keeps
//! across them (the newest decisions, the turns its messages started, the turns that raised a
//! request, the `!` commands that ended, the stalls that went on, the delete cards open, the
//! requests known only by a capped preview) and what its notifier remembers ([`Policy`], for
//! every event: a client adopts it for the events it wants, [`Policy::adopt`]).
//!
//! The hub folds whole (hydrated) payloads, as a client applies them once its fetches of capped
//! fields came back; a raise the hub could not hydrate is kept in [`ClientFold::capped`], so the
//! client fetches it as it would have.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::event::{
    is_delete_card, is_job_task, message_ref, went_on, AttentionReason, Event, EventBody,
    DELETE_KEY, JOB_TASK_PREFIX,
};
use crate::notify::{NotifyEvent, Policy};
use crate::reduce::{AttentionStack, Registry};
use crate::usage::UsageView;

/// The fold a snapshot was made with (`snapshot_fold` in the `control.subscribe` ack): a hash
/// of the code that folds and serializes [`ClientFold`] (twr-core's sources and the locked
/// dependencies, `build.rs`). A client installs only a snapshot of its own fold, and asks again
/// without one otherwise: another build's reducers may fold the same items otherwise, and a
/// snapshot of them would differ from what this client's replay gives.
pub const FOLD: &str = env!("TWR_CLIENT_FOLD");

/// How many of the newest `decision.made` items a snapshot carries; a client reads older ones
/// when it wants them (`decisions.list`).
pub const SNAPSHOT_DECISIONS: usize = 200;

/// How many of the turns started from a message's ref, and of the turns that raised a request,
/// are kept (the TUI's `STARTED_REFS`): a message whose answer comes after its turn started.
pub const STARTED_REFS: usize = 32;

/// How many ended `!` commands are kept (the TUI's ring of them).
pub const SHELL_DONE: usize = 32;

/// Every notification event: the memory a client adopts for those it wants.
pub const ALL_NOTIFY_EVENTS: &[NotifyEvent] = &[
    NotifyEvent::Permission,
    NotifyEvent::Question,
    NotifyEvent::Confirm,
    NotifyEvent::Error,
    NotifyEvent::Review,
    NotifyEvent::Hung,
    NotifyEvent::Stalled,
    NotifyEvent::Report,
    NotifyEvent::Reply,
];

/// The state folded from the control items up to [`ClientFold::last_gseq`] (module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClientFold {
    pub registry: Registry,
    pub attention: AttentionStack,
    pub usage: UsageView,
    /// What a notifier wanting every event remembers of the items.
    pub policy: Policy,
    /// The newest [`SNAPSHOT_DECISIONS`] `decision.made` items, oldest first, with their gseq.
    pub decisions: VecDeque<(u64, Event)>,
    /// How many `decision.made` items there were in all.
    pub decisions_seen: u64,
    /// The turns started from a message's ref: (thread, ref, turn), oldest first.
    pub started_refs: VecDeque<(String, i64, u64)>,
    /// The turns that raised a request (a permission or a question): (thread, turn).
    pub acted_turns: VecDeque<(String, u64)>,
    /// The `!` commands (jobs of a `shell` record) that ended, by job id, oldest first.
    pub shell_done: VecDeque<String>,
    /// The stall items resolved because their thread went on.
    pub went_on: BTreeSet<String>,
    /// The open delete cards, by attention id: the names of the threads each would delete.
    pub delete_cards: BTreeMap<String, Vec<String>>,
    /// The open items whose current raise was folded as a capped preview: its revision and the
    /// raise, as stored.
    pub capped: BTreeMap<String, (u64, Event)>,
    /// The control position folded up to.
    pub last_gseq: u64,
}

impl Default for ClientFold {
    fn default() -> Self {
        ClientFold {
            registry: Registry::default(),
            attention: AttentionStack::default(),
            usage: UsageView::default(),
            policy: Policy::new(ALL_NOTIFY_EVENTS),
            decisions: VecDeque::new(),
            decisions_seen: 0,
            started_refs: VecDeque::new(),
            acted_turns: VecDeque::new(),
            shell_done: VecDeque::new(),
            went_on: BTreeSet::new(),
            delete_cards: BTreeMap::new(),
            capped: BTreeMap::new(),
            last_gseq: 0,
        }
    }
}

impl ClientFold {
    /// Folds control item `ev` at control position `gseq` (a duplicate, at or before
    /// [`ClientFold::last_gseq`], changes nothing).
    pub fn apply(&mut self, ev: &Event, gseq: Option<u64>) {
        if gseq.is_some_and(|g| g <= self.last_gseq) {
            return;
        }
        if let Some(g) = gseq {
            self.last_gseq = g;
        }
        if matches!(ev.body, EventBody::DecisionMade(_)) {
            self.decisions_seen += 1;
            push_bounded(
                &mut self.decisions,
                (gseq.unwrap_or(0), ev.clone()),
                SNAPSHOT_DECISIONS,
            );
        }
        self.registry.apply(ev);
        if let Some(id) = stall_went_on(&self.attention, ev) {
            self.went_on.insert(id);
        }
        self.attention.apply_at(ev, gseq);
        self.usage.apply(ev);
        if let Some(r) = started_ref(ev) {
            push_new(&mut self.started_refs, r, STARTED_REFS);
        }
        if let Some(a) = acted_turn(ev) {
            push_new(&mut self.acted_turns, a, STARTED_REFS);
        }
        if let Some(job) = shell_job_end(ev) {
            push_bounded(&mut self.shell_done, job, SHELL_DONE);
        }
        if let EventBody::AttentionRaised(p) = &ev.body {
            if let Some(names) = delete_card_names(&ev.thread, p) {
                self.delete_cards.insert(p.attn_id.clone(), names);
            }
            match ev.body.data().map_or(true, |d| crate::blob::has_capped(&d)) {
                true => {
                    self.capped
                        .insert(p.attn_id.clone(), (gseq.unwrap_or(0), ev.clone()));
                }
                false => {
                    self.capped.remove(&p.attn_id);
                }
            }
        }
        let attention = &self.attention;
        self.delete_cards
            .retain(|id, _| attention.get(id).is_some());
        self.capped
            .retain(|id, (rev, _)| attention.get(id).is_some_and(|a| a.revision == *rev));
        self.policy
            .recall(ev, gseq, &self.registry, &self.attention);
    }

    /// Whether `thread` runs job `job` still (its `job:<id>` task is open in the registry): the
    /// hub records a job's first running update before it answers the call that started it, so
    /// a job a client knows of and that this state does not list as running has ended (or its
    /// thread is gone).
    pub fn job_running(&self, thread: &str, job: &str) -> bool {
        job_running(&self.registry, thread, job)
    }
}

/// Whether `registry` has job `job` of `thread` running ([`ClientFold::job_running`]).
pub fn job_running(registry: &Registry, thread: &str, job: &str) -> bool {
    registry.get(thread).is_some_and(|v| {
        v.background_tasks
            .contains_key(&format!("{JOB_TASK_PREFIX}{job}"))
    })
}

/// Appends `item`, dropping the oldest past `cap`.
pub fn push_bounded<T>(ring: &mut VecDeque<T>, item: T, cap: usize) {
    ring.push_back(item);
    while ring.len() > cap {
        ring.pop_front();
    }
}

/// Appends `item` unless the ring has it, dropping the oldest past `cap`.
pub fn push_new<T: PartialEq>(ring: &mut VecDeque<T>, item: T, cap: usize) {
    if !ring.contains(&item) {
        push_bounded(ring, item, cap);
    }
}

/// `ring` (kept by a client) followed by what `newer` has that it lacks, oldest first, trimmed
/// to `cap`: what pushing the items behind `newer` onto `ring` with [`push_new`] leaves when
/// `newer` holds the last `cap` of them.
pub fn merge_rings<T: PartialEq + Clone>(ring: &mut VecDeque<T>, newer: &VecDeque<T>, cap: usize) {
    for item in newer {
        push_new(ring, item.clone(), cap);
    }
}

/// The turn control item `ev` started from a message's ref: (thread, ref, turn).
pub fn started_ref(ev: &Event) -> Option<(String, i64, u64)> {
    let EventBody::TurnStarted(p) = &ev.body else {
        return None;
    };
    Some((ev.thread.clone(), message_ref(p)?, ev.turn?))
}

/// The turn that acted in control item `ev`: one whose raise is a request (permission or
/// question). (A tool call acts too: it comes on the thread's own stream.)
pub fn acted_turn(ev: &Event) -> Option<(String, u64)> {
    match &ev.body {
        EventBody::AttentionRaised(p) if p.reason.is_blocking() => {
            Some((ev.thread.clone(), ev.turn?))
        }
        _ => None,
    }
}

/// The job id of the `!` command (or any job) control item `ev` ends, as the hub records it.
pub fn shell_job_end(ev: &Event) -> Option<String> {
    let EventBody::TaskUpdated(p) = &ev.body else {
        return None;
    };
    if p.running || !is_job_task(&ev.source, &ev.body) {
        return None;
    }
    p.task_id.strip_prefix(JOB_TASK_PREFIX).map(str::to_string)
}

/// The attention id of the stall item control item `ev` resolves because its thread went on
/// (read before `attention` drops it).
pub fn stall_went_on(attention: &AttentionStack, ev: &Event) -> Option<String> {
    let EventBody::AttentionResolved(p) = &ev.body else {
        return None;
    };
    let stalled = attention
        .get(&p.attn_id)
        .is_some_and(|i| i.reason == AttentionReason::Stalled);
    (stalled && went_on(&p.ext)).then(|| p.attn_id.clone())
}

/// The names of the threads delete card `p` (raised on `thread`) would delete, if it is one
/// ("a thread" when it names none).
pub fn delete_card_names(thread: &str, p: &crate::event::AttentionRaised) -> Option<Vec<String>> {
    if !is_delete_card(thread, &p.ext) {
        return None;
    }
    let mut names: Vec<String> = p
        .ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(DELETE_KEY))
        .and_then(|d| d.get("threads"))
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    if names.is_empty() {
        names.push("a thread".into());
    }
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AttentionRaised, AttentionResolved, TaskUpdated, TurnResult};
    use crate::testkit::ThreadScript;

    fn fold(events: &[Event]) -> ClientFold {
        let mut f = ClientFold::default();
        for (i, ev) in events.iter().enumerate() {
            f.apply(ev, Some(i as u64 + 1));
        }
        f
    }

    #[test]
    fn rings_keep_the_newest_and_no_twice() {
        let mut r = VecDeque::new();
        for i in 0..40 {
            push_new(&mut r, i % 35, 32);
        }
        // 0..35 leave 3..=34; then 0, 1, 2 come back (popping 3, 4, 5), and so do 3 and 4.
        assert_eq!(r.len(), 32);
        assert_eq!(
            r.iter().rev().take(5).copied().collect::<Vec<_>>(),
            [4, 3, 2, 1, 0]
        );
        let mut kept: VecDeque<u32> = [1, 2, 3].into();
        merge_rings(&mut kept, &[3, 4, 5].into(), 4);
        assert_eq!(kept, VecDeque::from([2, 3, 4, 5]));
    }

    #[test]
    fn a_duplicate_changes_nothing_and_jobs_and_requests_are_kept() {
        let mut s = ThreadScript::new("t-a");
        let created = s.created("api");
        let started = s.turn_started("go");
        let raised = s.event(EventBody::AttentionRaised(AttentionRaised {
            attn_id: "a1".into(),
            reason: AttentionReason::Permission,
            summary: "run tests?".into(),
            ..Default::default()
        }));
        let job_end = Event::new(
            "t-a",
            99,
            None,
            crate::event::Source::Hub,
            EventBody::TaskUpdated(TaskUpdated {
                task_id: format!("{JOB_TASK_PREFIX}j1"),
                running: false,
                ..Default::default()
            }),
        );
        let ended = s.turn_ended(TurnResult::Completed);
        let f = fold(&[created, started, raised.clone(), job_end, ended]);
        assert_eq!(f.last_gseq, 5);
        assert!(f.attention.get("a1").is_some());
        assert_eq!(f.acted_turns, VecDeque::from([("t-a".to_string(), 1)]));
        let mut again = f.clone();
        again.apply(&raised, Some(3));
        assert_eq!(again, f, "a duplicate is folded once");
        let resolved = s.event(EventBody::AttentionResolved(AttentionResolved {
            attn_id: "a1".into(),
            ..Default::default()
        }));
        again.apply(&resolved, Some(6));
        assert!(again.attention.get("a1").is_none());
        assert!(again.went_on.is_empty(), "a permission is no stall");
    }

    #[test]
    fn it_round_trips_through_json() {
        let mut s = ThreadScript::new("t-a");
        let f = fold(&[s.created("api"), s.turn_started("go")]);
        let back: ClientFold = serde_json::from_slice(&serde_json::to_vec(&f).unwrap()).unwrap();
        assert_eq!(back, f);
    }
}

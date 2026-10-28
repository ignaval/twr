//! The control stream's snapshot ([`crate::snapshot`]) leaves the TUI as the replay of the
//! items up to its head does: what it keeps of the control stream ([`Kept`]: threads, deleted
//! ones too, attention items, usage, decisions once the older ones are read, what the notifier
//! remembers, the turns its messages started, the `!` commands that ended, the stalls that went
//! on, the delete cards, the raises known by their capped preview, where it stands), and the
//! live items after the head do the same to both, notifications included. A view kept across
//! a workspace switch comes back with its `!` commands and its last message as after a replay.
//!
//! On made-up logs here; on a real hub's stream lines by hand (`TWR_BENCH_CONTROL`, as for the
//! bench: `crates/twr-tui/src/bench.rs`):
//!
//! ```sh
//! TWR_BENCH_CONTROL=control.ndjson cargo test -p twr-tui --lib -- --ignored snapshot_real
//! ```

use std::time::{Duration, Instant};

use serde_json::{json, Value};
use twr_core::client_state::ClientFold;
use twr_core::config::{TerminalAlert, TuiNotifyConfig};
use twr_core::event::{
    AttentionReason, AttentionResolved, DecisionMade, Event, EventBody, ResolvedBy, Source,
    TaskUpdated, ThreadDeleted, TurnResult, HUB_THREAD, JOB_TASK_PREFIX,
};
use twr_core::notify::NotifyEvent;
use twr_core::proto::StreamItem;
use twr_core::testkit::ThreadScript;

use crate::app::{AppState, DecisionFilter, Screen, Sent, DECISIONS_CAP};
use crate::compact_tests::{control_items, kept, Kept};
use crate::notify::{Multiplexer, Notifier};

/// A notifier for `events` that alerts whatever has focus: what it gives is what it holds.
fn alerting(events: &[NotifyEvent]) -> Notifier {
    let cfg = TuiNotifyConfig {
        terminal: TerminalAlert::Osc9,
        when: twr_core::config::NotifyWhen::Always,
        events: events.to_vec(),
    };
    Notifier::new(&cfg, Multiplexer::None)
}

fn all_events() -> Notifier {
    alerting(twr_core::client_state::ALL_NOTIFY_EVENTS)
}

fn head(items: &[StreamItem]) -> u64 {
    items.iter().filter_map(|i| i.gseq).max().unwrap_or(0)
}

/// What the hub folds of `items` into its snapshot.
fn fold(items: &[StreamItem]) -> ClientFold {
    let mut f = ClientFold::default();
    for i in items {
        f.apply(&i.event, i.gseq);
    }
    f
}

/// What a hub's `decisions.list` answers of `items`: the `decision.made` items before
/// `before`, newest first, at most `limit`.
fn decisions_list(items: &[StreamItem], before: u64, limit: usize) -> Value {
    let found: Vec<&StreamItem> = items
        .iter()
        .rev()
        .filter(|i| i.gseq.is_some_and(|g| g < before))
        .filter(|i| matches!(i.event.body, EventBody::DecisionMade(_)))
        .collect();
    let listed: Vec<Value> = found
        .iter()
        .take(limit)
        .map(|i| json!({"gseq": i.gseq, "event": i.event}))
        .collect();
    json!({"items": listed, "more": found.len() > limit})
}

/// `st` into which `items` were replayed from the start (`st` may hold a view kept for its
/// workspace), with `n`.
fn replay_into(mut st: AppState, mut n: Notifier, items: &[StreamItem]) -> (AppState, Notifier) {
    let now = Instant::now();
    st.begin_catch_up(head(items));
    for item in items {
        crate::apply_control(&mut st, &mut n, item, now);
    }
    assert_eq!(st.catch_up, None, "the replay is applied");
    (st, n)
}

/// `st` with the snapshot of `items` installed, with `n` adopting its notification memory;
/// the older decisions not read yet.
fn install_into(mut st: AppState, mut n: Notifier, items: &[StreamItem]) -> (AppState, Notifier) {
    let f = fold(items);
    n.adopt(&f.policy);
    st.install_snapshot(f, head(items));
    assert_eq!((st.catch_up, st.catch_up_items), (None, None));
    (st, n)
}

/// Reads the decisions older than the snapshot's, as the hub holding `items` answers.
fn read_older(st: &mut AppState, items: &[StreamItem]) {
    if let Some((before, limit)) = st.older_decisions.take() {
        st.older_decisions_came(&decisions_list(items, before, limit));
    }
}

fn replayed(items: &[StreamItem], n: Notifier) -> (AppState, Notifier) {
    replay_into(AppState::new(), n, items)
}

fn installed(items: &[StreamItem], n: Notifier) -> (AppState, Notifier) {
    let (mut st, n) = install_into(AppState::new(), n, items);
    read_older(&mut st, items);
    (st, n)
}

/// Both keep the same of the control stream, and show the same.
fn same(what: &str, a: (&AppState, &Notifier), b: (&AppState, &Notifier)) {
    let (ka, kb): (Kept, Kept) = (kept(a.0, a.1), kept(b.0, b.1));
    if ka != kb {
        // The first field that differs, for a readable failure.
        assert_eq!(ka.views, kb.views, "{what}: views");
        assert_eq!(ka.deleted, kb.deleted, "{what}: deleted views");
        assert_eq!(ka.attention, kb.attention, "{what}: attention");
        assert_eq!(ka.usage, kb.usage, "{what}: usage");
        assert_eq!(
            ka.decisions_seen, kb.decisions_seen,
            "{what}: decisions seen"
        );
        assert_eq!(ka.decisions.len(), kb.decisions.len(), "{what}: decisions");
        assert_eq!(ka.policy, kb.policy, "{what}: notifier");
        assert_eq!(ka, kb, "{what}");
    }
    // Every part of the threads' views, not only what the compacted backlog keeps alike.
    assert!(a.0.registry == b.0.registry, "{what}: registry");
    assert_eq!(a.0.main_thread(), b.0.main_thread(), "{what}: main pane");
    assert_eq!(a.0.view.focus, b.0.view.focus, "{what}: selection");
    assert_eq!(
        a.0.view.picker.is_some(),
        b.0.view.picker.is_some(),
        "{what}: picker"
    );
}

/// Applies `tail` live to `st` (a minute apart) and returns what `n` alerts after each item.
fn live(st: &mut AppState, n: &mut Notifier, tail: &[StreamItem]) -> Vec<Option<String>> {
    let base = Instant::now();
    tail.iter()
        .enumerate()
        .map(|(i, item)| {
            let now = base + Duration::from_secs(60 * i as u64);
            crate::apply_control(st, n, item, now);
            n.flush(&st.registry, &st.attention, now + Duration::from_secs(30))
        })
        .collect()
}

/// The snapshot of `items[..cut]` installs what their replay leaves; the items after it then
/// do the same to both, and alert alike, for a notifier wanting every event and for one
/// wanting the default ones (which adopts the memory of every event). Returns how many alerts
/// the tail gave.
fn check(what: &str, items: &[StreamItem], cut: usize) -> usize {
    let (past, tail) = items.split_at(cut);
    let (mut r, mut rn) = replayed(past, all_events());
    let (mut s, mut sn) = installed(past, all_events());
    same(what, (&r, &rn), (&s, &sn));
    let alerts = live(&mut r, &mut rn, tail);
    assert_eq!(alerts, live(&mut s, &mut sn, tail), "{what}: alerts");
    same(&format!("{what}, live"), (&r, &rn), (&s, &sn));

    let (mut r, mut rn) = replayed(past, alerting(NotifyEvent::DEFAULTS));
    let (mut s, mut sn) = installed(past, alerting(NotifyEvent::DEFAULTS));
    let default_alerts = live(&mut r, &mut rn, tail);
    assert_eq!(
        default_alerts,
        live(&mut s, &mut sn, tail),
        "{what}: alerts of the default events"
    );
    alerts.iter().flatten().count()
}

fn set_ref(ev: &mut Event, r: i64) {
    if let EventBody::TurnStarted(p) = &mut ev.body {
        p.ext = json!({"twr": {"ref": r}}).as_object().cloned();
    }
}

fn job(s: &mut ThreadScript, id: &str, running: bool) -> Event {
    let mut ev = s.event(EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("{JOB_TASK_PREFIX}{id}"),
        running,
        ext: json!({"twr": {"job": {"shell": {"command": "ls"}}}})
            .as_object()
            .cloned(),
        ..Default::default()
    }));
    ev.source = Source::Hub;
    ev
}

fn orchestrator(s: &mut ThreadScript) -> Event {
    let mut ev = s.created("orch");
    if let EventBody::ThreadCreated(p) = &mut ev.body {
        p.ext = json!({"twr": {"role": "orchestrator"}})
            .as_object()
            .cloned();
    }
    ev
}

fn decision(hub: &mut ThreadScript, i: usize) -> Event {
    hub.event(EventBody::DecisionMade(DecisionMade {
        chosen: format!("d{i}"),
        ..Default::default()
    }))
}

/// A made-up log of what the generated ones lack: turns started from messages' refs (more
/// than the TUI keeps), requests raised in them, stalls closed because their thread went on
/// (and one closed otherwise), a delete card, raises known only by their capped preview (one
/// raised again whole), `!` commands ending, and `decisions` decisions.
fn targeted_log(decisions: usize) -> Vec<Event> {
    let mut o = ThreadScript::new("o");
    let mut w = ThreadScript::new("w");
    let mut hub = ThreadScript::new(HUB_THREAD);
    let mut v = vec![orchestrator(&mut o), w.created("api"), o.started("s")];
    v.push(w.started("s"));
    let mut d = 0;
    for r in 1..=40 {
        let mut ev = o.turn_started("go");
        set_ref(&mut ev, r);
        v.push(ev);
        if r % 3 == 0 {
            let id = format!("p{r}");
            v.push(o.raise(&id, AttentionReason::Permission));
            v.push(o.resolve(&id));
        }
        v.push(o.turn_ended(TurnResult::Completed));
        v.push(o.report(Some(r as u64), twr_core::event::Grade::Success));
        for _ in 0..decisions / 80 {
            v.push(decision(&mut hub, d));
            d += 1;
        }
        let j = format!("J{r}");
        v.push(job(&mut w, &j, true));
        if r % 4 != 3 {
            v.push(job(&mut w, &j, false));
        }
    }
    let stall_end = |s: &mut ThreadScript, id: &str, went_on: bool| {
        s.event(EventBody::AttentionResolved(AttentionResolved {
            attn_id: id.into(),
            by: ResolvedBy::Timeout,
            ext: went_on
                .then(|| json!({"twr": {"went_on": true}}).as_object().cloned())
                .flatten(),
            ..Default::default()
        }))
    };
    v.push(w.turn_started("work"));
    v.push(w.raise("s1", AttentionReason::Stalled));
    v.push(stall_end(&mut w, "s1", true));
    v.push(w.raise("s2", AttentionReason::Stalled));
    v.push(stall_end(&mut w, "s2", false));
    v.push(w.raise("s3", AttentionReason::Stalled));
    let mut card = hub.raise("del", AttentionReason::Question);
    if let EventBody::AttentionRaised(p) = &mut card.body {
        p.ext = json!({"twr": {"delete": {"threads": [{"name": "api"}]}}})
            .as_object()
            .cloned();
    }
    v.push(card);
    let capped = |s: &mut ThreadScript, id: &str| {
        let mut ev = s.raise(id, AttentionReason::Question);
        if let EventBody::AttentionRaised(p) = &mut ev.body {
            p.ext = json!({"twr": {"options_detail": {"_capped": 4000}}})
                .as_object()
                .cloned();
        }
        ev
    };
    v.push(capped(&mut w, "q1"));
    v.push(capped(&mut w, "q2"));
    v.push(w.raise("q2", AttentionReason::Question));
    while d < decisions {
        v.push(decision(&mut hub, d));
        d += 1;
    }
    let mut ev = o.turn_started("last");
    set_ref(&mut ev, 41);
    v.push(ev);
    v.push(o.raise("p41", AttentionReason::Question));
    v
}

#[test]
fn a_snapshot_installs_what_the_replay_leaves() {
    let mut alerts = 0;
    for (name, log) in twr_core::testkit::compaction_cases() {
        let items = control_items(log);
        for cut in [0, items.len() / 2, items.len()] {
            alerts += check(&format!("{name} at {cut}"), &items, cut);
        }
    }
    for seed in 0..20 {
        let items = control_items(twr_core::testkit::compaction_log(seed, 500));
        alerts += check(&format!("seed {seed}"), &items, 400);
    }
    for decisions in [0, 150, DECISIONS_CAP + 300] {
        let items = control_items(targeted_log(decisions));
        for cut in [items.len() / 3, items.len() * 2 / 3, items.len()] {
            alerts += check(
                &format!("targeted, {decisions} decisions, at {cut}"),
                &items,
                cut,
            );
        }
    }
    let bench = crate::bench::made_up_control();
    alerts += check("the bench's log", &bench, bench.len() * 9 / 10);
    assert!(alerts > 20, "the tails alert ({alerts})");
}

/// The targeted log exercises what it is for: rings past their cap, a went-on stall, a delete
/// card, a capped raise (the other raised again whole), decisions read before the kept ones.
#[test]
fn the_targeted_log_covers_what_the_snapshot_carries() {
    let items = control_items(targeted_log(DECISIONS_CAP + 300));
    let f = fold(&items);
    assert_eq!(f.started_refs.len(), 32);
    assert!(f.acted_turns.len() > 10);
    assert_eq!(f.shell_done.len(), 30);
    assert_eq!(f.went_on.iter().collect::<Vec<_>>(), ["s1"]);
    assert_eq!(f.delete_cards["del"], ["api"]);
    assert_eq!(f.capped.keys().collect::<Vec<_>>(), ["q1"]);
    assert_eq!(
        f.decisions.len(),
        twr_core::client_state::SNAPSHOT_DECISIONS
    );
    let (st, _) = install_into(AppState::new(), all_events(), &items);
    let rows = twr_core::client_state::SNAPSHOT_DECISIONS;
    assert_eq!(st.decisions.len(), rows);
    assert_eq!(st.decisions.seen(), (DECISIONS_CAP + 300) as u64);
    assert_eq!(
        st.older_decisions,
        Some((f.decisions[0].0, DECISIONS_CAP - rows))
    );
    assert!(st.incomplete.contains_key("q1"));
    // Nothing older to read when the snapshot holds them all.
    let items = control_items(targeted_log(150));
    let (st, _) = install_into(AppState::new(), all_events(), &items);
    assert_eq!((st.decisions.len(), st.older_decisions), (150, None));
}

/// Older decisions go before the kept rows, and live ones that came meanwhile stay: the
/// table is the replay's. The selected row stays selected, also one past the end (drawn on
/// the last row). A hub without `decisions.list` (or a failed read) leaves the rows as they
/// are.
#[test]
fn older_decisions_go_before_the_kept_ones() {
    let all = control_items(targeted_log(DECISIONS_CAP + 100));
    let mut hub = ThreadScript::new(HUB_THREAD);
    let mut extra: Vec<Event> = all.iter().map(|i| i.event.clone()).collect();
    for i in 0..50 {
        extra.push(decision(&mut hub, 90_000 + i));
    }
    let items = control_items(extra);
    let (past, tail) = items.split_at(all.len());
    for past_end in [false, true] {
        let (mut s, mut sn) = install_into(AppState::new(), all_events(), past);
        let unread = s.decisions.len();
        s.older_decisions_came(&Value::Null);
        s.older_decisions_came(&json!({"items": []}));
        assert_eq!(s.decisions.len(), unread);
        live(&mut s, &mut sn, tail);
        s.view.screen = Screen::Decisions;
        s.view.decision_sel = if past_end { 10_000 } else { 7 };
        let selected = s.decision_rows()[s.decision_selected()].id.clone();
        read_older(&mut s, &items);
        assert_eq!(s.decision_rows()[s.decision_selected()].id, selected);

        let (r, rn) = replayed(&items, all_events());
        let rows = |st: &AppState| {
            st.decisions
                .newest_first(&DecisionFilter::default())
                .into_iter()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&s).len(), DECISIONS_CAP);
        assert_eq!(rows(&s), rows(&r));
        assert_eq!(s.decisions.seen(), r.decisions.seen());
        let _ = (sn, rn);
    }
}

/// A hub cuts a `decisions.list` page short (`more`): the TUI reads the next one, older than
/// the oldest it had, until the table is full or nothing older is left, and ends with the rows
/// a replay keeps. A decision a view kept across a switch had selected, older than the
/// snapshot's rows, is selected again once a page brings it; one the user moved off is not.
#[test]
fn older_decisions_come_page_by_page_and_find_a_kept_selection() {
    let items = control_items(targeted_log(DECISIONS_CAP + 100));
    let page = |st: &mut AppState| {
        let Some((before, limit)) = st.older_decisions.take() else {
            return false;
        };
        // Pages of 300 at most, as a hub cuts one at its byte limit.
        let cut = limit.min(300);
        let mut answer = decisions_list(&items, before, cut);
        if cut < limit && answer["items"].as_array().is_some_and(|a| a.len() == cut) {
            answer["more"] = json!(true);
        }
        st.older_decisions_came(&answer);
        true
    };
    let (r, _) = replayed(&items, all_events());
    let rows = |st: &AppState| {
        st.decisions
            .newest_first(&DecisionFilter::default())
            .into_iter()
            .map(|r| r.id.clone())
            .collect::<Vec<_>>()
    };
    let (mut s, _) = install_into(AppState::new(), all_events(), &items);
    let mut pages = 0;
    while page(&mut s) {
        pages += 1;
    }
    assert!(pages > 1, "{pages} page(s)");
    assert_eq!(rows(&s), rows(&r));

    for moved in [false, true] {
        let (mut left, _) = replayed(&items, all_events());
        left.view.screen = Screen::Decisions;
        left.view.decision_sel = 900;
        let selected = left.decision_rows()[900].id.clone();
        let saved = left.take_view();
        let mut back = AppState::new();
        back.restore_view(saved);
        let (mut s, _) = install_into(back, all_events(), &items);
        assert!(
            s.pending_decision.is_some(),
            "older than the snapshot's rows"
        );
        if moved {
            s.view.decision_sel = 3;
        } else {
            // A live decision before its page comes moves the selection along: no move of
            // the user's.
            let mut hub = ThreadScript::new(HUB_THREAD);
            let ev = decision(&mut hub, 80_000);
            let g = head(&items) + 1;
            s.apply_event(&StreamItem {
                stream: twr_core::proto::StreamKind::Control,
                gseq: Some(g),
                event: ev,
            });
        }
        while page(&mut s) {}
        assert_eq!(s.pending_decision, None);
        let now = s.decision_rows()[s.decision_selected()].id.clone();
        match moved {
            false => assert_eq!(now, selected),
            true => assert_eq!(s.decision_selected(), 3),
        }
    }
}

/// A view kept across a switch to another workspace, with `!` commands running and its last
/// message sent: while away, more `!` commands end than the TUI remembers (some of the view's
/// among the first), the message's turn starts, raises a request and ends, or is still open,
/// or the message was not answered yet. Back with a snapshot, its commands and its message are
/// as after a replay of the log into it.
#[test]
fn a_view_kept_across_a_switch_comes_back_as_after_a_replay() {
    for case in ["ended", "open", "unanswered"] {
        let mut o = ThreadScript::new("o");
        let mut w = ThreadScript::new("w");
        let mut gone = ThreadScript::new("gone");
        let mut before = vec![orchestrator(&mut o), w.created("api"), gone.created("old")];
        before.push(o.started("s"));
        for r in 1..=5 {
            let mut ev = o.turn_started("go");
            set_ref(&mut ev, r);
            before.push(ev);
            before.push(o.raise(&format!("p{r}"), AttentionReason::Permission));
            before.push(o.resolve(&format!("p{r}")));
            before.push(o.turn_ended(TurnResult::Completed));
        }
        for j in ["J1", "J2", "J3"] {
            before.push(job(&mut w, j, true));
        }
        before.push(job(&mut gone, "J4", true));
        let items = control_items(before.clone());

        // The view as it was left.
        let (mut first, _) = replayed(&items, all_events());
        for (j, t) in [("J1", "w"), ("J2", "w"), ("J3", "w"), ("J4", "gone")] {
            first.apply_result("shell.run", &json!({"job": j, "thread": t, "name": "ls"}));
        }
        assert_eq!(first.shell_jobs.len(), 4);
        first.sent = Some(Sent {
            thread: Some("o".into()),
            text: "hi".into(),
            msg_ref: (case != "unanswered").then_some(7),
            waiting: u32::from(case == "unanswered"),
            ..Sent::default()
        });
        let saved = first.take_view();

        // Away.
        let mut away = before;
        away.push(job(&mut w, "J1", false));
        for j in 100..140 {
            away.push(job(&mut w, &format!("J{j}"), true));
            away.push(job(&mut w, &format!("J{j}"), false));
        }
        away.push(job(&mut w, "J2", false));
        let mut ev = o.turn_started("hi");
        set_ref(&mut ev, 7);
        away.push(ev);
        away.push(o.raise("p7", AttentionReason::Permission));
        away.push(o.resolve("p7"));
        if case != "open" {
            away.push(o.turn_ended(TurnResult::Completed));
        }
        away.push(gone.event(EventBody::ThreadDeleted(ThreadDeleted {
            name: "old".into(),
            ..Default::default()
        })));
        let items = control_items(away);

        let mut back = AppState::new();
        back.restore_view(saved.clone());
        let (mut r, rn) = replay_into(back, all_events(), &items);
        let mut back = AppState::new();
        back.restore_view(saved);
        let (mut s, sn) = install_into(back, all_events(), &items);
        if case == "unanswered" {
            for st in [&mut r, &mut s] {
                st.apply_result("say", &json!({"ref": 7, "thread": "o"}));
            }
        }
        same(case, (&r, &rn), (&s, &sn));
        assert_eq!(s.sent, r.sent, "{case}: the last message");
        let sent = s.sent.as_ref().unwrap();
        // (An answer that comes after its turn ended finds the turn, not its end.)
        assert_eq!(
            (sent.turn, sent.acted, sent.ended),
            (Some(6), true, case == "ended"),
            "{case}"
        );
        assert_eq!(s.started_refs, r.started_refs, "{case}");
        assert_eq!(s.acted_turns, r.acted_turns, "{case}");
        assert_eq!(s.shell_done, r.shell_done, "{case}");
        assert!(
            !s.shell_done.contains(&"J1".to_string()),
            "J1 ended among the first"
        );
        // A command of a thread gone ends with it: no end of it may ever come.
        let jobs = |st: &AppState| {
            st.shell_jobs
                .iter()
                .filter(|j| j.thread != "gone")
                .map(|j| j.job.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(jobs(&s), jobs(&r), "{case}: the commands still to stop");
        assert_eq!(jobs(&s), ["J3"], "{case}");
        assert!(s.shell_jobs.iter().all(|j| j.thread != "gone"), "{case}");
    }
}

/// By hand, on a real hub's control stream (`TWR_BENCH_CONTROL`): the snapshot of the whole
/// stream and of its first nine tenths, the rest then live.
#[test]
#[ignore = "by hand: TWR_BENCH_CONTROL names a file of a real hub's control stream lines"]
fn snapshot_real_log_installs_what_the_replay_leaves() {
    let path = std::env::var_os("TWR_BENCH_CONTROL").expect("TWR_BENCH_CONTROL");
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let items = crate::bench::decode(&lines);
    let t = Instant::now();
    let alerts = check("the real log", &items, items.len() * 9 / 10);
    check("the whole real log", &items, items.len());
    let f = fold(&items);
    eprintln!(
        "{} control items ({alerts} alerts live); snapshot {:.1} KB; compared in {:.1} s",
        items.len(),
        serde_json::to_vec(&f).unwrap().len() as f64 / 1e3,
        t.elapsed().as_secs_f64()
    );
}

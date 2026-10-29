//! The stall timer: a thread with an open turn or background tasks of its runtime (a job has
//! its own timeout, and its records are no activity) that is not waiting on the user (no open
//! permission / question) and has had no event for `stall_after` gets one hub-raised
//! `attention.raised{reason: stalled, summary: "No activity for N min"}` for that turn. Not
//! while one of its jobs runs or waits for its slots (its own, not a `!` command of the
//! user's; one this hub runs, [`Core::runs_jobs_of`]: jobs never outlive their hub): its turn
//! or its runtime's watcher waits on the job, legitimately silent; once the job ends, the
//! silence counts again.
//!
//! A hung turn: a turn open and quiet while the oldest prompt queued for the thread waits (a
//! job's end, a message) for `hung_after` (`min(stall_after, 5 min)`): the turn should have
//! ended and did not, or what it waits on is stuck. How quiet is judged by what it has in
//! flight (`hub::work`, [`hung_wait`]), counted from its runtime's last progress (not a
//! heartbeat, nor a record of the hub's such as a git snapshot): `hung_after` when nothing of
//! it is in flight that twr sees (a runtime that wrote its reply and never closed the turn),
//! when only its tool calls are open with no background work of its own running (a waiter
//! that outlived what it waited for), or when twr cannot see its tools; `stall_after`, and at
//! least twice `hung_after`, while background work of its own runs (a task its runtime started
//! in the background, a job), which reviews and tests keep quiet for long. Such a turn gets no
//! plain stalled item meanwhile; it gets a distinct one, a hung-turn card ([`HUNG_KEY`]:
//! `{turn, waiting, state, key, work}`: the reading, the incident and what it had in flight
//! then), replacing an open plain one in the same batch (closed `by: timeout` with
//! `ext.twr.superseded_by` the card's id, [`SUPERSEDED_BY_KEY`]): `"<thread>'s turn N
//! seems hung: ..."`, saying what waits, what was in flight and what that suggests, with the
//! options [`HUNG_INTERRUPT`] (the turn is interrupted, as
//! `message.send_now` does, and the prompt that waits runs next) and [`HUNG_WAIT`] (the card
//! closes, and no other hung-turn card is raised for that turn; plain stalls go on). Only the
//! user answers it. It notifies as `hung` (on by default, unlike `stalled`), and the
//! orchestrator gets it as an urgent notice line (`reason=hung reading=<state> raised_at=...`),
//! and is told when it closes (`hub::updates`). A turn gets one card per incident
//! ([`incident_key`]: the reading and the calls and work it stood on, or the runtime's last
//! progress when nothing was in flight): however a card closed, the same incident raises no
//! other for that turn, so a call that stays open behind a card that closed is not raised
//! again every few minutes.
//!
//! A plain stall's summary says what its thread has in flight (`; in flight: ...`, `; nothing
//! in flight that twr can see`). The item resolves `by: timeout` as soon as another progress
//! event is appended to that thread (not a git snapshot), including the turn's end or the
//! runtime's closing sequence: the resolution is part
//! of the batch that records the activity ([`Core::stall_resolutions`]), so a hub that dies in
//! between cannot leave it open. When that batch is the thread's own work going on (its
//! runtime said, did or finished something, [`goes_on`]) and nothing in it ends that work (a
//! stop, a crash, an interrupt, a park, a failed turn, [`ends_work`]), the resolution says so,
//! `ext.twr.went_on` ([`WENT_ON_KEY`]): only such a stall resolved by itself (a client folds
//! it away, and a summary's basis reads past it, [`Core::stall_went_on`]). Startup resolves
//! any stalled item whose turn is no longer active (logs written before that), without it.
//! While its runtime compacts its context (a `context.compacting` without its
//! `context.compacted`, [`ThreadView::compacting`]), which can be minutes of silence, a thread
//! gets neither a stalled item nor a hung-turn card, for at most
//! [`COMPACTING_HOLD_SECS`] from the compaction's start ([`compaction_holds`]); past that its
//! silence counts as any other.
//! Inventory heartbeats renew task liveness without resetting stalls or repeatedly resolving
//! and raising them. A thread has at most one open stalled item. The check runs on a hub tick
//! every `min(30 s, stall_after / 4)`; `stall_after_secs = 0` turns it off.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::{AttentionItem, ThreadView};
use twr_core::work::{ago, Sight, State, TaskKind, Work};

use super::orch::cut;
use super::{
    append_err, rpc, twr_ext, twr_ext_map, Batch, BatchEvent, Caller, Core, HubState, Inner,
    ResolvedBy,
};

/// The longest a turn may be silent while a prompt waits before it is hung.
pub const HUNG_AFTER_MAX: Duration = Duration::from_secs(5 * 60);

/// How long a turn may be silent while a prompt waits, for stall threshold `after`.
pub fn hung_after(after: Duration) -> Duration {
    after.min(HUNG_AFTER_MAX)
}

/// Whether `v`'s runtime is compacting its context at `now`, begun less than
/// [`COMPACTING_HOLD_SECS`] ago: its silence is no stall (see the module docs).
pub(super) fn compaction_holds(v: &ThreadView, now: DateTime<Utc>) -> bool {
    let hold = chrono::Duration::seconds(i64::try_from(COMPACTING_HOLD_SECS).unwrap_or(i64::MAX));
    v.compacting.is_some_and(|since| now - since < hold)
}

/// The hung-turn cards (see the module docs), folded from the log.
#[derive(PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct HungState {
    /// Open cards: attn_id -> (thread, turn).
    cards: HashMap<String, (String, u64)>,
    /// The turn of each thread whose card the user answered: no other card for it.
    answered: HashMap<String, u64>,
    /// The incidents each thread's open turn had a card for ([`Hung::key`]): no other card
    /// for the same one, however its card closed. Thread -> (turn, keys).
    #[serde(default)]
    raised: HashMap<String, (u64, Vec<String>)>,
    /// The incident of each open card: attn_id -> key. A card closed because nothing waited
    /// behind its turn any longer gives its incident back: a message queued later is raised.
    #[serde(default)]
    card_keys: HashMap<String, String>,
}

/// `ext.twr` key (`true`) of a hung-turn card's resolution when nothing waited behind its turn
/// any longer (`Core::check_stalls`).
pub(super) const NOTHING_WAITS_KEY: &str = "nothing_waits";

/// Most incidents of one turn [`HungState::raised`] keeps.
const RAISED_KEPT: usize = 16;

/// A turn the hung-turn check found hung ([`Core::hung_turn`]).
struct Hung {
    turn: u64,
    /// What waits behind it, in words.
    waiting: String,
    /// What it had in flight then.
    work: Work,
    /// The incident: its reading and what it stood on (the open calls, the work running, or
    /// the last progress when nothing was in flight). A card closed for it is not raised again.
    key: String,
}

/// The stalls of a thread whose resolution says its work went on ([`WENT_ON_KEY`]) that are
/// kept, the newest: more than a summary's basis reads past (`hub::summary`).
const WENT_ON_KEPT: usize = 64;

/// The plain stalls each thread went on from ([`WENT_ON_KEY`]; not a hung-turn card), folded
/// from the log: thread -> attn_ids, oldest first, at most [`WENT_ON_KEPT`].
#[derive(PartialEq, Debug, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct WentOn(HashMap<String, VecDeque<String>>);

/// Longest tick period.
const TICK_MAX: Duration = Duration::from_secs(30);

/// Shortest tick period.
const TICK_MIN: Duration = Duration::from_millis(10);

/// "No activity for N min" (seconds below a minute).
fn stall_summary(after: Duration) -> String {
    let secs = after.as_secs();
    if secs >= 60 {
        format!("No activity for {} min", secs / 60)
    } else {
        format!("No activity for {} s", secs.max(1))
    }
}

/// How long a turn whose runtime has `work` in flight must be quiet before it is hung, with
/// `hung_after` and `stall_after` the thresholds: `hung_after` when nothing is in flight that
/// twr sees, when only its calls are, or when twr cannot see; `stall_after`, and at least twice
/// `hung_after`, while background work of its own runs, which reviews and tests keep quiet for
/// long; never while it works, waits on the user or has no turn. A compaction past its hold
/// (`compaction_holds`) counts as nothing in flight.
fn hung_wait(
    work: &Work,
    hung_after: chrono::Duration,
    stall_after: chrono::Duration,
) -> Option<chrono::Duration> {
    match work.state {
        State::WaitingOnWork => Some(stall_after.max(hung_after * 2)),
        State::WaitingOnCall | State::NothingInFlight | State::Unknown | State::Compacting => {
            Some(hung_after)
        }
        State::Working | State::WaitingForYou | State::Idle | State::Background => None,
    }
}

/// The incident a hung-turn card is about ([`Hung::key`]): its reading, and the calls and work
/// it stood on (when it stood on none, the runtime's last progress: progress since and a new
/// silence is a new incident).
fn incident_key(work: &Work) -> String {
    let ids = |it: &mut dyn Iterator<Item = &str>| it.collect::<Vec<_>>().join(",");
    let calls = ids(&mut work.calls.iter().map(|c| c.id.as_str()));
    match work.state {
        // (A task is named with its start: an id reused for a new run is another task.)
        State::WaitingOnWork => format!(
            "waiting_on_work:{calls}|{}",
            work.running_work()
                .map(|t| match t.since {
                    Some(s) => format!("{}@{}", t.id, s.to_rfc3339()),
                    None => t.id.clone(),
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        State::WaitingOnCall => format!("waiting_on_call:{calls}"),
        s => format!(
            "{}:{}",
            s.as_str(),
            work.progress_at
                .map_or_else(|| "none".to_string(), |p| p.to_rfc3339())
        ),
    }
}

/// Longest evidence a hung-turn card's summary carries, in characters.
const EVIDENCE_MAX: usize = 380;

/// The summary of hung-turn card `h` of thread `name`: what waits, then what it had in flight
/// and what that suggests, with its uncertainty.
fn hung_summary(name: &str, h: &Hung) -> String {
    let w = &h.work;
    let silent = w.quiet_secs().map_or_else(|| "a while".into(), ago);
    let age = |t: DateTime<Utc>| ago((w.as_of - t).num_seconds());
    let detached = w
        .tasks
        .iter()
        .filter(|t| t.kind == TaskKind::Detached)
        .count();
    let leftovers = match detached {
        0 => String::new(),
        1 => " 1 task moved to the background after its call returned runs unwaited (it may               be a leftover)."
            .into(),
        n => format!(
            " {n} tasks moved to the background after their calls returned run unwaited (they              may be leftovers)."
        ),
    };
    let evidence = match w.state {
        State::WaitingOnWork => format!(
            "Its background work still runs ({}): reviews and tests can be this quiet, so check              that work before interrupting.",
            w.in_flight().join(", ")
        ),
        State::WaitingOnCall => {
            let calls = w
                .in_flight()
                .into_iter()
                .take(w.calls.len().max(1))
                .collect::<Vec<_>>()
                .join(", ");
            let ended = w.ended.as_ref().map_or(String::new(), |e| {
                format!(
                    "; the last, \"{}\", ended {} ago",
                    twr_core::activity::cut(&e.what, 60),
                    age(e.at)
                )
            });
            format!(
                "It waits on {calls} and no background work of its own runs{ended}: the call                  may be waiting on something that already ended or never comes.{leftovers}"
            )
        }
        State::Compacting => format!(
            "Its context compaction began {} ago and has not ended.{leftovers}",
            w.compacting_since.map_or_else(|| "a while".into(), age)
        ),
        State::Unknown if w.sight == Sight::Stale => {
            "Its box is not connected, so what twr shows of it may be behind.".into()
        }
        State::Unknown => {
            "twr cannot see this runtime's tool calls, so it cannot tell whether it works.".into()
        }
        _ => format!(
            "Nothing of it is in flight that twr can see (no tool call open, no task running):              the turn may have been left open, or its model may still be writing.{leftovers}"
        ),
    };
    format!(
        "{name}'s turn {} seems hung: no output from its runtime for {silent} while {} waits to \
         start the next turn. {} Interrupt it to run that now, or wait.",
        h.turn,
        h.waiting,
        twr_core::activity::cut(&evidence, EVIDENCE_MAX)
    )
}

/// What a plain stall's summary adds of what its thread has in flight (`; in flight: Bash
/// "Run the tests" 12m`, `; nothing in flight that twr can see`), nothing when twr cannot see
/// its runtime.
fn in_flight_clause(work: &Work) -> String {
    if matches!(work.sight, Sight::None | Sight::Stale) {
        return String::new();
    }
    let parts = work.in_flight();
    if parts.is_empty() {
        "; nothing in flight that twr can see".into()
    } else {
        format!("; in flight: {}", parts.join(", "))
    }
}

/// The tick period for `after`.
pub(super) fn tick_period(after: Duration) -> Duration {
    (after / 4).clamp(TICK_MIN, TICK_MAX)
}

impl Core {
    /// Raises a stalled item for every running, unblocked thread silent for `after` (a
    /// hung-turn card where a prompt waits behind its turn).
    pub(super) fn check_stalls(&mut self, hub: &Inner, after: Duration) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let (Ok(after_chrono), Ok(hung_chrono)) = (
            chrono::Duration::from_std(after),
            chrono::Duration::from_std(hung_after(after)),
        ) else {
            return Ok(());
        };
        let now = Utc::now();
        let mut hung: Vec<(String, Hung, Option<String>)> = Vec::new();
        let mut due: Vec<(String, Option<u64>, String)> = Vec::new();
        for v in self.registry.list() {
            if v.attached.is_some()
                || v.is_blocked()
                || v.last_activity.is_none()
                || compaction_holds(v, now)
            {
                continue;
            }
            if let Some(h) = self.hung_turn(v, now, hung_chrono, after_chrono)? {
                let plain = self
                    .stalled
                    .get(&v.id)
                    .filter(|a| !self.hung.cards.contains_key(*a))
                    .cloned();
                hung.push((v.id.clone(), h, plain));
                continue;
            }
            // A prompt waits behind its open turn: the hung-turn check decides, within
            // `hung_after` of its arrival.
            // (Not a turn being interrupted or taken back: no hung-turn card comes for it.)
            let behind = v
                .current_turn
                .is_some_and(|t| self.hung.answered.get(&v.id) != Some(&t))
                && !v.current_turn_interrupted
                && !v.current_turn_withdrawn
                && self.store.pending_count(&v.id)? > 0;
            if behind
                || self.stalled.contains_key(&v.id)
                || !v.last_activity.is_some_and(|t| now - t >= after_chrono)
                // A job has its own timeout: only the runtime's own work stalls.
                || !v.runtime_work()
                // Its turn or its runtime's watcher waits on a job of its own.
                || self.runs_jobs_of(&v.id)
            {
                continue;
            }
            let summary = if v.current_turn.is_some() {
                format!(
                    "{}{}",
                    stall_summary(after),
                    in_flight_clause(&self.work(v, now))
                )
            } else {
                format!("{} from its background tasks", stall_summary(after))
            };
            due.push((v.id.clone(), v.current_turn, summary));
        }
        // A hung-turn card whose turn nothing waits behind any longer (the message was taken
        // back, the queue dropped): closed, saying so; its turn's work is as it was.
        let mut stale: Vec<(String, String, u64)> = Vec::new();
        for (attn_id, (thread, turn)) in &self.hung.cards {
            let still = self
                .registry
                .get(thread)
                .is_some_and(|v| v.current_turn == Some(*turn));
            if still && self.store.pending_count(thread)? == 0 {
                stale.push((attn_id.clone(), thread.clone(), *turn));
            }
        }
        for (attn_id, thread, turn) in stale {
            let mut closed = resolution(&attn_id);
            if let EventBody::AttentionResolved(p) = &mut closed {
                p.ext = twr_ext_map(json!({ NOTHING_WAITS_KEY: true }));
            }
            self.append(hub, &thread, Some(turn), Source::Hub, closed)?;
        }
        // One independent item per thread: a failure part-way leaves the rest for the next
        // tick.
        for (thread, h, plain) in hung {
            let name = self.thread_name(&thread);
            let turn = h.turn;
            let summary = hung_summary(&name, &h);
            let card = new_id();
            let mut batch = Batch::default();
            if let Some(attn_id) = plain {
                // Closed for the card, not because activity resumed: it says so.
                let mut closed = resolution(&attn_id);
                if let EventBody::AttentionResolved(p) = &mut closed {
                    p.ext = twr_ext_map(json!({ SUPERSEDED_BY_KEY: card }));
                }
                batch.push(&thread, Some(turn), Source::Hub, closed);
            }
            batch.push(
                &thread,
                Some(turn),
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: card,
                    reason: AttentionReason::Stalled,
                    summary,
                    options: vec![HUNG_INTERRUPT.into(), HUNG_WAIT.into()],
                    ext: twr_ext_map(json!({ HUNG_KEY: {
                        "turn": turn,
                        "waiting": h.waiting,
                        "state": h.work.state,
                        "key": h.key,
                        "work": h.work,
                    } })),
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch)?;
        }
        for (thread, turn, summary) in due {
            self.append(
                hub,
                &thread,
                turn,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: new_id(),
                    reason: AttentionReason::Stalled,
                    summary,
                    options: vec!["dismiss".into()],
                    ..Default::default()
                }),
            )?;
        }
        Ok(())
    }

    /// The open turn of `v`, when it is hung at `now` (see the module docs): no hung-turn card
    /// open or answered for it, a prompt queued behind it for `hung_after`, and its runtime
    /// quiet for as long as what it has in flight allows ([`hung_wait`]); never twice for the
    /// same incident ([`Hung::key`]).
    fn hung_turn(
        &self,
        v: &ThreadView,
        now: DateTime<Utc>,
        hung_after: chrono::Duration,
        stall_after: chrono::Duration,
    ) -> anyhow::Result<Option<Hung>> {
        let Some(turn) = v.current_turn else {
            return Ok(None);
        };
        if v.current_turn_interrupted
            || v.current_turn_withdrawn
            || self.hung.answered.get(&v.id) == Some(&turn)
            || self
                .stalled
                .get(&v.id)
                .is_some_and(|a| self.hung.cards.contains_key(a))
            || self
                .quiet_since(v)
                .or(v.last_activity)
                .is_none_or(|t| now - t < hung_after)
        {
            return Ok(None);
        }
        let Some(waiting) = self.store.pending_list(&v.id, 1)?.into_iter().next() else {
            return Ok(None);
        };
        let queued_at = DateTime::parse_from_rfc3339(&waiting.queued_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);
        if now - queued_at < hung_after {
            return Ok(None);
        }
        let mut work = self.work(v, now);
        // What it has in flight, judged with this check's thresholds (not the minute after
        // which a client reads a turn as quiet).
        work.state = work.reading(v.is_blocked());
        let Some(wait) = hung_wait(&work, hung_after, stall_after) else {
            return Ok(None);
        };
        // Quiet that long: counted from its runtime's last progress (not from the hub's own
        // records, a git snapshot, a heartbeat).
        let quiet = work.quiet().unwrap_or(chrono::Duration::MAX);
        if quiet < wait {
            return Ok(None);
        }
        let key = incident_key(&work);
        if self
            .hung
            .raised
            .get(&v.id)
            .is_some_and(|(t, keys)| *t == turn && keys.contains(&key))
        {
            return Ok(None);
        }
        let what = match waiting.from {
            TurnFrom::User => "a message of yours".to_string(),
            TurnFrom::Orchestrator => "a message from the orchestrator".to_string(),
            TurnFrom::Runtime => {
                let first = waiting.prompt.lines().next().unwrap_or_default();
                match first.strip_prefix("[twr job ") {
                    Some(rest) => format!(
                        "the end of job {}",
                        cut(&crate::report::display_text(rest, false), 160)
                    ),
                    None => format!(
                        "a prompt of the hub's ({})",
                        cut(&crate::report::display_text(first, false), 160)
                    ),
                }
            }
        };
        Ok(Some(Hung {
            turn,
            waiting: what,
            work,
            key,
        }))
    }

    /// Bookkeeping of open stalled items (live and on replay).
    pub(super) fn track_stall(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::AttentionRaised(p)
                if p.reason == AttentionReason::Stalled
                    && ev.source == Source::Hub
                    && ev.thread != HUB_THREAD =>
            {
                self.stalled.insert(ev.thread.clone(), p.attn_id.clone());
            }
            EventBody::AttentionResolved(p) if self.stalled.get(&ev.thread) == Some(&p.attn_id) => {
                self.stalled.remove(&ev.thread);
                // (A hung-turn card is never folded away: not one of them.)
                if went_on(&p.ext) && !self.hung.cards.contains_key(&p.attn_id) {
                    let kept = self.went_on.0.entry(ev.thread.clone()).or_default();
                    kept.push_back(p.attn_id.clone());
                    while kept.len() > WENT_ON_KEPT {
                        kept.pop_front();
                    }
                }
            }
            EventBody::ThreadDeleted(_) => {
                self.went_on.0.remove(&ev.thread);
            }
            _ => {}
        }
        match &ev.body {
            EventBody::AttentionRaised(p)
                if p.reason == AttentionReason::Stalled
                    && ev.source == Source::Hub
                    && is_hung_card(&p.ext) =>
            {
                let turn = twr_ext(&p.ext)
                    .and_then(|t| t.get(HUNG_KEY))
                    .and_then(|h| h.get("turn"))
                    .and_then(Value::as_u64)
                    .or(ev.turn)
                    .unwrap_or_default();
                self.hung
                    .cards
                    .insert(p.attn_id.clone(), (ev.thread.clone(), turn));
                let key = twr_ext(&p.ext)
                    .and_then(|t| t.get(HUNG_KEY))
                    .and_then(|h| h.get("key"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let raised = self
                    .hung
                    .raised
                    .entry(ev.thread.clone())
                    .or_insert_with(|| (turn, Vec::new()));
                if raised.0 != turn {
                    *raised = (turn, Vec::new());
                }
                raised.1.push(key.clone());
                if raised.1.len() > RAISED_KEPT {
                    raised.1.remove(0);
                }
                self.hung.card_keys.insert(p.attn_id.clone(), key);
            }
            EventBody::AttentionResolved(p) => {
                let key = self.hung.card_keys.remove(&p.attn_id);
                if let Some((thread, turn)) = self.hung.cards.remove(&p.attn_id) {
                    if p.by == ResolvedBy::User {
                        self.hung.answered.insert(thread.clone(), turn);
                    }
                    let nothing_waits =
                        twr_ext(&p.ext).is_some_and(|t| t.contains_key(NOTHING_WAITS_KEY));
                    if let (true, Some(key), Some((t, keys))) =
                        (nothing_waits, key, self.hung.raised.get_mut(&thread))
                    {
                        if *t == turn {
                            keys.retain(|k| *k != key);
                        }
                    }
                }
            }
            EventBody::ThreadDeleted(_) => {
                self.hung.answered.remove(&ev.thread);
                self.hung.raised.remove(&ev.thread);
            }
            // A new turn has incidents of its own.
            EventBody::TurnStarted(_)
                if self
                    .hung
                    .raised
                    .get(&ev.thread)
                    .is_some_and(|(t, _)| ev.turn != Some(*t)) =>
            {
                self.hung.raised.remove(&ev.thread);
            }
            _ => {}
        }
    }

    /// Whether `attn_id` is a plain stall of `thread` that resolved because the thread went on
    /// working ([`WENT_ON_KEY`]; one of the newest [`WENT_ON_KEPT`]).
    pub(super) fn stall_went_on(&self, thread: &str, attn_id: &str) -> bool {
        self.went_on
            .0
            .get(thread)
            .is_some_and(|k| k.iter().any(|a| a == attn_id))
    }

    /// Whether `attn_id` is an open hung-turn card.
    pub(super) fn hung_card(&self, attn_id: &str) -> bool {
        self.hung.cards.contains_key(attn_id)
    }

    /// Answers hung-turn card `item` (see the module docs) with `decision` (`None`: a
    /// dismissal, as [`HUNG_WAIT`]). Users only.
    pub(super) fn answer_hung_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{} is a hung-turn card: only the user answers it; tell the user",
                    item.attn_id
                ),
            ));
        }
        let Some((thread, turn)) = self.hung.cards.get(&item.attn_id).cloned() else {
            return Err(rpc(codes::NOT_FOUND, "no such hung-turn card"));
        };
        let decision = decision.unwrap_or(HUNG_WAIT).trim().to_ascii_lowercase();
        let interrupt = match decision.as_str() {
            HUNG_INTERRUPT => true,
            HUNG_WAIT | "dismiss" => false,
            other => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "{other:?} is no option of {}: {HUNG_INTERRUPT} or {HUNG_WAIT}",
                        item.attn_id
                    ),
                ))
            }
        };
        let view = self.view(&thread)?.clone();
        let head = self
            .store
            .pending_list(&thread, 1)
            .map_err(super::internal)?
            .into_iter()
            .next();
        // Still the turn the card is about, with something waiting: it is interrupted.
        let interrupting = interrupt && view.current_turn == Some(turn) && head.is_some();
        if interrupting
            && !view.current_turn_interrupted
            && !self.turn_interruptible(hub, &thread, &view)
        {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{}'s runtime cannot interrupt a turn: `twr stop {}` stops it",
                    view.name, view.name
                ),
            ));
        }
        // The interruption is recorded with the answer (one batch): `turn.interrupted` first
        // makes the turn end `interrupted` whatever its runtime does, as `message.send_now`.
        let already = view.current_turn_interrupted || view.current_turn_withdrawn;
        let mut batch = Batch::default();
        let answered = batch.push(
            &thread,
            view.current_turn,
            caller.source(),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: item.attn_id.clone(),
                by: ResolvedBy::User,
                answer: Some(twr_core::adapter::Answer::new(if interrupt {
                    HUNG_INTERRUPT
                } else {
                    HUNG_WAIT
                })),
                ..Default::default()
            }),
        );
        let head = head.filter(|_| interrupting);
        if let (Some(head), false) = (&head, already) {
            batch.push(
                &thread,
                Some(turn),
                caller.source(),
                EventBody::TurnInterrupted(TurnInterrupted {
                    msg_ref: head.id,
                    turn,
                    ..Default::default()
                }),
            );
        }
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        let ev = done
            .events
            .get(answered)
            .cloned()
            .ok_or_else(|| super::internal("the answer was not recorded"))?;
        if let Some(head) = head {
            self.release_for(&thread, head.id)?;
            if !already {
                if let Some(live) = self.rt.get(&thread).and_then(|r| r.live.as_ref()) {
                    if live.handle.interrupt().is_err() {
                        // The runtime is exiting: its closing sequence ends the turn.
                        tracing::info!(thread = %thread, turn, "the runtime was gone before the interrupt");
                    }
                }
            }
        }
        Ok(ev)
    }

    /// The resolutions (`by: timeout`) a batch of `events` implies: activity on a thread with
    /// an open stalled item resolves it, after the batch's own events. The item's own events do
    /// not count, and a batch that resolves the item itself needs none. A resolution says the
    /// thread went on working ([`WENT_ON_KEY`]) when the batch has its own work going on and
    /// nothing that ends it ([`went_on_in`]).
    pub(super) fn stall_resolutions(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let mut out: Vec<BatchEvent> = Vec::new();
        for ev in events {
            if is_job_task(&ev.source, &ev.body) {
                continue;
            }
            if matches!(
                ev.body,
                EventBody::ConversationUpdate(_)
                    | EventBody::WorktreeDisk(_)
                    | EventBody::WorktreeTrimmed(_)
                    | EventBody::GitSnapshot(_)
                    | EventBody::TaskUpdated(TaskUpdated {
                        heartbeat: true,
                        ..
                    })
            ) {
                continue;
            }
            let Some(attn_id) = self.stalled.get(&ev.thread) else {
                continue;
            };
            if out.iter().any(|r| r.thread == ev.thread) || is_own(&ev.body, attn_id) {
                continue;
            }
            // A hung-turn card closes only on what moves the turn's work (its runtime's own
            // events, a turn's start or end, what ends the work): a rename or another record of
            // the hub's leaves the hang as it was, and its incident raises no other card.
            if self.hung.cards.contains_key(attn_id) {
                // (Late output of a turn that is no longer open is not this turn's work, as
                // `hub::activity` reads it.)
                let open = self.registry.get(&ev.thread).and_then(|v| v.current_turn);
                let late = ev.body.is_agent() && ev.turn.is_some() && ev.turn != open;
                if late || !moves_work(ev) {
                    continue;
                }
            }
            let resolved_here = events.iter().any(|e| {
                e.thread == ev.thread
                    && matches!(&e.body, EventBody::AttentionResolved(p) if p.attn_id == *attn_id)
            });
            if resolved_here {
                continue;
            }
            let view = self.registry.get(&ev.thread);
            let open = view.and_then(|v| v.current_turn);
            // The turn was being interrupted or taken back: its end is no work going on.
            let stopping =
                view.is_some_and(|v| v.current_turn_interrupted || v.current_turn_withdrawn);
            let mut body = resolution(attn_id);
            if went_on_in(events, &ev.thread, stopping) {
                if let EventBody::AttentionResolved(p) = &mut body {
                    p.ext = twr_ext_map(json!({ WENT_ON_KEY: true }));
                }
            }
            out.push(BatchEvent {
                thread: ev.thread.clone(),
                turn: ev.turn.or(open),
                source: Source::Hub,
                body,
                raw: None,
            });
        }
        out
    }

    /// Startup: resolves the stalled items of threads with no remaining work (a log from
    /// before the resolution was part of the activity's batch). Each is its own append.
    pub(super) fn repair_stalls(&mut self, hub: &Inner) -> anyhow::Result<()> {
        let stale: Vec<(String, String)> = self
            .stalled
            .iter()
            .filter(|(t, _)| {
                self.registry
                    .get(t.as_str())
                    .is_none_or(|v| !v.runtime_work())
            })
            .map(|(t, a)| (t.clone(), a.clone()))
            .collect();
        for (thread, attn_id) in stale {
            let turn = self
                .registry
                .get(&thread)
                .and_then(|v| v.last_turn.as_ref())
                .map(|t| t.turn);
            self.append(hub, &thread, turn, Source::Hub, resolution(&attn_id))?;
        }
        Ok(())
    }
}

/// Whether `events` (a batch) have `thread`'s own work going on ([`goes_on`]) and nothing that
/// ends it ([`ends_work`]); `stopping`: its open turn was being interrupted or taken back.
fn went_on_in(events: &[BatchEvent], thread: &str, stopping: bool) -> bool {
    let mine = || events.iter().filter(|e| e.thread == thread);
    !mine().any(|e| ends_work(&e.body)) && mine().any(|e| goes_on(e, stopping))
}

/// Whether `e` is the thread's own work going on: its runtime said, thought, did or finished
/// something, asked the user (a permission, a question, a review), started a turn of its own,
/// or one of its background tasks moved (not a heartbeat; a job's task is the hub's). Not a
/// turn's end once it was being interrupted or taken back (`stopping`).
fn goes_on(e: &BatchEvent, stopping: bool) -> bool {
    if !matches!(e.source, Source::Adapter(_)) {
        return false;
    }
    match &e.body {
        EventBody::AgentText(_)
        | EventBody::AgentToolCall(_)
        | EventBody::AgentToolResult(_)
        | EventBody::AgentThinking(_)
        | EventBody::ContextCompacting(_)
        | EventBody::ContextCompacted(_)
        | EventBody::TurnStarted(_) => true,
        EventBody::TaskUpdated(p) => !p.heartbeat,
        EventBody::TurnEnded(p) => p.result == TurnResult::Completed && !stopping,
        EventBody::AttentionRaised(p) => p.reason != AttentionReason::Error,
        _ => false,
    }
}

/// Whether `body` ends a thread's work rather than going on with it: its runtime stopped,
/// crashed, was parked or taken over (attached), its turn was interrupted or taken back, ended
/// otherwise than completed, or failed (an `error` item).
fn ends_work(body: &EventBody) -> bool {
    match body {
        EventBody::ThreadStopped(_)
        | EventBody::ThreadCrashed(_)
        | EventBody::ThreadParked(_)
        | EventBody::ThreadAttached(_)
        | EventBody::TurnInterrupted(_)
        | EventBody::TurnWithdrawn(_) => true,
        EventBody::TurnEnded(p) => p.result != TurnResult::Completed,
        EventBody::AttentionRaised(p) => p.reason == AttentionReason::Error,
        _ => false,
    }
}

/// Whether `e` moves its thread's work, for a hung-turn card ([`Core::stall_resolutions`]): an
/// event of its runtime's, a turn's start or end, or what ends the work ([`ends_work`]).
fn moves_work(e: &BatchEvent) -> bool {
    matches!(e.source, Source::Adapter(_))
        || matches!(e.body, EventBody::TurnStarted(_) | EventBody::TurnEnded(_))
        || ends_work(&e.body)
}

/// Whether `body` is one of the stalled item `attn_id`'s own events.
fn is_own(body: &EventBody, attn_id: &str) -> bool {
    match body {
        EventBody::AttentionRaised(p) => p.attn_id == attn_id,
        EventBody::AttentionAnnotated(p) => p.attn_id == attn_id,
        EventBody::AttentionResolved(p) => p.attn_id == attn_id,
        _ => false,
    }
}

fn resolution(attn_id: &str) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.to_string(),
        by: ResolvedBy::Timeout,
        answer: None,
        ..Default::default()
    })
}

/// The stall tick task: checks every [`tick_period`] until the hub stops or is dropped.
pub(super) async fn stall_ticker(hub: Weak<Inner>, after: Duration) {
    let mut tick = tokio::time::interval(tick_period(after));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() == HubState::Stopped {
            return;
        }
        let r = h.core().check_stalls(&h, after);
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "stall check failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{Hub, HubOptions};
    use twr_core::config::Config;

    /// 22:00:00 UTC of the incident's day, plus `secs`.
    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-28T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    /// The incident (unified-left-sidebar, turn 15, 2026-10-28) replayed as its log would be
    /// recorded now: its calls, tasks and times as they were (ids shortened; its foreground
    /// tasks carry `ext.twr.foreground_call`, a waiter moved to the background by Claude's
    /// 600 s Bash timeout `ext.twr.detached`). The old check (silence and a waiting message
    /// alone) raised a hung-turn card at each of the three times; this one raises one, at the
    /// second: the waiter that outlived the tests it waited for, with that said.
    #[tokio::test]
    async fn the_incident_raises_one_card_for_the_waiter_that_outlived_its_tests() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        let adapter = || Source::Adapter("claude-code".into());
        let mut evs: Vec<(i64, Option<u64>, Source, EventBody)> = vec![(
            -1920,
            None,
            Source::Orchestrator,
            EventBody::ThreadCreated(ThreadCreated {
                name: "uls".into(),
                box_name: "local".into(),
                runtime: "claude-code".into(),
                ..Default::default()
            }),
        )];
        evs.push((
            -1920,
            Some(15),
            Source::Orchestrator,
            EventBody::TurnStarted(TurnStarted::default()),
        ));
        let call = |secs: i64, id: &str, purpose: &str| {
            let mut ext = None;
            set_twr_ext(&mut ext, PURPOSE_KEY, json!(purpose));
            (
                secs,
                Some(15),
                adapter(),
                EventBody::AgentToolCall(AgentToolCall {
                    call_id: id.into(),
                    tool: "Bash".into(),
                    ext,
                    ..Default::default()
                }),
            )
        };
        let result = |secs: i64, id: &str| {
            (
                secs,
                Some(15),
                adapter(),
                EventBody::AgentToolResult(AgentToolResult {
                    call_id: id.into(),
                    ok: true,
                    ..Default::default()
                }),
            )
        };
        let task =
            |secs: i64, id: &str, what: &str, running: bool, placed: Option<(&str, Value)>| {
                let mut ext = None;
                if let Some((k, v)) = placed {
                    set_twr_ext(&mut ext, k, v);
                }
                (
                    secs,
                    None,
                    adapter(),
                    EventBody::TaskUpdated(TaskUpdated {
                        task_id: id.into(),
                        running,
                        description: what.into(),
                        ext,
                        ..Default::default()
                    }),
                )
            };
        let fg = |c: &str| Some((FOREGROUND_CALL_KEY, json!(c)));
        let detached = || Some((DETACHED_KEY, json!(true)));
        // 21:56: tests in the background, and a waiter on them (`pgrep -f` matching itself).
        evs.push(call(-212, "c-rvl", "Wait for TUI tests"));
        evs.push(task(-208, "bbk", "Wait for TUI tests", true, fg("c-rvl")));
        evs.push(task(390, "bbk", "", true, detached()));
        evs.push(result(390, "c-rvl"));
        // 22:07: review round 3 in the background, a waiter on its answer.
        evs.push(call(463, "c-p4l", "Re-review at xhigh (round 3)"));
        evs.push(task(464, "bu3", "Re-review at xhigh (round 3)", true, None));
        evs.push(result(464, "c-p4l"));
        evs.push(call(466, "c-gxy", "Wait for round 3 answer"));
        evs.push(task(
            470,
            "bsv",
            "Wait for round 3 answer",
            true,
            fg("c-gxy"),
        ));
        evs.push((
            473,
            None,
            Source::Hub,
            EventBody::GitSnapshot(Default::default()),
        ));
        let checks: [(i64, &str, bool); 3] = [
            // 22:12:53: the review runs: waiting on work, no card yet.
            (773, "waiting_on_work", false),
            // 22:23:53: the tests ended at 22:18:25; their waiter is still open.
            (1433, "waiting_on_call", true),
            // 22:31:53: review round 4 runs.
            (1913, "waiting_on_work", false),
        ];
        let mut rest = vec![
            task(829, "bu3", "", false, None),
            task(847, "bsv", "", false, fg("c-gxy")),
            result(848, "c-gxy"),
            call(934, "c-cz2", "Apply test edit; run TUI tests"),
            task(935, "b9p", "Apply test edit; run TUI tests", true, None),
            result(936, "c-cz2"),
            call(939, "c-mzy", "Wait for TUI tests"),
            task(943, "b1y", "Wait for TUI tests", true, fg("c-mzy")),
            task(1105, "b9p", "", false, None),
        ];
        let mut later = vec![
            task(1541, "b1y", "", true, detached()),
            result(1541, "c-mzy"),
            call(1553, "c-mmv", "Clippy, commit, re-review round 4"),
            task(1554, "b8g", "Clippy, commit, re-review round 4", true, None),
            result(1554, "c-mmv"),
            call(1558, "c-to5", "Wait for round 4 answer"),
            task(1562, "bjj", "Wait for round 4 answer", true, fg("c-to5")),
            (
                1613,
                None,
                Source::Hub,
                EventBody::GitSnapshot(Default::default()),
            ),
        ];
        let mut seq = 0;
        let mut fold = |hub: &Hub, evs: &mut Vec<(i64, Option<u64>, Source, EventBody)>| {
            let batch: Vec<(Event, Option<u64>)> = evs
                .drain(..)
                .map(|(secs, turn, source, body)| {
                    seq += 1;
                    let mut e = Event::new("uls", seq, turn, source, body);
                    e.ts = at(secs);
                    (e, None)
                })
                .collect();
            hub.inner.core().fold_events(&batch);
        };
        let (hung_after, stall_after) = (
            chrono::Duration::seconds(300),
            chrono::Duration::seconds(600),
        );
        fold(&hub, &mut evs);
        for (i, (secs, state, card)) in checks.into_iter().enumerate() {
            match i {
                1 => fold(&hub, &mut rest),
                2 => fold(&hub, &mut later),
                _ => {}
            }
            let core = hub.inner.core();
            let v = core.registry.get("uls").unwrap().clone();
            let now = at(secs);
            // The old check: silent for `hung_after` since the last event (a git snapshot
            // counted), a message waiting: a card every time.
            assert!(now - v.last_activity.unwrap() >= hung_after, "{secs}");
            let w = core.work(&v, now);
            assert_eq!(w.state.as_str(), state, "{secs}: {w:#?}");
            let wait = hung_wait(&w, hung_after, stall_after).unwrap();
            let quiet = chrono::Duration::seconds(w.quiet_secs().unwrap());
            assert_eq!(quiet >= wait, card, "{secs}: quiet {quiet} wait {wait}");
            if card {
                assert_eq!(incident_key(&w), "waiting_on_call:c-mzy");
                let summary = hung_summary(
                    "uls",
                    &Hung {
                        turn: 15,
                        waiting: "a message from the orchestrator".into(),
                        key: incident_key(&w),
                        work: w,
                    },
                );
                assert_eq!(
                    summary,
                    "uls's turn 15 seems hung: no output from its runtime for 5m while a \
                     message from the orchestrator waits to start the next turn. It waits on \
                     Bash \"Wait for TUI tests\" 8m and no background work of its own runs; \
                     the last, \"Apply test edit; run TUI tests\", ended 5m ago: the call may \
                     be waiting on something that already ended or never comes. 1 task moved \
                     to the background after its call returned runs unwaited (it may be a \
                     leftover). Interrupt it to run that now, or wait."
                );
            }
        }
        // Round 4 ran until after 22:36: quiet 597 s at 22:35:59, still no card.
        let core = hub.inner.core();
        let v = core.registry.get("uls").unwrap().clone();
        let w = core.work(&v, at(2159));
        assert_eq!(w.state, State::WaitingOnWork);
        assert!(
            chrono::Duration::seconds(w.quiet_secs().unwrap())
                < hung_wait(&w, hung_after, stall_after).unwrap()
        );
        assert_eq!(
            w.line(),
            "quiet 9m · Bash \"Wait for round 4 answer\" 10m · background: Clippy, commit, \
             re-review round 4 10m · 2 detached"
        );
    }

    /// A task's id reused for a new run is another incident; the same run is the same one.
    #[test]
    fn an_incident_names_each_run_of_its_tasks() {
        let mut w = Work::new(at(600));
        w.turn = Some(1);
        w.state = State::WaitingOnWork;
        let run = |since: i64| twr_core::work::Task {
            id: "review".into(),
            what: "Re-review".into(),
            kind: TaskKind::Background,
            since: Some(at(since)),
            awaited: false,
            progress_at: None,
            alive_at: at(since),
        };
        w.tasks = vec![run(0)];
        let first = incident_key(&w);
        assert_eq!(incident_key(&w), first);
        w.tasks = vec![run(300)];
        assert_ne!(incident_key(&w), first);
    }

    /// A hung-turn card whose turn nothing waits behind any longer (the message was taken
    /// back) is closed on the next check, saying so, and its incident is given back: a message
    /// queued later raises a card again.
    #[tokio::test]
    async fn a_hung_card_nothing_waits_behind_is_closed() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        for (id, name, role) in [
            ("orch", "orchestrator", json!({"role": "orchestrator"})),
            ("api", "api", json!({})),
        ] {
            hub.append(
                id,
                None,
                Source::Hub,
                EventBody::ThreadCreated(ThreadCreated {
                    name: name.into(),
                    box_name: "local".into(),
                    runtime: "fake".into(),
                    ext: twr_ext_map(role),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        hub.append(
            "api",
            Some(1),
            Source::User("test".into()),
            EventBody::TurnStarted(TurnStarted::default()),
            None,
        )
        .unwrap();
        hub.append(
            "api",
            Some(1),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "h1".into(),
                reason: AttentionReason::Stalled,
                summary: "api's turn 1 seems hung: …".into(),
                options: vec![HUNG_INTERRUPT.into(), HUNG_WAIT.into()],
                ext: twr_ext_map(json!({ HUNG_KEY: {"turn": 1, "waiting": "a message",
                                                    "key": "waiting_on_call:c1"} })),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            hub.inner.core().hung.raised.get("api"),
            Some(&(1, vec!["waiting_on_call:c1".to_string()]))
        );
        {
            let mut core = hub.inner.core();
            core.check_stalls(&hub.inner, Duration::from_secs(600))
                .unwrap();
            assert!(!core.hung_card("h1"), "nothing waits: closed");
            assert_eq!(core.hung.raised.get("api"), Some(&(1, Vec::new())));
        }
        let (_, evs) = hub.thread_events("orch", 0, 1000).unwrap();
        assert!(
            evs.iter()
                .any(|e| matches!(&e.body, EventBody::ConversationUpdate(p)
                if p.text == "api's seems-hung card h1 closed: nothing waits behind its turn \
                              any longer; it is no longer a current alarm.")),
            "{evs:#?}"
        );
    }

    /// A hung-turn card that closes because its thread went on tells the orchestrator so, in
    /// the batch that closes it: the alarm is over.
    #[tokio::test]
    async fn a_hung_card_that_closes_tells_the_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        for (id, name, role) in [
            ("orch", "orchestrator", json!({"role": "orchestrator"})),
            ("api", "api", json!({})),
        ] {
            hub.append(
                id,
                None,
                Source::Hub,
                EventBody::ThreadCreated(ThreadCreated {
                    name: name.into(),
                    box_name: "local".into(),
                    runtime: "fake".into(),
                    ext: twr_ext_map(role),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        hub.append(
            "api",
            Some(1),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "h1".into(),
                reason: AttentionReason::Stalled,
                summary: "api's turn 1 seems hung: …".into(),
                options: vec![HUNG_INTERRUPT.into(), HUNG_WAIT.into()],
                ext: twr_ext_map(json!({ HUNG_KEY: {"turn": 1, "waiting": "a message"} })),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        // A rename is no work: the card stays open, and nothing is said.
        hub.append(
            "api",
            None,
            Source::User("test".into()),
            EventBody::ThreadRenamed(ThreadRenamed {
                name: "api2".into(),
                previous: "api".into(),
                by: ChangedBy::User,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert!(hub.inner.core().hung_card("h1"), "the rename closed it");
        // Nor does late output of a turn no longer open.
        hub.append(
            "api",
            Some(7),
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: "late".into(),
                is_final: true,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert!(hub.inner.core().hung_card("h1"), "late output closed it");
        hub.append(
            "api",
            None,
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: "Back to it.".into(),
                is_final: true,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let (_, evs) = hub.thread_events("orch", 0, 1000).unwrap();
        let said: Vec<String> = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ConversationUpdate(p) => Some(p.text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            said.contains(
                &"api2's seems-hung card h1 closed: its runtime went on; it is no longer a \
                  current alarm."
                    .to_string()
            ),
            "{said:#?}"
        );
    }

    #[test]
    fn summaries_and_ticks() {
        assert_eq!(
            stall_summary(Duration::from_secs(600)),
            "No activity for 10 min"
        );
        assert_eq!(
            stall_summary(Duration::from_millis(300)),
            "No activity for 1 s"
        );
        assert_eq!(tick_period(Duration::from_secs(600)), TICK_MAX);
        assert_eq!(
            tick_period(Duration::from_millis(400)),
            Duration::from_millis(100)
        );
        assert_eq!(tick_period(Duration::from_millis(1)), TICK_MIN);
    }

    /// A compaction holds the checks from its start for at most `COMPACTING_HOLD_SECS`; its
    /// end, or the end of its turn, lets them go at once.
    #[test]
    fn a_compaction_holds_the_checks_for_a_while() {
        let t0 = Utc::now();
        let mut v = ThreadView::new("t");
        let at = |n: i64| t0 + chrono::Duration::seconds(n);
        let ev = |seq: u64, ts: DateTime<Utc>, body: EventBody| {
            let mut e = Event::new("t", seq, Some(1), Source::Adapter("fake".into()), body);
            e.ts = ts;
            e
        };
        v.apply(&ev(
            1,
            at(0),
            EventBody::TurnStarted(TurnStarted::default()),
        ));
        assert!(!compaction_holds(&v, at(1)));
        v.apply(&ev(
            2,
            at(10),
            EventBody::ContextCompacting(ContextCompacting::default()),
        ));
        let hold = i64::try_from(COMPACTING_HOLD_SECS).unwrap();
        assert!(compaction_holds(&v, at(10 + 600)));
        assert!(compaction_holds(&v, at(10 + hold - 1)));
        assert!(!compaction_holds(&v, at(10 + hold)));
        v.apply(&ev(
            3,
            at(20),
            EventBody::ContextCompacted(ContextCompacted::default()),
        ));
        assert!(!compaction_holds(&v, at(21)));
        v.apply(&ev(
            4,
            at(30),
            EventBody::ContextCompacting(ContextCompacting::default()),
        ));
        v.apply(&ev(5, at(31), EventBody::TurnEnded(TurnEnded::default())));
        assert!(!compaction_holds(&v, at(32)));
    }
}

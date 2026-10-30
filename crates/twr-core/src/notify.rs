//! What deserves a user notification, decided from the control stream alone.
//!
//! A [`Policy`] watches committed control events (after they are folded into the
//! [`Registry`] and [`AttentionStack`]) and turns the few that need the user into
//! [`Candidate`]s: a new permission, question or confirm card, a failure the hub left for the
//! user, a review, a stall, a worker's finished turn, and the orchestrator's reply to the
//! user. Everything else (lifecycle, agent output, decisions, annotations, resolutions,
//! re-raises of an open item, the hub's background notice turns) stays quiet.
//!
//! A candidate is not delivered at once: the caller holds it for [`Candidate::hold`] and asks
//! [`still_due`] before delivering, so an item answered, withdrawn or auto-retried meanwhile
//! (a transient crash raises an `error` item that the retry resolves) never notifies. A newer
//! candidate can cover a held one ([`Candidate::covers`]): a review raised by a turn's
//! rewritten report replaces its "finished". Any other rewrite of a report comes as a
//! revision ([`Candidate::revision`]): a held report then reads as its latest, and one already
//! delivered stays delivered. Several due at once become one message ([`digest`]).
//!
//! The policy knows nothing about delivery (a terminal bell, a desktop or a phone): the TUI
//! uses it for terminal alerts, and the hub for its notification command (`[notify]`); both
//! hold candidates in a [`Held`] and keep [`GAP`] between two notifications.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::event::{
    is_confirm_card, is_delete_card, is_hung_card, AttentionReason, Event, EventBody, Grade,
    JsonMap, Source, TurnFrom, TurnResult, DELETE_KEY, HUB_THREAD,
};
use crate::reduce::{AttentionStack, Registry};

/// What a notification is about, most urgent first (the derived `Ord` is that order).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyEvent {
    /// A runtime asks to use a tool.
    Permission,
    /// A runtime, or the hub (which thread did you mean?), asks a question.
    Question,
    /// A confirm card, or a delete card the orchestrator asked for: only the user answers it.
    Confirm,
    /// A failure the hub left for the user (after any automatic retry).
    Error,
    /// A finished turn graded `gave_up` or `needs_verification`.
    Review,
    /// A turn silent while something waits to start the next one (a hung-turn card).
    Hung,
    /// A running turn silent for `stall_after_secs`.
    Stalled,
    /// A worker finished a turn (`report.ready`), when no failure or review covers it.
    Report,
    /// The orchestrator finished a turn the user started (its reply to a message), or one its
    /// runtime started by itself after a background task.
    Reply,
}

impl NotifyEvent {
    /// The events notified unless the configuration says otherwise: all but [`Stalled`]
    /// (silence can be legitimate long work).
    ///
    /// [`Stalled`]: NotifyEvent::Stalled
    pub const DEFAULTS: &'static [NotifyEvent] = &[
        NotifyEvent::Permission,
        NotifyEvent::Question,
        NotifyEvent::Confirm,
        NotifyEvent::Error,
        NotifyEvent::Review,
        NotifyEvent::Hung,
        NotifyEvent::Report,
        NotifyEvent::Reply,
    ];

    /// What a message says the thread does (`api needs permission: ...`).
    fn verb(self) -> &'static str {
        match self {
            NotifyEvent::Permission => "needs permission",
            NotifyEvent::Question => "asks",
            NotifyEvent::Confirm => "waits for your confirmation",
            NotifyEvent::Error => "failed",
            NotifyEvent::Review => "needs review",
            NotifyEvent::Hung => "seems hung",
            NotifyEvent::Stalled => "is stalled",
            NotifyEvent::Report => "finished",
            NotifyEvent::Reply => "replied",
        }
    }
}

/// Shortest time between two notifications: what comes meanwhile goes out together after it.
pub const GAP: Duration = Duration::from_secs(5);

/// Until a terminal reports focus, a key, click or paste this recent counts as the user
/// looking; a TUI tells its hub it is looked at while it has focus and such input this recent.
pub const INPUT_FOCUS: Duration = Duration::from_secs(30);

/// How long a candidate waits before it is checked again and delivered: long enough for an
/// automatic retry to resolve a crash's `error` item, or for an answer given at once.
const HOLD: Duration = Duration::from_secs(2);

/// [`HOLD`] for `error` items: a crashed runtime's retry waits for the thread to be reaped.
const ERROR_HOLD: Duration = Duration::from_secs(5);

/// Longest text of one notification line, in characters.
const TEXT_MAX: usize = 160;

/// How many keys a policy remembers to deliver each thing once.
const KEYS_MAX: usize = 4096;

/// Threads a digest of several names before "+N more".
const DIGEST_NAMES: usize = 3;

/// Something worth telling the user, not yet checked again ([`still_due`]) nor delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What it is about.
    pub event: NotifyEvent,
    /// Identity, delivered at most once: `attn:<id>`, `report:<thread>:<turn>` or
    /// `reply:<thread>:<turn>`.
    pub key: String,
    /// Thread it concerns ([`HUB_THREAD`] for the hub's own questions and cards).
    pub thread: String,
    /// Turn it concerns: a report's or reply's, the one an attention item was raised in.
    pub turn: Option<u64>,
    /// The attention item, for the ones raised by one.
    pub attn_id: Option<String>,
    /// Who it is about: the thread's name, `orchestrator`, or `twr` for the hub.
    pub who: String,
    /// One line on what happened, without the name (control characters removed).
    pub text: String,
    /// A later revision of what `key` already gave out (a rewritten report): how the
    /// candidate held with that key reads now. It is never news by itself.
    pub revision: bool,
}

impl Candidate {
    /// How long to wait before checking it again and delivering it.
    pub fn hold(&self) -> Duration {
        match self.event {
            NotifyEvent::Error => ERROR_HOLD,
            _ => HOLD,
        }
    }

    /// Whether this makes `held` (an earlier candidate, not yet delivered) old news: a review
    /// of the turn a held report is about (its report was rewritten with a grade that needs
    /// review).
    pub fn covers(&self, held: &Candidate) -> bool {
        self.event == NotifyEvent::Review
            && held.event == NotifyEvent::Report
            && held.thread == self.thread
            && held.turn.is_some()
            && held.turn == self.turn
    }

    /// `api needs permission: Bash cargo test`.
    pub fn line(&self) -> String {
        if self.text.is_empty() {
            format!("{} {}", self.who, self.event.verb())
        } else {
            format!("{} {}: {}", self.who, self.event.verb(), self.text)
        }
    }
}

/// The rules: which events are wanted, what was already turned into a candidate, and the
/// orchestrator turns users started (whose end is a reply). Serialized whole (sets sorted) in
/// the hub's checkpoints of its folded state. Equal policies remember the same (the compacted
/// backlog's tests compare them, [`crate::compact`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Policy {
    #[serde(serialize_with = "crate::sorted::set")]
    events: HashSet<NotifyEvent>,
    /// Keys already given out, oldest first (bounded by [`KEYS_MAX`]).
    order: VecDeque<String>,
    #[serde(serialize_with = "crate::sorted::set")]
    given: HashSet<String>,
    /// Open orchestrator turns a user started: (thread, turn).
    #[serde(serialize_with = "crate::sorted::set")]
    user_turns: HashSet<(String, u64)>,
    /// Per thread, the turns whose report is known: a rewrite of one only revises it,
    /// whatever the thread did since. Kept across a delete: a deleted thread can be restored.
    #[serde(serialize_with = "crate::sorted::map_of_sets")]
    reported: HashMap<String, HashSet<u64>>,
}

impl Policy {
    /// A policy for `events`.
    pub fn new(events: &[NotifyEvent]) -> Self {
        Policy {
            events: events.iter().copied().collect(),
            ..Default::default()
        }
    }

    /// A policy for `events` that remembers what `memory` (a policy of any events, folded from
    /// the same items: a hub's snapshot, `crate::client_state`) does. It then gives for each
    /// later item what a policy for `events` folded from those items would: keys of events not
    /// wanted are never looked up (whether an event is wanted is asked first), and no key given
    /// in a replay can come again (an attention key names the raise that opened the item, a
    /// report or reply key its turn, and a report known is a revision).
    pub fn adopt(events: &[NotifyEvent], memory: &Policy) -> Policy {
        Policy {
            events: events.iter().copied().collect(),
            ..memory.clone()
        }
    }

    /// This memory without what it keeps of `threads` (gone for good: purged from the trash):
    /// their reports known and their report and reply keys. What the compacted backlog leaves
    /// out of a purged thread (`crate::compact`) changes nothing else of it.
    pub fn without_threads(&self, threads: &HashSet<String>) -> Policy {
        let theirs = |key: &str| {
            ["report:", "reply:"].iter().any(|p| {
                key.strip_prefix(p)
                    .and_then(|rest| rest.rsplit_once(':'))
                    .is_some_and(|(t, _)| threads.contains(t))
            })
        };
        let mut p = self.clone();
        p.reported.retain(|t, _| !threads.contains(t));
        p.user_turns.retain(|(t, _)| !threads.contains(t));
        p.given.retain(|k| !theirs(k));
        p.order.retain(|k| !theirs(k));
        p
    }

    /// The wanted events, most urgent first.
    pub fn wanted(&self) -> Vec<NotifyEvent> {
        let mut out: Vec<NotifyEvent> = self.events.iter().copied().collect();
        out.sort();
        out
    }

    /// Whether `event` is wanted.
    pub fn wants(&self, event: NotifyEvent) -> bool {
        self.events.contains(&event)
    }

    /// Looks at control event `ev` at control position `gseq`, already folded into
    /// `registry` and `attention`, and returns what it gives the user to know, if anything.
    /// Feed it live events only: a replay of the past is no news ([`Policy::recall`]).
    pub fn observe(
        &mut self,
        ev: &Event,
        gseq: Option<u64>,
        registry: &Registry,
        attention: &AttentionStack,
    ) -> Option<Candidate> {
        let c = match &ev.body {
            EventBody::AttentionRaised(p) => {
                let item = attention.get(&p.attn_id)?;
                // A raise of an item already open (a permission sent again with new input)
                // is no new request.
                if gseq.is_some_and(|g| item.first_revision != 0 && item.first_revision != g) {
                    return None;
                }
                // A delete card a user client asked for: that client shows it (`twr delete`
                // asks y/N, the TUI its card).
                if is_delete_card(&ev.thread, &p.ext) && delete_asked_by_user(&p.ext) {
                    return None;
                }
                let event = if is_confirm_card(&ev.thread, &p.ext) {
                    NotifyEvent::Confirm
                } else {
                    match p.reason {
                        AttentionReason::Permission => NotifyEvent::Permission,
                        AttentionReason::Question => NotifyEvent::Question,
                        AttentionReason::Error => NotifyEvent::Error,
                        AttentionReason::Review => NotifyEvent::Review,
                        AttentionReason::Stalled if is_hung_card(&p.ext) => NotifyEvent::Hung,
                        AttentionReason::Stalled => NotifyEvent::Stalled,
                    }
                };
                let who = who(&ev.thread, registry);
                Candidate {
                    event,
                    // With the raise that opened it: an id reopened after an answer is new.
                    key: format!("attn:{}:{}", p.attn_id, item.event_id),
                    thread: ev.thread.clone(),
                    turn: ev.turn,
                    attn_id: Some(p.attn_id.clone()),
                    text: clean(without_name(&p.summary, &who), TEXT_MAX),
                    who,
                    revision: false,
                }
            }
            EventBody::ReportReady(p) => {
                let view = registry.get(&ev.thread)?;
                if ev.thread == HUB_THREAD || view.is_orchestrator() {
                    return None;
                }
                let turn = p.turn.or(ev.turn)?;
                let known = self
                    .reported
                    .get(&ev.thread)
                    .is_some_and(|t| t.contains(&turn));
                // A rewrite of a known report revises it, whatever the thread did since (a
                // later turn ended, or has background tasks running); it is never news anew.
                if !known {
                    // A failed turn has its `error` item (or its automatic retry); an
                    // interrupted one gets no report.
                    let completed = view
                        .last_turn
                        .as_ref()
                        .is_some_and(|l| l.turn == turn && l.result == TurnResult::Completed);
                    if !completed {
                        return None;
                    }
                    // Known from here on, whether or not it is news.
                    self.reported
                        .entry(ev.thread.clone())
                        .or_default()
                        .insert(turn);
                    // An interim report: the turn ended with background tasks still running,
                    // and the runtime's own turn after them reports again. The thread's tasks
                    // say so too, for a report that lacks the marker (its own tasks: a `!`
                    // command of the user's starts no turn that reports again).
                    let interim = view.own_background()
                        || p.ext
                            .as_ref()
                            .and_then(|e| e.get("twr"))
                            .and_then(|t| t.get("background_tasks"))
                            .and_then(serde_json::Value::as_u64)
                            .is_some_and(|n| n > 0);
                    if interim {
                        return None;
                    }
                }
                // Its `review` item says it (and covers a held report of the same turn).
                if p.grade.needs_review() && self.wants(NotifyEvent::Review) {
                    return None;
                }
                let grade = match p.grade {
                    Grade::Success => "",
                    Grade::Partial => "partly: ",
                    Grade::GaveUp => "gave up: ",
                    Grade::NeedsVerification => "needs verification: ",
                };
                Candidate {
                    event: NotifyEvent::Report,
                    key: format!("report:{}:{turn}", ev.thread),
                    thread: ev.thread.clone(),
                    turn: Some(turn),
                    attn_id: None,
                    who: view.name.clone(),
                    text: clean(&format!("{grade}{}", first_line(&p.summary)), TEXT_MAX),
                    revision: known,
                }
            }
            EventBody::TurnStarted(p) => {
                let turn = ev.turn?;
                // The user's message, or the runtime going on by itself (a background task
                // of the user's request finished); never a hub notice or handoff.
                // (Not a compaction the user asked for: it replies nothing.)
                let user = (matches!(ev.source, Source::User(_)) || p.from == TurnFrom::Runtime)
                    && !crate::event::is_compact_turn(p);
                if user && registry.get(&ev.thread)?.is_current_orchestrator() {
                    self.user_turns.insert((ev.thread.clone(), turn));
                }
                return None;
            }
            EventBody::TurnEnded(p) => {
                let turn = ev.turn?;
                if !self.user_turns.remove(&(ev.thread.clone(), turn)) {
                    return None;
                }
                // A failed reply raises an `error` item.
                if p.result != TurnResult::Completed {
                    return None;
                }
                Candidate {
                    event: NotifyEvent::Reply,
                    key: format!("reply:{}:{turn}", ev.thread),
                    thread: ev.thread.clone(),
                    turn: Some(turn),
                    attn_id: None,
                    who: "orchestrator".into(),
                    text: String::new(),
                    revision: false,
                }
            }
            EventBody::ThreadDeleted(_) => {
                self.user_turns.retain(|(t, _)| *t != ev.thread);
                return None;
            }
            _ => return None,
        };
        if !self.wants(c.event) {
            return None;
        }
        // A revision refreshes what is held under its key; a key is news once.
        if c.revision || self.give(&c.key) {
            return Some(c);
        }
        None
    }

    /// Looks at control event `ev` of a replay of the past, as [`Policy::observe`] does, but
    /// gives nothing: it only remembers what later events need, such as the orchestrator turn
    /// a user started that is still running.
    pub fn recall(
        &mut self,
        ev: &Event,
        gseq: Option<u64>,
        registry: &Registry,
        attention: &AttentionStack,
    ) {
        let _ = self.observe(ev, gseq, registry, attention);
    }

    /// Records `key` as given out; false when it already was.
    fn give(&mut self, key: &str) -> bool {
        if !self.given.insert(key.to_string()) {
            return false;
        }
        self.order.push_back(key.to_string());
        while self.order.len() > KEYS_MAX {
            if let Some(old) = self.order.pop_front() {
                self.given.remove(&old);
            }
        }
        true
    }
}

/// Candidates waiting for their [`Candidate::hold`], as a deliverer keeps them: a newer one
/// replaces what it covers, a revision rewrites the held one of its key.
#[derive(Debug, Default)]
pub struct Held {
    pending: Vec<(Instant, Candidate)>,
}

impl Held {
    /// Holds `c`, given at `now`: a revision only refreshes the candidate held under its key
    /// (once delivered, a rewrite stays quiet), anything else waits for its hold.
    pub fn add(&mut self, c: Candidate, now: Instant) {
        if c.revision {
            if let Some((_, held)) = self.pending.iter_mut().find(|(_, held)| held.key == c.key) {
                *held = Candidate {
                    revision: false,
                    ..c
                };
            }
            return;
        }
        self.pending.retain(|(_, held)| !c.covers(held));
        self.pending.push((now + c.hold(), c));
    }

    /// When something is due, and not within [`GAP`] of `last_sent`; `None`: nothing held.
    pub fn next_due(&self, last_sent: Option<Instant>) -> Option<Instant> {
        let due = self.pending.iter().map(|(at, _)| *at).min()?;
        Some(match last_sent {
            Some(sent) => due.max(sent + GAP),
            None => due,
        })
    }

    /// Takes what is due at `now` (the rest stays held).
    pub fn take_due(&mut self, now: Instant) -> Vec<Candidate> {
        let (due, waiting): (Vec<_>, Vec<_>) =
            self.pending.drain(..).partition(|(at, _)| *at <= now);
        self.pending = waiting;
        due.into_iter().map(|(_, c)| c).collect()
    }

    /// Drops everything held.
    pub fn clear(&mut self) {
        self.pending.clear();
    }
}

/// Whether `c` still says something true now: its attention item is still open, its thread
/// still exists.
pub fn still_due(c: &Candidate, registry: &Registry, attention: &AttentionStack) -> bool {
    match &c.attn_id {
        // Still the item that was raised, not one reopened under its id since.
        Some(id) => attention
            .get(id)
            .is_some_and(|i| c.key == format!("attn:{id}:{}", i.event_id)),
        None => c.thread == HUB_THREAD || registry.get(&c.thread).is_some(),
    }
}

/// One notification for `due` (most urgent first): a title and a body, both free of control
/// characters. `None` when nothing is due.
pub fn digest(due: &[Candidate]) -> Option<(String, String)> {
    let mut due: Vec<&Candidate> = due.iter().collect();
    due.sort_by_key(|c| c.event);
    match due.as_slice() {
        [] => None,
        [one] => Some((
            clean(&format!("twr: {}", one.who), TEXT_MAX),
            clean(&one.line(), TEXT_MAX * 2),
        )),
        many => {
            let need = many
                .iter()
                .filter(|c| c.event < NotifyEvent::Stalled)
                .count();
            let title = if need > 0 {
                format!("twr: {need} need you")
            } else {
                format!("twr: {} updates", many.len())
            };
            let mut parts: Vec<String> = many
                .iter()
                .take(DIGEST_NAMES)
                .map(|c| format!("{} {}", c.who, c.event.verb()))
                .collect();
            if many.len() > DIGEST_NAMES {
                parts.push(format!("+{} more", many.len() - DIGEST_NAMES));
            }
            Some((
                clean(&title, TEXT_MAX),
                clean(&parts.join(", "), TEXT_MAX * 2),
            ))
        }
    }
}

/// Who a thread is in a message.
fn who(thread: &str, registry: &Registry) -> String {
    if thread == HUB_THREAD {
        return "twr".into();
    }
    match registry.get(thread) {
        Some(v) if v.is_orchestrator() => "orchestrator".into(),
        Some(v) => v.name.clone(),
        None => thread.to_string(),
    }
}

/// Whether a delete card's `ext.twr.delete.asked_by` is a user client (`user:<client>`).
fn delete_asked_by_user(ext: &Option<JsonMap>) -> bool {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(DELETE_KEY))
        .and_then(|d| d.get("asked_by"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|by| by.starts_with("user:"))
}

/// `summary` without a leading `<name>: ` (the hub's own items start with it).
fn without_name<'a>(summary: &'a str, name: &str) -> &'a str {
    summary
        .strip_prefix(name)
        .and_then(|s| s.strip_prefix(": "))
        .unwrap_or(summary)
}

/// The first non-empty line of `s`.
fn first_line(s: &str) -> &str {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// `s` on one line, every control character (C0, DEL, C1: escapes, bells, line breaks) a
/// space, runs of spaces one, cut to `max` characters with an ellipsis. Safe to put inside a
/// terminal escape sequence or a desktop notification.
pub fn clean(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for ch in s.chars() {
        if ch.is_control() || ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    if out.chars().count() > max {
        let cut: String = out.chars().take(max.saturating_sub(1)).collect();
        return format!("{}…", cut.trim_end());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AttentionRaised, TaskUpdated, ThreadCreated};
    use crate::testkit::ThreadScript;
    use serde_json::json;

    /// A registry and stack fed control events with their gseq, and a policy watching them.
    struct Feed {
        registry: Registry,
        attention: AttentionStack,
        policy: Policy,
        gseq: u64,
    }

    impl Feed {
        fn new(events: &[NotifyEvent]) -> Self {
            Feed {
                registry: Registry::new(),
                attention: AttentionStack::new(),
                policy: Policy::new(events),
                gseq: 0,
            }
        }

        fn defaults() -> Self {
            Self::new(NotifyEvent::DEFAULTS)
        }

        fn push(&mut self, ev: Event) -> Option<Candidate> {
            self.gseq += 1;
            self.registry.apply(&ev);
            self.attention.apply_at(&ev, Some(self.gseq));
            if !ev.body.is_control() {
                return None;
            }
            self.policy
                .observe(&ev, Some(self.gseq), &self.registry, &self.attention)
        }
    }

    fn orchestrator(s: &mut ThreadScript) -> Event {
        s.event(EventBody::ThreadCreated(ThreadCreated {
            name: "orch".into(),
            ext: json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned(),
            ..Default::default()
        }))
    }

    #[test]
    fn a_new_permission_notifies_once_and_its_reraise_does_not() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        let c = f
            .push(api.raise("t1:p1", AttentionReason::Permission))
            .unwrap();
        assert_eq!(c.event, NotifyEvent::Permission);
        assert_eq!(c.who, "api");
        assert_eq!(c.line(), "api needs permission: Permission t1:p1");
        assert!(still_due(&c, &f.registry, &f.attention));
        assert_eq!(
            f.push(api.raise("t1:p1", AttentionReason::Permission)),
            None
        );
        assert_eq!(f.push(api.annotate("t1:p1", "looks fine")), None);
        f.push(api.resolve("t1:p1"));
        assert!(
            !still_due(&c, &f.registry, &f.attention),
            "answered: no longer due"
        );
    }

    #[test]
    fn quiet_events_give_nothing() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        for ev in [
            api.created("api"),
            api.started("s1"),
            api.turn_started("go"),
            api.git("main", 1),
            api.parked(),
            api.resumed("s1"),
            api.stopped(),
        ] {
            assert_eq!(f.push(ev), None);
        }
    }

    #[test]
    fn a_confirm_card_is_a_confirmation_not_a_question() {
        let mut f = Feed::defaults();
        let mut hub = ThreadScript::new(HUB_THREAD);
        let ev = hub.event(EventBody::AttentionRaised(AttentionRaised {
            attn_id: "c1".into(),
            reason: AttentionReason::Question,
            summary: "Delete api?".into(),
            options: vec!["Delete".into(), "Cancel".into()],
            ext: json!({"twr": {"delete": {"threads": [], "asked_by": "orchestrator"}}})
                .as_object()
                .cloned(),
            ..Default::default()
        }));
        let c = f.push(ev).unwrap();
        assert_eq!(c.event, NotifyEvent::Confirm);
        assert_eq!(c.who, "twr");
        // One the user asked for (`twr delete`) is on their screen already.
        let ev = hub.event(EventBody::AttentionRaised(AttentionRaised {
            attn_id: "c2".into(),
            reason: AttentionReason::Question,
            summary: "Delete web?".into(),
            ext: json!({"twr": {"delete": {"threads": [], "asked_by": "user:cli"}}})
                .as_object()
                .cloned(),
            ..Default::default()
        }));
        assert_eq!(f.push(ev), None);
        // The hub's routing question is a question.
        let c = f.push(hub.raise("q1", AttentionReason::Question)).unwrap();
        assert_eq!(c.event, NotifyEvent::Question);
    }

    #[test]
    fn a_completed_turn_reports_and_a_failed_one_leaves_it_to_its_error() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::Success)).unwrap();
        assert_eq!(c.event, NotifyEvent::Report);
        assert_eq!(c.line(), "api finished: Did the thing.");
        // A rewritten report of the same turn is a revision of the same news.
        let r = f.push(api.report(Some(1), Grade::Partial)).unwrap();
        assert!(r.revision);
        assert_eq!(r.key, c.key);
        assert_eq!(r.line(), "api finished: partly: Did the thing.");

        f.push(api.turn_started("again"));
        f.push(api.turn_ended(TurnResult::Error));
        assert_eq!(f.push(api.report(Some(2), Grade::GaveUp)), None);
        let mut err = api.raise("e1", AttentionReason::Error);
        if let EventBody::AttentionRaised(p) = &mut err.body {
            p.summary = "api: turn 2 failed: boom".into();
        }
        let c = f.push(err).unwrap();
        assert_eq!(c.event, NotifyEvent::Error);
        assert_eq!(c.line(), "api failed: turn 2 failed: boom");
    }

    #[test]
    fn an_auto_retried_crash_is_no_longer_due() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.crashed(1));
        let c = f.push(api.raise("e1", AttentionReason::Error)).unwrap();
        assert_eq!(c.hold(), ERROR_HOLD);
        let mut retry = api.resolve("e1");
        if let EventBody::AttentionResolved(p) = &mut retry.body {
            p.by = crate::event::ResolvedBy::AutoRetry;
            p.answer = None;
        }
        f.push(retry);
        assert!(!still_due(&c, &f.registry, &f.attention));
    }

    #[test]
    fn a_review_covers_its_report_unless_reviews_are_off() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        assert_eq!(f.push(api.report(Some(1), Grade::GaveUp)), None);
        let c = f.push(api.raise("r1", AttentionReason::Review)).unwrap();
        assert_eq!(c.event, NotifyEvent::Review);

        let mut f = Feed::new(&[NotifyEvent::Report]);
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::GaveUp)).unwrap();
        assert_eq!(c.line(), "api finished: gave up: Did the thing.");
        assert_eq!(f.push(api.raise("r1", AttentionReason::Review)), None);
    }

    /// A report its review item said, live or replayed, is known all the same: rewritten as a
    /// success it revises that news, never a completion anew.
    #[test]
    fn a_report_its_review_said_is_revised_not_announced_when_rewritten() {
        for replayed in [false, true] {
            let mut f = Feed::defaults();
            let mut api = ThreadScript::new("t1");
            let past = [
                api.created("api"),
                api.turn_started("go"),
                api.turn_ended(TurnResult::Completed),
                api.report(Some(1), Grade::GaveUp),
                api.raise("r1", AttentionReason::Review),
            ];
            for ev in past {
                if replayed {
                    f.gseq += 1;
                    f.registry.apply(&ev);
                    f.attention.apply_at(&ev, Some(f.gseq));
                    f.policy
                        .recall(&ev, Some(f.gseq), &f.registry, &f.attention);
                } else {
                    f.push(ev);
                }
            }
            let r = f.push(api.report(Some(1), Grade::Success)).unwrap();
            assert!(r.revision, "replayed: {replayed}");
        }
    }

    /// A report rewritten after a later turn ended still revises its news.
    #[test]
    fn a_report_rewritten_after_a_later_turn_still_revises_it() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::Success)).unwrap();
        f.push(api.turn_started("again"));
        f.push(api.turn_ended(TurnResult::Completed));
        let r = f.push(api.report(Some(1), Grade::Partial)).unwrap();
        assert!(r.revision);
        assert_eq!(r.key, c.key);
        assert_eq!(r.line(), "api finished: partly: Did the thing.");
        // A turn never reported stays quiet once a later one ended.
        f.push(api.turn_started("third"));
        f.push(api.turn_ended(TurnResult::Completed));
        assert_eq!(f.push(api.report(Some(2), Grade::Success)), None);
    }

    /// A known report rewritten while a later turn's background tasks run still revises it.
    #[test]
    fn a_report_rewritten_while_a_later_turns_tasks_run_still_revises_it() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.started("s1"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::Success)).unwrap();
        f.push(api.turn_started("again"));
        f.push(api.event(EventBody::TaskUpdated(TaskUpdated {
            task_id: "tests".into(),
            running: true,
            description: "tests".into(),
            ..Default::default()
        })));
        f.push(api.turn_ended(TurnResult::Completed));
        let r = f.push(api.report(Some(1), Grade::Partial)).unwrap();
        assert!(r.revision);
        assert_eq!(r.key, c.key);
    }

    /// A report recalled from the replay stays known however many other keys go by.
    #[test]
    fn a_recalled_report_stays_known_past_the_key_cache() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        for ev in [
            api.created("api"),
            api.turn_started("go"),
            api.turn_ended(TurnResult::Completed),
            api.report(Some(1), Grade::Success),
        ] {
            f.gseq += 1;
            f.registry.apply(&ev);
            f.attention.apply_at(&ev, Some(f.gseq));
            f.policy
                .recall(&ev, Some(f.gseq), &f.registry, &f.attention);
        }
        let mut web = ThreadScript::new("t2");
        f.push(web.created("web"));
        for i in 0..KEYS_MAX + 10 {
            let id = format!("p{i}");
            assert!(f
                .push(web.raise(&id, AttentionReason::Permission))
                .is_some());
            f.push(web.resolve(&id));
        }
        assert!(f
            .push(api.report(Some(1), Grade::Partial))
            .is_some_and(|c| c.revision));
    }

    /// An id reopened after its item was answered is a new request, and the first one's
    /// candidate is no longer due once the id names the new item.
    #[test]
    fn an_attention_id_reopened_after_an_answer_is_a_new_request() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        let first = f
            .push(api.raise("p1", AttentionReason::Permission))
            .unwrap();
        assert!(still_due(&first, &f.registry, &f.attention));
        f.push(api.resolve("p1"));
        let again = f
            .push(api.raise("p1", AttentionReason::Permission))
            .unwrap();
        assert_ne!(again.key, first.key);
        assert!(!still_due(&first, &f.registry, &f.attention));
        assert!(still_due(&again, &f.registry, &f.attention));
    }

    /// A report its review said, then the thread deleted and restored (live or replayed):
    /// rewritten as a success it revises that news, never a completion anew.
    #[test]
    fn a_restored_threads_report_rewritten_revises_it() {
        for replayed in [false, true] {
            let mut f = Feed::defaults();
            let mut api = ThreadScript::new("t1");
            let past = [
                api.created("api"),
                api.turn_started("go"),
                api.turn_ended(TurnResult::Completed),
                api.report(Some(1), Grade::GaveUp),
                api.raise("r1", AttentionReason::Review),
                api.event(EventBody::ThreadDeleted(crate::event::ThreadDeleted {
                    name: "api".into(),
                    ..Default::default()
                })),
                api.event(EventBody::ThreadRestored(crate::event::ThreadRestored {
                    item: "trash-1".into(),
                    name: "api".into(),
                    ..Default::default()
                })),
            ];
            for ev in past {
                if replayed {
                    f.gseq += 1;
                    f.registry.apply(&ev);
                    f.attention.apply_at(&ev, Some(f.gseq));
                    f.policy
                        .recall(&ev, Some(f.gseq), &f.registry, &f.attention);
                } else {
                    f.push(ev);
                }
            }
            let r = f.push(api.report(Some(1), Grade::Success)).unwrap();
            assert!(r.revision, "replayed: {replayed}");
        }
    }

    /// However many turns a thread reports since, a rewrite of an earlier report revises it.
    #[test]
    fn a_report_rewritten_after_many_later_turns_still_revises_it() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::Success)).unwrap();
        for turn in 2..40 {
            f.push(api.turn_started("again"));
            f.push(api.turn_ended(TurnResult::Completed));
            f.push(api.report(Some(turn), Grade::Success));
        }
        let r = f.push(api.report(Some(1), Grade::Partial)).unwrap();
        assert!(r.revision);
        assert_eq!(r.key, c.key);
    }

    #[test]
    fn stalls_are_off_by_default() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        assert_eq!(f.push(api.raise("s1", AttentionReason::Stalled)), None);
        let mut f = Feed::new(&[NotifyEvent::Stalled]);
        let mut web = ThreadScript::new("t2");
        f.push(web.created("web"));
        f.push(web.turn_started("go"));
        assert!(f.push(web.raise("s2", AttentionReason::Stalled)).is_some());
    }

    #[test]
    fn only_the_orchestrators_reply_to_the_user_notifies() {
        let mut f = Feed::defaults();
        let mut o = ThreadScript::new("o1");
        f.push(orchestrator(&mut o));
        // A hub notice turn: no news.
        let mut notice = o.turn_started("[twr notice]");
        notice.source = Source::Hub;
        f.push(notice);
        assert_eq!(f.push(o.turn_ended(TurnResult::Completed)), None);
        // The user's message.
        let mut said = o.turn_started("what is api doing?");
        said.source = Source::User("tui".into());
        f.push(said);
        let c = f.push(o.turn_ended(TurnResult::Completed)).unwrap();
        assert_eq!(c.event, NotifyEvent::Reply);
        assert_eq!(c.line(), "orchestrator replied");
        // Orchestrator turns get no report notification.
        assert_eq!(f.push(o.report(Some(2), Grade::Success)), None);
        // A failed reply is left to its error item.
        let mut said = o.turn_started("again");
        said.source = Source::User("tui".into());
        f.push(said);
        assert_eq!(f.push(o.turn_ended(TurnResult::Error)), None);
        // A compaction the user asked for replies nothing.
        let mut compact = o.turn_started("/compact");
        compact.source = Source::User("tui".into());
        if let EventBody::TurnStarted(p) = &mut compact.body {
            crate::event::set_twr_ext(
                &mut p.ext,
                crate::event::COMPACT_TURN_KEY,
                serde_json::json!(true),
            );
        }
        f.push(compact);
        assert_eq!(f.push(o.turn_ended(TurnResult::Completed)), None);
    }

    #[test]
    fn an_interim_report_waits_for_the_runtime_turn_after_its_background_tasks() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let mut interim = api.report(Some(1), Grade::Success);
        if let EventBody::ReportReady(p) = &mut interim.body {
            p.ext = json!({"twr": {"background_tasks": 1}}).as_object().cloned();
        }
        assert_eq!(f.push(interim), None);
        let mut resumed = api.turn_started("");
        if let EventBody::TurnStarted(p) = &mut resumed.body {
            p.from = TurnFrom::Runtime;
        }
        f.push(resumed);
        f.push(api.turn_ended(TurnResult::Completed));
        assert!(f.push(api.report(Some(2), Grade::Success)).is_some());

        // The orchestrator going on by itself after a background task replies too.
        let mut o = ThreadScript::new("o1");
        f.push(orchestrator(&mut o));
        let mut resumed = o.turn_started("");
        resumed.source = Source::Adapter("claude-code".into());
        if let EventBody::TurnStarted(p) = &mut resumed.body {
            p.from = TurnFrom::Runtime;
        }
        f.push(resumed);
        let c = f.push(o.turn_ended(TurnResult::Completed)).unwrap();
        assert_eq!(c.event, NotifyEvent::Reply);
    }

    #[test]
    fn a_report_without_the_marker_is_interim_while_the_threads_tasks_run() {
        let task = |id: &str, running| {
            EventBody::TaskUpdated(TaskUpdated {
                task_id: id.into(),
                running,
                description: "tests".into(),
                ..Default::default()
            })
        };
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.started("s1"));
        f.push(api.turn_started("go"));
        f.push(api.event(task("tests", true)));
        f.push(api.turn_ended(TurnResult::Completed));
        // A report written over the hub's interim one, without its marker.
        assert_eq!(f.push(api.report(Some(1), Grade::Success)), None);
        f.push(api.event(task("tests", false)));
        let mut resumed = api.turn_started("");
        if let EventBody::TurnStarted(p) = &mut resumed.body {
            p.from = TurnFrom::Runtime;
        }
        f.push(resumed);
        f.push(api.turn_ended(TurnResult::Completed));
        assert!(f.push(api.report(Some(2), Grade::Success)).is_some());
    }

    #[test]
    fn a_review_of_a_rewritten_report_covers_its_finished() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let report = f.push(api.report(Some(1), Grade::Success)).unwrap();
        assert_eq!(report.turn, Some(1));
        assert_eq!(f.push(api.report(Some(1), Grade::NeedsVerification)), None);
        let mut raised = api.raise("r1", AttentionReason::Review);
        raised.turn = Some(1);
        let review = f.push(raised).unwrap();
        assert!(review.covers(&report));
        assert!(!report.covers(&review));
        // Not another turn's report, nor another thread's.
        let mut other = report.clone();
        other.turn = Some(2);
        assert!(!review.covers(&other));
        other.turn = Some(1);
        other.thread = "t2".into();
        assert!(!review.covers(&other));
    }

    #[test]
    fn a_recalled_replay_gives_nothing_but_keeps_the_turns_still_running() {
        let mut f = Feed::defaults();
        let mut o = ThreadScript::new("o1");
        let mut api = ThreadScript::new("t1");
        let mut said = o.turn_started("what is api doing?");
        said.source = Source::User("tui".into());
        for ev in [
            orchestrator(&mut o),
            api.created("api"),
            api.turn_started("go"),
            api.raise("p1", AttentionReason::Permission),
            api.turn_ended(TurnResult::Completed),
            api.report(Some(1), Grade::Success),
            said,
        ] {
            f.gseq += 1;
            f.registry.apply(&ev);
            f.attention.apply_at(&ev, Some(f.gseq));
            f.policy
                .recall(&ev, Some(f.gseq), &f.registry, &f.attention);
        }
        // The past is no news: rewritten, it only revises what nothing holds.
        assert!(f
            .push(api.report(Some(1), Grade::Partial))
            .is_some_and(|c| c.revision));
        // The reply to the message sent before.
        let c = f.push(o.turn_ended(TurnResult::Completed)).unwrap();
        assert_eq!(c.event, NotifyEvent::Reply);
    }

    #[test]
    fn a_deleted_thread_is_no_longer_due() {
        let mut f = Feed::defaults();
        let mut api = ThreadScript::new("t1");
        f.push(api.created("api"));
        f.push(api.turn_started("go"));
        f.push(api.turn_ended(TurnResult::Completed));
        let c = f.push(api.report(Some(1), Grade::Success)).unwrap();
        f.push(api.event(EventBody::ThreadDeleted(Default::default())));
        assert!(!still_due(&c, &f.registry, &f.attention));
    }

    #[test]
    fn digests_put_the_urgent_first_and_count_the_rest() {
        let c = |event, who: &str| Candidate {
            event,
            key: who.into(),
            thread: who.into(),
            turn: None,
            attn_id: None,
            who: who.into(),
            text: "x".into(),
            revision: false,
        };
        assert_eq!(digest(&[]), None);
        assert_eq!(
            digest(&[c(NotifyEvent::Question, "web")]),
            Some(("twr: web".into(), "web asks: x".into()))
        );
        let (title, body) = digest(&[
            c(NotifyEvent::Report, "a"),
            c(NotifyEvent::Permission, "b"),
            c(NotifyEvent::Report, "c"),
            c(NotifyEvent::Error, "d"),
            c(NotifyEvent::Reply, "e"),
        ])
        .unwrap();
        assert_eq!(title, "twr: 2 need you");
        assert_eq!(body, "b needs permission, d failed, a finished, +2 more");
        let (title, _) =
            digest(&[c(NotifyEvent::Report, "a"), c(NotifyEvent::Reply, "o")]).unwrap();
        assert_eq!(title, "twr: 2 updates");
        // A name with control characters, or a long one, makes a clean title.
        let (title, body) = digest(&[c(NotifyEvent::Question, "a\x07b\x1b]9;x\x1b\\")]).unwrap();
        assert_eq!(title, "twr: a b ]9;x \\");
        assert!(!body.chars().any(char::is_control));
        let long = "n".repeat(500);
        let (title, _) = digest(&[c(NotifyEvent::Question, &long)]).unwrap();
        assert_eq!(title.chars().count(), TEXT_MAX);
        assert!(title.starts_with("twr: nnn") && title.ends_with('…'));
    }

    #[test]
    fn clean_text_holds_no_control_characters() {
        assert_eq!(clean("a\x1b]9;x\x07b\n\tc  d", 100), "a ]9;x b c d");
        assert_eq!(clean("\u{9b}31m red", 100), "31m red");
        assert_eq!(clean("abcdefgh", 5), "abcd…");
        assert_eq!(clean("  ", 5), "");
    }
}

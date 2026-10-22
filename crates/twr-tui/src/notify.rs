//! Terminal notifications (`[tui.notify]`): the TUI rings or notifies through the terminal
//! it runs in when something needs the user, while the user is not looking at it.
//!
//! The rules of what deserves one are [`twr_core::notify`]'s; this module holds candidates
//! for their [`Candidate::hold`], checks them again ([`still_due`]), drops them while the
//! terminal has focus, batches what is due together into one message ([`digest`]), keeps
//! [`GAP`] between two alerts, and writes the escape sequence. Live control events are news;
//! the replay at startup is the past, only remembered ([`Notifier::recall`]).

use std::time::Instant;

use twr_core::config::{NotifyWhen, TerminalAlert, TuiNotifyConfig};
use twr_core::event::Event;
use twr_core::notify::NotifyEvent;
use twr_core::notify::{digest, still_due, Candidate, Held, Policy};
use twr_core::reduce::{AttentionStack, Registry};

pub use twr_core::notify::{GAP, INPUT_FOCUS};

/// How often, at most, the TUI tells its hub again that the user attends to it
/// (`notify.looking`; the hub believes it for 25 s).
pub const LOOKING_EVERY: std::time::Duration = std::time::Duration::from_secs(10);

/// What the TUI last told its hub of the user's attention (`notify.looking`), whose
/// notification command keeps quiet while the user attends to the TUI
/// ([`Notifier::attended`]).
#[derive(Debug, Default)]
pub struct Attendance {
    /// When it last said the user attends; `None`: it said they don't, or nothing yet.
    told: Option<Instant>,
}

impl Attendance {
    /// What to tell the hub at `now`, the user attending or not: `Some(true)` that they do
    /// (when it changed, or [`LOOKING_EVERY`] after it was last said), `Some(false)` that they
    /// stopped.
    pub fn step(&mut self, attended: bool, now: Instant) -> Option<bool> {
        match (attended, self.told) {
            (true, last)
                if last.is_none_or(|t| now.saturating_duration_since(t) >= LOOKING_EVERY) =>
            {
                self.told = Some(now);
                Some(true)
            }
            (false, Some(_)) => {
                self.told = None;
                Some(false)
            }
            _ => None,
        }
    }

    /// When to [`Attendance::step`] again: to say it again, or as the attention lapses.
    pub fn next_check(&self, notifier: &Notifier) -> Option<Instant> {
        let renew = self.told? + LOOKING_EVERY;
        Some(
            notifier
                .attention_ends()
                .map_or(renew, |end| end.min(renew)),
        )
    }

    /// Whether the hub was told the user attends, and not told otherwise since.
    pub fn told(&self) -> bool {
        self.told.is_some()
    }
}

/// Longest string GNU screen passes on, in bytes: it prints a longer one in its window.
const SCREEN_STRING_MAX: usize = 767;

/// The terminal multiplexer the TUI runs in, if any: OSC sequences go through its
/// passthrough to reach the terminal outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Multiplexer {
    /// None: sequences go to the terminal as they are.
    #[default]
    None,
    /// tmux: `ESC P tmux; ... ESC \`, every `ESC` inside doubled. tmux passes it on only with
    /// `allow-passthrough on`.
    Tmux,
    /// GNU screen: `ESC P ... ESC \`, at most [`SCREEN_STRING_MAX`] bytes inside.
    Screen,
    /// tmux and screen, one inside the other: which one is inside can't be told, so OSC
    /// sequences can't be wrapped for both. The bell stands in for them.
    Nested,
}

impl Multiplexer {
    /// The one the environment names: tmux (`$TMUX`), GNU screen (`$STY`), or both.
    pub fn from_env() -> Self {
        Self::named(
            std::env::var_os("TMUX").is_some(),
            std::env::var_os("STY").is_some(),
        )
    }

    /// The one for `$TMUX` set (`tmux`) and `$STY` set (`screen`).
    fn named(tmux: bool, screen: bool) -> Self {
        match (tmux, screen) {
            (true, true) => Multiplexer::Nested,
            (true, false) => Multiplexer::Tmux,
            (false, true) => Multiplexer::Screen,
            (false, false) => Multiplexer::None,
        }
    }
}

/// Whether the terminal has focus, as it reported (focus reporting, `ESC [ ? 1004 h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    /// Nothing reported yet: the terminal (or a multiplexer between, such as tmux without
    /// `focus-events`) may never report it.
    #[default]
    Unknown,
    /// Focused.
    Focused,
    /// Not focused.
    Unfocused,
}

/// What [`Notifier::workspace_waiting`] makes of another workspace's new waiting items.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceAlert {
    /// These bytes alert the terminal.
    Send(String),
    /// Nothing to send: the user looks at the TUI, or such alerts are off.
    Seen,
    /// Not now (too soon after the last alert): ask again later.
    Later,
}

/// Candidates waiting, the terminal's focus and when the last alert went out.
#[derive(Debug)]
pub struct Notifier {
    alert: TerminalAlert,
    when: NotifyWhen,
    /// What it notifies of (`[tui.notify] events`).
    events: Vec<NotifyEvent>,
    policy: Policy,
    /// Candidates and when they are due.
    pending: Held,
    focus: Focus,
    last_input: Option<Instant>,
    last_sent: Option<Instant>,
    /// What OSC sequences are wrapped for.
    mux: Multiplexer,
}

impl Notifier {
    /// A notifier for `cfg`, in a terminal behind `mux`.
    pub fn new(cfg: &TuiNotifyConfig, mux: Multiplexer) -> Self {
        Notifier {
            alert: cfg.terminal,
            when: cfg.when,
            events: cfg.events.clone(),
            policy: Policy::new(&cfg.events),
            pending: Held::default(),
            focus: Focus::Unknown,
            last_input: None,
            last_sent: None,
            mux,
        }
    }

    /// Alerts the terminal as `alert` says from now on (the settings changed
    /// `[tui.notify] terminal`), the rest as it was.
    pub fn set_alert(&mut self, alert: TerminalAlert) {
        self.alert = alert;
    }

    /// Whether it alerts at all (`terminal` is not `off`).
    pub fn enabled(&self) -> bool {
        self.alert != TerminalAlert::Off
    }

    /// Looks at live control event `ev` at `gseq`, already applied to `registry` and
    /// `attention`.
    pub fn observe(
        &mut self,
        ev: &Event,
        gseq: Option<u64>,
        registry: &Registry,
        attention: &AttentionStack,
        now: Instant,
    ) {
        // Followed while off too (what turns run), so that alerts turned on midway (the
        // settings) know of the turns already started; nothing is held for them meanwhile.
        if let Some(c) = self.policy.observe(ev, gseq, registry, attention) {
            if self.enabled() {
                self.pending.add(c, now);
            }
        }
    }

    /// Looks at control event `ev` of the replay at startup: it notifies nothing, but an
    /// orchestrator turn still running then replies when it ends.
    pub fn recall(
        &mut self,
        ev: &Event,
        gseq: Option<u64>,
        registry: &Registry,
        attention: &AttentionStack,
    ) {
        self.policy.recall(ev, gseq, registry, attention);
    }

    /// What it remembers of the control stream (the tests compare it).
    #[cfg(test)]
    pub(crate) fn policy(&self) -> &Policy {
        &self.policy
    }

    /// A notifier for another hub (a workspace switched to): nothing held or remembered of
    /// this one's log, the terminal's focus and the time of the last alert kept.
    pub fn renewed(&self) -> Notifier {
        Notifier {
            alert: self.alert,
            when: self.when,
            events: self.events.clone(),
            policy: Policy::new(&self.events),
            pending: Held::default(),
            focus: self.focus,
            last_input: self.last_input,
            last_sent: self.last_sent,
            mux: self.mux,
        }
    }

    /// Of `permissions` and `questions` waiting on the user (in another workspace), those of
    /// the kinds it notifies of (`[tui.notify] events`).
    pub fn waiting_count(&self, permissions: usize, questions: usize) -> usize {
        let on = |e| self.events.contains(&e);
        usize::from(on(NotifyEvent::Permission)) * permissions
            + usize::from(on(NotifyEvent::Question)) * questions
    }

    /// Workspace `name`, which is not the one shown, now has `waiting` items blocking on the
    /// user, more than when last looked at: the bytes that alert the terminal, naming the
    /// workspace. Nothing while the user looks at the TUI (its tab strip shows the count) or
    /// when neither permissions nor questions notify; within [`GAP`] of the last alert, not
    /// yet ([`WorkspaceAlert::Later`]).
    pub fn workspace_waiting(
        &mut self,
        name: &str,
        waiting: usize,
        now: Instant,
    ) -> WorkspaceAlert {
        let wanted = self
            .events
            .iter()
            .any(|e| matches!(e, NotifyEvent::Permission | NotifyEvent::Question));
        if !self.enabled() || !wanted || self.looking(now) {
            return WorkspaceAlert::Seen;
        }
        if self
            .last_sent
            .is_some_and(|t| now.saturating_duration_since(t) < GAP)
        {
            return WorkspaceAlert::Later;
        }
        self.last_sent = Some(now);
        let body = format!(
            "{waiting} item{} waiting on you",
            if waiting == 1 { "" } else { "s" }
        );
        WorkspaceAlert::Send(sequence(
            self.alert,
            &format!("twr: workspace {name}"),
            &body,
            self.mux,
        ))
    }

    /// The control stream ended: nothing held can be checked again (an answer given
    /// meanwhile never arrives), so nothing held is sent.
    pub fn lost(&mut self) {
        self.pending.clear();
    }

    /// The terminal reported focus gained (`true`) or lost.
    pub fn focus(&mut self, focused: bool) {
        self.focus = if focused {
            Focus::Focused
        } else {
            Focus::Unfocused
        };
    }

    /// The user pressed a key, clicked or pasted at `now`.
    pub fn input(&mut self, now: Instant) {
        self.last_input = Some(now);
    }

    /// When [`Notifier::flush`] has something to do, if ever.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.next_due(self.last_sent)
    }

    /// Whether the user attends to the TUI at `now`: it has focus (or never said) and a key,
    /// click or paste came within [`INPUT_FOCUS`]. The TUI tells its hub so
    /// (`notify.looking`), whose notification hook then keeps quiet.
    pub fn attended(&self, now: Instant) -> bool {
        self.focus != Focus::Unfocused
            && self
                .last_input
                .is_some_and(|t| now.saturating_duration_since(t) < INPUT_FOCUS)
    }

    /// When [`Notifier::attended`] turns false without anything happening: the last input's
    /// [`INPUT_FOCUS`] later.
    pub fn attention_ends(&self) -> Option<Instant> {
        self.last_input.map(|t| t + INPUT_FOCUS)
    }

    /// Whether the user is looking at the TUI at `now`, as far as it can tell.
    fn looking(&self, now: Instant) -> bool {
        match (self.when, self.focus) {
            (NotifyWhen::Always, _) => false,
            (NotifyWhen::Unfocused, Focus::Focused) => true,
            (NotifyWhen::Unfocused, Focus::Unfocused) => false,
            (NotifyWhen::Unfocused, Focus::Unknown) => self
                .last_input
                .is_some_and(|t| now.saturating_duration_since(t) < INPUT_FOCUS),
        }
    }

    /// Takes what is due at `now`, still true and unseen, and returns the bytes that alert
    /// the terminal for all of it at once; `None` when nothing is to be said. What is due
    /// while the user looks at the TUI is dropped: they see it there.
    pub fn flush(
        &mut self,
        registry: &Registry,
        attention: &AttentionStack,
        now: Instant,
    ) -> Option<String> {
        if self.next_due().is_none_or(|at| at > now) {
            return None;
        }
        let due: Vec<Candidate> = self
            .pending
            .take_due(now)
            .into_iter()
            .filter(|c| still_due(c, registry, attention))
            .collect();
        if due.is_empty() || self.looking(now) {
            return None;
        }
        let (title, body) = digest(&due)?;
        self.last_sent = Some(now);
        Some(sequence(self.alert, &title, &body, self.mux))
    }
}

/// The bytes that alert the terminal: a bell, or an OSC 9 / OSC 777 notification with
/// `title` and `body` (free of control characters, [`twr_core::notify::clean`]), wrapped for
/// `mux`'s passthrough (tmux passes a bell on by itself); a bell instead when tmux and screen
/// are nested. Empty for `off`.
pub fn sequence(alert: TerminalAlert, title: &str, body: &str, mux: Multiplexer) -> String {
    // OSC 777 separates its fields with `;`; OSC 9 reads `9;4;...` as a progress report.
    let title = title.replace(';', ",");
    let body = body.replace(';', ",");
    let osc = |body: &str| match alert {
        TerminalAlert::Off => String::new(),
        TerminalAlert::Bell => "\x07".into(),
        TerminalAlert::Osc9 => format!("\x1b]9;{title}: {body}\x07"),
        TerminalAlert::Osc777 => format!("\x1b]777;notify;{title};{body}\x07"),
    };
    match (alert, mux) {
        (TerminalAlert::Off | TerminalAlert::Bell, _) | (_, Multiplexer::None) => osc(&body),
        (_, Multiplexer::Nested) => "\x07".into(),
        (_, Multiplexer::Tmux) => {
            format!("\x1bPtmux;{}\x1b\\", osc(&body).replace('\x1b', "\x1b\x1b"))
        }
        // A longer string would show in screen's window: the body gives way.
        (_, Multiplexer::Screen) => {
            let room = SCREEN_STRING_MAX.saturating_sub(osc("").len());
            format!("\x1bP{}\x1b\\", osc(&cut_bytes(&body, room)))
        }
    }
}

/// `s` cut to at most `max` bytes, on a character boundary, with an ellipsis when cut.
fn cut_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for ch in s.chars() {
        if out.len() + ch.len_utf8() + '…'.len_utf8() > max {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use twr_core::event::{AttentionReason, EventBody, Grade, TurnResult};
    use twr_core::notify::NotifyEvent;
    use twr_core::testkit::ThreadScript;

    struct World {
        registry: Registry,
        attention: AttentionStack,
        gseq: u64,
        n: Notifier,
    }

    impl World {
        fn new(alert: TerminalAlert, when: NotifyWhen) -> Self {
            Self::with_events(alert, when, NotifyEvent::DEFAULTS)
        }

        fn with_events(alert: TerminalAlert, when: NotifyWhen, events: &[NotifyEvent]) -> Self {
            let cfg = TuiNotifyConfig {
                terminal: alert,
                when,
                events: events.to_vec(),
            };
            World {
                registry: Registry::new(),
                attention: AttentionStack::new(),
                gseq: 0,
                n: Notifier::new(&cfg, Multiplexer::None),
            }
        }

        fn push(&mut self, ev: Event, now: Instant) {
            self.gseq += 1;
            self.registry.apply(&ev);
            self.attention.apply_at(&ev, Some(self.gseq));
            self.n
                .observe(&ev, Some(self.gseq), &self.registry, &self.attention, now);
        }

        fn flush(&mut self, now: Instant) -> Option<String> {
            self.n.flush(&self.registry, &self.attention, now)
        }
    }

    fn secs(t0: Instant, s: f64) -> Instant {
        t0 + Duration::from_secs_f64(s)
    }

    /// An interim report holds nothing, so its rewrite once the tasks run on sends nothing.
    #[test]
    fn an_interim_reports_rewrite_sends_nothing() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Osc9, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(api.turn_ended(TurnResult::Completed), t0);
        let mut interim = api.report(Some(1), Grade::Success);
        if let EventBody::ReportReady(p) = &mut interim.body {
            p.ext = serde_json::json!({"twr": {"background_tasks": 1}})
                .as_object()
                .cloned();
        }
        w.push(interim, t0);
        w.push(api.report(Some(1), Grade::Success), secs(t0, 1.0));
        assert_eq!(w.n.next_due(), None);
        assert_eq!(w.flush(secs(t0, 20.0)), None);
    }

    /// Alerts turned on during an orchestrator turn the user started (the settings) still
    /// say its reply: the turn was followed while they were off.
    #[test]
    fn alerts_turned_on_midway_know_the_turn_running() {
        use twr_core::event::{Source, ThreadCreated};
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Off, NotifyWhen::Always);
        let mut o = ThreadScript::new("o1");
        w.push(
            o.event(EventBody::ThreadCreated(ThreadCreated {
                name: "orch".into(),
                ext: serde_json::json!({"twr": {"role": "orchestrator"}})
                    .as_object()
                    .cloned(),
                ..Default::default()
            })),
            t0,
        );
        let mut said = o.turn_started("what is api doing?");
        said.source = Source::User("tui".into());
        w.push(said, t0);
        w.n.set_alert(TerminalAlert::Bell);
        w.push(o.turn_ended(TurnResult::Completed), t0);
        assert!(w.flush(secs(t0, 60.0)).is_some());
    }

    #[test]
    fn off_holds_nothing() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Off, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.raise("p1", AttentionReason::Permission), t0);
        assert!(!w.n.enabled());
        assert_eq!(w.n.next_due(), None);
        assert_eq!(w.flush(secs(t0, 60.0)), None);
    }

    #[test]
    fn a_permission_rings_after_its_hold_unless_answered() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Bell, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(api.raise("p1", AttentionReason::Permission), t0);
        assert_eq!(w.n.next_due(), Some(secs(t0, 2.0)));
        assert_eq!(w.flush(secs(t0, 1.0)), None, "not due yet");
        assert_eq!(w.flush(secs(t0, 2.0)).as_deref(), Some("\x07"));
        assert_eq!(w.n.next_due(), None);

        // Answered within its hold: nothing.
        w.push(api.raise("p2", AttentionReason::Permission), secs(t0, 10.0));
        w.push(api.resolve("p2"), secs(t0, 11.0));
        assert_eq!(w.flush(secs(t0, 12.0)), None);
        assert_eq!(w.n.next_due(), None, "dropped, not kept");
    }

    #[test]
    fn what_is_due_together_goes_out_once_and_alerts_keep_a_gap() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Osc777, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        let mut web = ThreadScript::new("t2");
        w.push(api.created("api"), t0);
        w.push(web.created("web"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(web.turn_started("go"), t0);
        w.push(api.raise("p1", AttentionReason::Permission), t0);
        w.push(web.raise("q1", AttentionReason::Question), t0);
        assert_eq!(
            w.flush(secs(t0, 2.0)).as_deref(),
            Some("\x1b]777;notify;twr: 2 need you;api needs permission, web asks\x07")
        );
        // A report a second later waits for the gap.
        w.push(web.turn_ended(TurnResult::Completed), secs(t0, 3.0));
        w.push(web.report(Some(1), Grade::Success), secs(t0, 3.0));
        assert_eq!(w.n.next_due(), Some(secs(t0, 7.0)));
        assert_eq!(w.flush(secs(t0, 5.0)), None);
        assert_eq!(
            w.flush(secs(t0, 7.0)).as_deref(),
            Some("\x1b]777;notify;twr: web;web finished: Did the thing.\x07")
        );
    }

    #[test]
    fn a_review_raised_during_the_hold_replaces_the_finished_it_regrades() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Osc777, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(api.turn_ended(TurnResult::Completed), t0);
        w.push(api.report(Some(1), Grade::Success), t0);
        // `report.write` regrades the turn a second later, and raises its review.
        w.push(api.report(Some(1), Grade::NeedsVerification), secs(t0, 1.0));
        let mut review = api.raise("r1", AttentionReason::Review);
        review.turn = Some(1);
        w.push(review, secs(t0, 1.0));
        assert_eq!(w.n.next_due(), Some(secs(t0, 3.0)));
        assert_eq!(w.flush(secs(t0, 2.0)), None);
        assert_eq!(
            w.flush(secs(t0, 3.0)).as_deref(),
            Some("\x1b]777;notify;twr: api;api needs review: Review r1\x07")
        );
        assert_eq!(w.n.next_due(), None);
    }

    #[test]
    fn a_held_report_says_its_latest_revision_and_a_delivered_one_stays_quiet() {
        let t0 = Instant::now();
        let mut w = World::with_events(
            TerminalAlert::Osc9,
            NotifyWhen::Always,
            &[NotifyEvent::Report],
        );
        let mut api = ThreadScript::new("t1");
        let report = |api: &mut ThreadScript, grade, summary: &str| {
            let mut ev = api.report(Some(1), grade);
            if let EventBody::ReportReady(p) = &mut ev.body {
                p.summary = summary.into();
            }
            ev
        };
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(api.turn_ended(TurnResult::Completed), t0);
        let ev = report(&mut api, Grade::Success, "All tests passed.");
        w.push(ev, t0);
        // `report.write` regrades it a second later; reviews are off.
        let ev = report(&mut api, Grade::GaveUp, "Verification failed.");
        w.push(ev, secs(t0, 1.0));
        assert_eq!(w.n.next_due(), Some(secs(t0, 2.0)), "its deadline holds");
        assert_eq!(
            w.flush(secs(t0, 2.0)).as_deref(),
            Some("\x1b]9;twr: api: api finished: gave up: Verification failed.\x07")
        );
        // Delivered: a later rewrite says nothing.
        let ev = report(&mut api, Grade::Success, "Fixed it.");
        w.push(ev, secs(t0, 3.0));
        assert_eq!(w.n.next_due(), None);
        assert_eq!(w.flush(secs(t0, 20.0)), None);
    }

    #[test]
    fn nothing_held_is_sent_once_the_control_stream_is_lost() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Bell, NotifyWhen::Always);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);
        w.push(api.raise("p1", AttentionReason::Permission), t0);
        // Another client may answer it now: this TUI would never know.
        w.n.lost();
        assert_eq!(w.n.next_due(), None);
        assert_eq!(w.flush(secs(t0, 2.0)), None);
    }

    #[test]
    fn focus_decides_in_unfocused_mode() {
        let t0 = Instant::now();
        let mut w = World::new(TerminalAlert::Bell, NotifyWhen::Unfocused);
        let mut api = ThreadScript::new("t1");
        w.push(api.created("api"), t0);
        w.push(api.turn_started("go"), t0);

        // Focus unknown and no input: rings.
        w.push(api.raise("p1", AttentionReason::Permission), t0);
        assert!(w.flush(secs(t0, 2.0)).is_some());
        // Focus unknown, but the user just typed: they are looking.
        w.n.input(secs(t0, 20.0));
        w.push(api.raise("p2", AttentionReason::Permission), secs(t0, 20.0));
        assert_eq!(w.flush(secs(t0, 22.0)), None);
        // Focused: never.
        w.n.focus(true);
        w.push(
            api.raise("p3", AttentionReason::Permission),
            secs(t0, 100.0),
        );
        assert_eq!(w.flush(secs(t0, 102.0)), None);
        // Unfocused, even right after typing.
        w.n.input(secs(t0, 200.0));
        w.n.focus(false);
        w.push(
            api.raise("p4", AttentionReason::Permission),
            secs(t0, 200.0),
        );
        assert!(w.flush(secs(t0, 202.0)).is_some());
    }

    #[test]
    fn another_workspaces_waiting_items_alert_once_unless_looked_at() {
        let t0 = Instant::now();
        let cfg = TuiNotifyConfig {
            terminal: TerminalAlert::Osc9,
            when: NotifyWhen::Unfocused,
            events: NotifyEvent::DEFAULTS.to_vec(),
        };
        let mut n = Notifier::new(&cfg, Multiplexer::None);
        n.focus(false);
        assert_eq!(
            n.workspace_waiting("a", 2, t0),
            WorkspaceAlert::Send("\x1b]9;twr: workspace a: 2 items waiting on you\x07".into())
        );
        // Within the gap: later.
        assert_eq!(
            n.workspace_waiting("b", 1, secs(t0, 1.0)),
            WorkspaceAlert::Later
        );
        // A renewed notifier keeps the focus and the gap.
        let mut r = n.renewed();
        assert_eq!(
            r.workspace_waiting("b", 1, secs(t0, 2.0)),
            WorkspaceAlert::Later
        );
        assert!(matches!(
            r.workspace_waiting("b", 1, secs(t0, 6.0)),
            WorkspaceAlert::Send(_)
        ));
        // Looking at the TUI, or without permissions and questions: seen.
        r.focus(true);
        assert_eq!(
            r.workspace_waiting("b", 3, secs(t0, 20.0)),
            WorkspaceAlert::Seen
        );
        let quiet = TuiNotifyConfig {
            events: vec![NotifyEvent::Report],
            ..cfg
        };
        let mut q = Notifier::new(&quiet, Multiplexer::None);
        q.focus(false);
        assert_eq!(q.workspace_waiting("a", 1, t0), WorkspaceAlert::Seen);
    }

    #[test]
    fn sequences_carry_no_field_separator_and_wrap_for_a_multiplexer() {
        use Multiplexer::{Screen, Tmux};
        let none = Multiplexer::None;
        assert_eq!(sequence(TerminalAlert::Off, "t", "b", Tmux), "");
        assert_eq!(sequence(TerminalAlert::Bell, "t", "b", Tmux), "\x07");
        assert_eq!(sequence(TerminalAlert::Bell, "t", "b", Screen), "\x07");
        assert_eq!(
            sequence(TerminalAlert::Osc9, "twr: api", "a;b", none),
            "\x1b]9;twr: api: a,b\x07"
        );
        assert_eq!(
            sequence(TerminalAlert::Osc777, "x;y", "b", none),
            "\x1b]777;notify;x,y;b\x07"
        );
        assert_eq!(
            sequence(TerminalAlert::Osc9, "t", "b", Tmux),
            "\x1bPtmux;\x1b\x1b]9;t: b\x07\x1b\\"
        );
        assert_eq!(
            sequence(TerminalAlert::Osc9, "t", "b", Screen),
            "\x1bP\x1b]9;t: b\x07\x1b\\"
        );
        assert_eq!(
            sequence(TerminalAlert::Osc777, "t", "b", Screen),
            "\x1bP\x1b]777;notify;t;b\x07\x1b\\"
        );
        // Nested tmux and screen: the bell, for either order.
        assert_eq!(Multiplexer::named(true, true), Multiplexer::Nested);
        assert_eq!(Multiplexer::named(true, false), Tmux);
        assert_eq!(Multiplexer::named(false, true), Screen);
        assert_eq!(Multiplexer::named(false, false), none);
        let nested = Multiplexer::Nested;
        assert_eq!(sequence(TerminalAlert::Osc9, "t", "b", nested), "\x07");
        assert_eq!(sequence(TerminalAlert::Osc777, "t", "b", nested), "\x07");
        assert_eq!(sequence(TerminalAlert::Off, "t", "b", nested), "");
        // Screen passes on at most 767 bytes: a long body is cut to fit.
        let body = "é".repeat(500);
        let out = sequence(TerminalAlert::Osc9, "t", &body, Screen);
        let inner = &out["\x1bP".len()..out.len() - "\x1b\\".len()];
        assert!(inner.len() <= SCREEN_STRING_MAX, "{}", inner.len());
        assert!(inner.ends_with("é…\x07"));
    }

    #[test]
    fn attendance_is_told_when_it_changes_and_renewed() {
        let cfg = TuiNotifyConfig::default();
        let mut n = Notifier::new(&cfg, Multiplexer::None);
        let mut a = Attendance::default();
        let t0 = Instant::now();
        // Nothing typed: not attended, nothing to say.
        assert_eq!(a.step(n.attended(t0), t0), None);
        assert_eq!(a.next_check(&n), None);
        n.input(t0);
        assert_eq!(a.step(n.attended(t0), t0), Some(true));
        // Again within LOOKING_EVERY: nothing; after it: said again.
        let t1 = t0 + Duration::from_secs(3);
        assert_eq!(a.step(n.attended(t1), t1), None);
        assert_eq!(a.next_check(&n), Some(t0 + LOOKING_EVERY));
        let t2 = t0 + LOOKING_EVERY;
        assert_eq!(a.step(n.attended(t2), t2), Some(true));
        // The attention lapses 30 s after the last input.
        assert_eq!(
            a.next_check(&n),
            Some((t2 + LOOKING_EVERY).min(t0 + INPUT_FOCUS))
        );
        let t3 = t0 + INPUT_FOCUS;
        assert_eq!(a.step(n.attended(t3), t3), Some(false));
        assert!(!a.told());
        // Focus lost: at once.
        n.input(t3);
        assert_eq!(a.step(n.attended(t3), t3), Some(true));
        n.focus(false);
        assert_eq!(a.step(n.attended(t3), t3), Some(false));
    }
}

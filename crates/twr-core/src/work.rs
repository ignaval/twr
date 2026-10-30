//! What a thread has in flight, and how much of it twr can see: the `work` object of
//! `thread.get`, `thread.list` and `thread.activity`, which the hub builds from the log when
//! asked (`hub::work`), the CLI (`twr thread show`) and the TUI draw, the orchestrator reads, and
//! the hub's hung-turn check judges by (`hub::stall`). One reading for all of them.
//!
//! It says, as of `as_of`:
//! - the open turn and the tool calls it waits on ([`Call`]: started, no result yet), with the
//!   task a call runs in the foreground of;
//! - the thread's tasks ([`Task`]): its runtime's own background work, foreground tasks (part
//!   of a call), tasks moved to the background after their call returned (`detached`: nothing
//!   waits for them; a waiter that outlived its work looks like this), and the hub's jobs for it;
//! - a compaction under way, the prompts queued behind the turn ([`Queued`]), and the latest
//!   background task that ended in the turn ([`Ended`]);
//! - when the runtime last made progress (`progress_at`: its words, tool calls and results, a
//!   task's start, end or progress) and, apart from that, when it was last seen alive
//!   (`alive_at`: a task's heartbeat, which only says a process is there, never that it moves);
//! - how much twr can see of its runtime ([`Sight`]), and its reading ([`State`]).
//!
//! The reading never infers failure from silence alone, nor activity from heartbeats: a quiet
//! turn is `waiting_on_work` when background work of its own runs (its lifecycle as the runtime
//! reported it), `waiting_on_call` when only a call is open, `nothing_in_flight` when nothing
//! is, and `unknown` when twr cannot see its tools. No reasoning of the agent's is in it: only
//! what its tools and tasks are, as the runtime named them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A turn whose runtime made progress this recently is `working` ([`State::Working`]), in
/// seconds.
pub const QUIET_AFTER_SECS: i64 = 60;

/// Most calls and tasks a [`Work`] lists ([`Work::more`] counts the rest).
pub const LISTED_MAX: usize = 8;

/// What a thread has in flight (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Work {
    /// When the hub read it: every time in it is as of then.
    pub as_of: DateTime<Utc>,
    /// The hub's reading.
    pub state: State,
    /// How much twr sees of its runtime.
    pub sight: Sight,
    /// The open turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<u64>,
    /// When the open turn started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_started_at: Option<DateTime<Utc>>,
    /// The tool calls the open turn waits on, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calls: Vec<Call>,
    /// Its tasks and jobs, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<Task>,
    /// Calls and tasks left out of the lists ([`LISTED_MAX`] each).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub more: usize,
    /// Since when its runtime compacts its context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting_since: Option<DateTime<Utc>>,
    /// The latest background task of its runtime that ended in the open turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<Ended>,
    /// The prompts queued for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<Queued>,
    /// When its runtime last made progress (see the module docs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_at: Option<DateTime<Utc>>,
    /// When it was last seen alive, when that is later than `progress_at`: a heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alive_at: Option<DateTime<Utc>>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The hub's reading of a thread's work ([`Work::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// No open turn and nothing of its own running.
    Idle,
    /// Waiting on the user: a permission, a question.
    WaitingForYou,
    /// No open turn; tasks or jobs of its own run.
    Background,
    /// An open turn whose runtime made progress in the last [`QUIET_AFTER_SECS`].
    Working,
    /// Its runtime compacts its context.
    Compacting,
    /// A quiet turn while background work of its own runs (a task the runtime started in the
    /// background, a job): reviews and tests are often this quiet.
    WaitingOnWork,
    /// A quiet turn waiting on its open tool calls, with no background work of its own
    /// running: a long command, or a call blocked on something that ended or never comes.
    WaitingOnCall,
    /// A quiet open turn with no call open and nothing of its own running: its model may still
    /// be writing (a call shows once it is whole), or the turn was left open.
    NothingInFlight,
    /// twr cannot see what it does: a terminal runtime, a thread driven in a terminal, or a
    /// box not connected (what the log shows may be behind).
    Unknown,
}

impl State {
    /// Its name as serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::WaitingForYou => "waiting_for_you",
            State::Background => "background",
            State::Working => "working",
            State::Compacting => "compacting",
            State::WaitingOnWork => "waiting_on_work",
            State::WaitingOnCall => "waiting_on_call",
            State::NothingInFlight => "nothing_in_flight",
            State::Unknown => "unknown",
        }
    }

    /// Whether it is a quiet open turn ([`State::WaitingOnWork`], [`State::WaitingOnCall`],
    /// [`State::NothingInFlight`]).
    pub fn quiet(self) -> bool {
        matches!(
            self,
            State::WaitingOnWork | State::WaitingOnCall | State::NothingInFlight
        )
    }
}

/// How much twr sees of a thread's runtime ([`Work::sight`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sight {
    /// Its tool calls and tasks, and which tasks a call waits for (Claude Code).
    Full,
    /// Its tool calls and tasks, not which task a call waits for (Codex, ACP agents).
    Calls,
    /// None of its tools: a terminal runtime, or a thread driven in a terminal.
    None,
    /// Its box is not connected: what the log shows may be behind.
    Stale,
}

/// An open tool call ([`Work::calls`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Call {
    /// The runtime's id of the call.
    pub id: String,
    /// Its tool.
    pub tool: String,
    /// What it does, as the agent named it or its input says (`Wait for round 3 answer`).
    pub what: String,
    /// When it started.
    pub since: DateTime<Utc>,
    /// The task it runs in the foreground of, when the runtime said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
}

/// What kind of work a [`Task`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskKind {
    /// Its runtime's background work.
    Background,
    /// Part of an open tool call, which waits for it.
    Foreground,
    /// Started in the foreground of a call that returned while it ran: nothing waits for it.
    Detached,
    /// A job the hub runs for the thread.
    Job,
}

/// A task or job of the thread ([`Work::tasks`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Its id.
    pub id: String,
    /// What it does, as the runtime (or the job) described it.
    pub what: String,
    /// Its kind.
    pub kind: TaskKind,
    /// When it started, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// An open call blocks on it (a detached task waited for again: Claude Code's
    /// `TaskOutput`): it counts as running work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub awaited: bool,
    /// Its latest start, end or progress the hub recorded, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress_at: Option<DateTime<Utc>>,
    /// Its latest update of any kind, heartbeats too.
    pub alive_at: DateTime<Utc>,
}

/// The latest background task that ended in the open turn ([`Work::ended`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    /// Its id.
    pub id: String,
    /// What it did.
    pub what: String,
    /// When it ended.
    pub at: DateTime<Utc>,
}

/// The prompts queued for a thread ([`Work::queued`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queued {
    /// How many.
    pub count: usize,
    /// When the oldest arrived.
    pub oldest_at: DateTime<Utc>,
    /// Whose the oldest is: `user`, `orchestrator`, `job` (a job's end) or `hub`.
    pub from: String,
}

impl Work {
    /// An empty reading as of `now`: idle, fully seen.
    pub fn new(now: DateTime<Utc>) -> Self {
        Work {
            as_of: now,
            state: State::Idle,
            sight: Sight::Full,
            turn: None,
            turn_started_at: None,
            calls: Vec::new(),
            tasks: Vec::new(),
            more: 0,
            compacting_since: None,
            ended: None,
            queued: None,
            progress_at: None,
            alive_at: None,
        }
    }

    /// The background work of its own that runs: its runtime's background tasks, its jobs and
    /// detached tasks an open call waits for again (not foreground tasks, which their call
    /// stands for, nor detached ones nothing waits for).
    pub fn running_work(&self) -> impl Iterator<Item = &Task> {
        self.tasks.iter().filter(|t| {
            matches!(t.kind, TaskKind::Background | TaskKind::Job)
                || (t.kind == TaskKind::Detached && t.awaited)
        })
    }

    /// The reading of what it has in flight, for a thread waiting on the user (`waiting`) or
    /// not: see [`State`]. [`Work::reading`], with a turn that made progress in the last
    /// [`QUIET_AFTER_SECS`] read as [`State::Working`].
    pub fn assess(&self, waiting: bool) -> State {
        let reading = self.reading(waiting);
        if reading.quiet() && self.quiet_secs().is_none_or(|s| s < QUIET_AFTER_SECS) {
            State::Working
        } else {
            reading
        }
    }

    /// What it has in flight, whatever how long it has been quiet: what [`Work::assess`] says
    /// once the turn is quiet (never [`State::Working`]). The hung-turn check judges by it,
    /// with its own thresholds.
    pub fn reading(&self, waiting: bool) -> State {
        if matches!(self.sight, Sight::None | Sight::Stale) {
            return if waiting {
                State::WaitingForYou
            } else if self.turn.is_none() && self.tasks.is_empty() {
                State::Idle
            } else {
                State::Unknown
            };
        }
        if waiting {
            return State::WaitingForYou;
        }
        if self.turn.is_none() {
            return if self.tasks.is_empty() {
                State::Idle
            } else {
                State::Background
            };
        }
        if self.compacting_since.is_some() {
            return State::Compacting;
        }
        if self.running_work().next().is_some() {
            State::WaitingOnWork
        } else if !self.calls.is_empty() {
            State::WaitingOnCall
        } else {
            State::NothingInFlight
        }
    }

    /// How long its runtime has made no progress (counted from the open turn's start when it
    /// made none in it), `None` when unknown.
    pub fn quiet(&self) -> Option<chrono::Duration> {
        let since = match (self.progress_at, self.turn_started_at) {
            (Some(p), Some(t)) => Some(p.max(t)),
            (p, t) => p.or(t),
        }?;
        Some((self.as_of - since).max(chrono::Duration::zero()))
    }

    /// [`Work::quiet`] in whole seconds.
    pub fn quiet_secs(&self) -> Option<i64> {
        self.quiet().map(|q| q.num_seconds())
    }

    /// Seconds from `t` to [`Work::as_of`].
    fn age(&self, t: DateTime<Utc>) -> String {
        ago((self.as_of - t).num_seconds())
    }

    /// One line on what it has in flight, every time relative to [`Work::as_of`] (`quiet 5m ·
    /// Bash "Wait for round 3 answer" 5m · background: Re-review (round 3) 6m · 1 detached ·
    /// 1 queued 12m`). Empty when idle with nothing queued.
    pub fn line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let lead = match self.state {
            State::Working => Some("working".to_string()),
            State::Compacting => Some(match self.compacting_since {
                Some(t) => format!("compacting {}", self.age(t)),
                None => "compacting".into(),
            }),
            State::Unknown => Some(match self.sight {
                Sight::Stale => "box not connected: may be behind".into(),
                _ => "twr cannot see its tools".into(),
            }),
            s if s.quiet() => self.quiet_secs().map(|q| format!("quiet {}", ago(q))),
            _ => None,
        };
        parts.extend(lead);
        parts.extend(self.in_flight());
        if self.state.quiet() && self.running_work().next().is_none() {
            if let Some(e) = &self.ended {
                parts.push(format!("last background work ended {} ago", self.age(e.at)));
            }
        }
        if let Some(a) = self.alive_at.filter(|_| self.state.quiet()) {
            parts.push(format!("heartbeat {} ago", self.age(a)));
        }
        if let Some(q) = &self.queued {
            parts.push(format!("{} queued {}", q.count, self.age(q.oldest_at)));
        }
        parts.join(" · ")
    }

    /// What it has in flight, a part each, every time relative to [`Work::as_of`]: its open
    /// calls (`Bash "Wait for round 3 answer" 5m`), its running background work and jobs
    /// (`background: Re-review (round 3) 6m`), how many tasks run detached, and how many more
    /// were not listed.
    pub fn in_flight(&self) -> Vec<String> {
        let mut parts = Vec::new();
        for c in &self.calls {
            parts.push(format!("{} {}", call_name(c), self.age(c.since)));
        }
        for t in self.running_work() {
            let kind = match t.kind {
                TaskKind::Job => "job",
                TaskKind::Detached => "detached, waited for",
                _ => "background",
            };
            parts.push(match t.since {
                Some(s) => format!("{kind}: {} {}", short(&t.what), self.age(s)),
                None => format!("{kind}: {}", short(&t.what)),
            });
        }
        let detached = self
            .tasks
            .iter()
            .filter(|t| t.kind == TaskKind::Detached && !t.awaited)
            .count();
        if detached > 0 {
            parts.push(format!("{detached} detached"));
        }
        if self.more > 0 {
            parts.push(format!("{} more", self.more));
        }
        parts
    }

    /// What it has in flight, a line each, for `twr thread show`: the reading, the open turn,
    /// each call and task, what ended last, the queue, progress and liveness.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![format!(
            "state: {}{}",
            self.state.as_str(),
            match self.sight {
                Sight::Full => "",
                Sight::Calls => " (tool calls seen; not which task a call waits for)",
                Sight::None => " (twr cannot see this runtime's tools)",
                Sight::Stale => " (its box is not connected: this may be behind)",
            }
        )];
        if let Some(turn) = self.turn {
            out.push(match self.turn_started_at {
                Some(t) => format!("turn {turn}, started {} ago", self.age(t)),
                None => format!("turn {turn}"),
            });
        }
        if let Some(t) = self.compacting_since {
            out.push(format!("compacting its context for {}", self.age(t)));
        }
        for c in &self.calls {
            out.push(format!(
                "call: {} open {}{}",
                call_name(c),
                self.age(c.since),
                c.task
                    .as_deref()
                    .map_or(String::new(), |t| format!(" (task {t})"))
            ));
        }
        for t in &self.tasks {
            let kind = match t.kind {
                TaskKind::Background => "background",
                TaskKind::Foreground => "foreground",
                TaskKind::Detached if t.awaited => "detached, waited for again by an open call",
                TaskKind::Detached => "detached (its call returned; nothing waits for it)",
                TaskKind::Job => "job",
            };
            let mut line = format!("{kind}: {} [{}]", short(&t.what), t.id);
            if let Some(s) = t.since {
                line.push_str(&format!(", running {}", self.age(s)));
            }
            match t.progress_at {
                Some(p) if p < t.alive_at => line.push_str(&format!(
                    ", last progress {} ago, heartbeat {} ago",
                    self.age(p),
                    self.age(t.alive_at)
                )),
                Some(p) => line.push_str(&format!(", last update {} ago", self.age(p))),
                None => line.push_str(&format!(", last update {} ago", self.age(t.alive_at))),
            }
            out.push(line);
        }
        if self.more > 0 {
            out.push(format!("{} more not listed", self.more));
        }
        if let Some(e) = &self.ended {
            out.push(format!(
                "last background task to end: {} [{}], {} ago",
                short(&e.what),
                e.id,
                self.age(e.at)
            ));
        }
        if let Some(q) = &self.queued {
            out.push(format!(
                "queued: {} prompt(s), the oldest ({}) waiting {}",
                q.count,
                q.from,
                self.age(q.oldest_at)
            ));
        }
        out.push(match self.progress_at {
            Some(p) => format!("last progress: {} ago", self.age(p)),
            None => "last progress: none seen".into(),
        });
        if let Some(a) = self.alive_at {
            out.push(format!(
                "last heartbeat: {} ago (a process is there; no progress)",
                self.age(a)
            ));
        }
        out.push(format!("as of {}", self.as_of.format("%H:%M:%S UTC")));
        out
    }
}

/// `Bash "Wait for round 3 answer"`: a call's tool and what it does.
fn call_name(c: &Call) -> String {
    let what = short(&c.what);
    if what.is_empty() || what == c.tool {
        c.tool.clone()
    } else {
        format!("{} \"{}\"", c.tool, what)
    }
}

/// `s` on one line, cut to 60 characters.
fn short(s: &str) -> String {
    crate::activity::cut(s, 60)
}

/// `secs` as `45s`, `5m`, `1h20m`.
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s % 3600 / 60 == 0 {
        format!("{}h", s / 3600)
    } else {
        format!("{}h{}m", s / 3600, s % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-28T22:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn at(secs: i64) -> DateTime<Utc> {
        t0() + chrono::Duration::seconds(secs)
    }

    fn task(id: &str, what: &str, kind: TaskKind, since: i64) -> Task {
        Task {
            id: id.into(),
            what: what.into(),
            kind,
            since: Some(at(since)),
            awaited: false,
            progress_at: Some(at(since)),
            alive_at: at(since),
        }
    }

    fn call(id: &str, what: &str, since: i64, task: Option<&str>) -> Call {
        Call {
            id: id.into(),
            tool: "Bash".into(),
            what: what.into(),
            since: at(since),
            task: task.map(Into::into),
        }
    }

    /// A turn open since 0, as of `now`, with its last progress at `progress`.
    fn turn(now: i64, progress: i64) -> Work {
        let mut w = Work::new(at(now));
        w.turn = Some(15);
        w.turn_started_at = Some(at(0));
        w.progress_at = Some(at(progress));
        w
    }

    #[test]
    fn ages() {
        assert_eq!(ago(-3), "0s");
        assert_eq!(ago(45), "45s");
        assert_eq!(ago(300), "5m");
        assert_eq!(ago(3600), "1h");
        assert_eq!(ago(4800), "1h20m");
    }

    /// The incident's first card (turn 15, 22:12:53): a review started in the background and a
    /// foreground waiter on its answer, 5 min quiet. Waiting on work, not hung.
    #[test]
    fn a_waiter_on_a_running_review_waits_on_work() {
        let mut w = turn(305, 0);
        w.calls = vec![call("c1", "Wait for round 3 answer", 2, Some("bsv"))];
        w.tasks = vec![
            task("bbk", "Wait for TUI tests", TaskKind::Detached, -980),
            task(
                "bu3",
                "Re-review at xhigh (round 3)",
                TaskKind::Background,
                0,
            ),
            task("bsv", "Wait for round 3 answer", TaskKind::Foreground, 2),
        ];
        w.queued = Some(Queued {
            count: 1,
            oldest_at: at(-600),
            from: "orchestrator".into(),
        });
        let s = w.assess(false);
        assert_eq!(s, State::WaitingOnWork);
        w.state = s;
        assert_eq!(
            w.line(),
            "quiet 5m · Bash \"Wait for round 3 answer\" 5m · background: Re-review at xhigh \
             (round 3) 5m · 1 detached · 1 queued 15m"
        );
    }

    /// The incident's second card (22:23:53): the tests the waiter waited for ended 5 min ago;
    /// the waiter (a `pgrep -f` loop that matched itself) is still open. Waiting on a call,
    /// with what ended last said.
    #[test]
    fn a_waiter_that_outlived_its_work_waits_on_a_call() {
        let mut w = turn(494, 166);
        w.calls = vec![call("c2", "Wait for TUI tests", 0, Some("b1y"))];
        w.tasks = vec![
            task("bbk", "Wait for TUI tests", TaskKind::Detached, -1146),
            task("b1y", "Wait for TUI tests", TaskKind::Foreground, 4),
        ];
        w.ended = Some(Ended {
            id: "b9p".into(),
            what: "Apply test edit; run TUI tests".into(),
            at: at(166),
        });
        let s = w.assess(false);
        assert_eq!(s, State::WaitingOnCall);
        w.state = s;
        assert_eq!(
            w.line(),
            "quiet 5m · Bash \"Wait for TUI tests\" 8m · 1 detached · last background work \
             ended 5m ago"
        );
    }

    /// Nothing open and nothing running: the turn may have been left open (or its model still
    /// writes). Heartbeats of a task alone never make it working.
    #[test]
    fn nothing_in_flight_and_heartbeats_are_no_progress() {
        let mut w = turn(400, 10);
        assert_eq!(w.assess(false), State::NothingInFlight);
        // A heartbeat is liveness, not progress.
        w.alive_at = Some(at(390));
        assert_eq!(w.assess(false), State::NothingInFlight);
        w.state = State::NothingInFlight;
        assert_eq!(w.line(), "quiet 6m · heartbeat 10s ago");
        // Progress a moment ago: working, though what it has in flight reads the same.
        w.progress_at = Some(at(380));
        assert_eq!(w.assess(false), State::Working);
        assert_eq!(w.reading(false), State::NothingInFlight);
        // A turn with no progress yet counts from its start.
        w.progress_at = None;
        assert_eq!(w.quiet_secs(), Some(400));
    }

    /// A detached task an open call waits for again (Claude Code's `TaskOutput` after the test
    /// run's Bash call timed out) is work it waits on, not a leftover.
    #[test]
    fn a_detached_task_waited_for_again_is_running_work() {
        let mut w = turn(400, 10);
        w.calls = vec![Call {
            tool: "TaskOutput".into(),
            ..call("c3", "", 300, Some("tests"))
        }];
        w.tasks = vec![task("tests", "Run the TUI tests", TaskKind::Detached, 0)];
        assert_eq!(w.assess(false), State::WaitingOnCall);
        w.tasks[0].awaited = true;
        assert_eq!(w.assess(false), State::WaitingOnWork);
        w.state = State::WaitingOnWork;
        assert_eq!(
            w.line(),
            "quiet 6m · TaskOutput 1m · detached, waited for: Run the TUI tests 6m"
        );
    }

    /// Jobs are work it waits on; a compaction is its own state; the user's items come first;
    /// a runtime twr cannot see is unknown, never hung or working.
    #[test]
    fn jobs_compactions_requests_and_blind_runtimes() {
        let mut w = turn(400, 10);
        w.tasks = vec![task("job:1", "gates", TaskKind::Job, 0)];
        assert_eq!(w.assess(false), State::WaitingOnWork);
        w.compacting_since = Some(at(100));
        assert_eq!(w.assess(false), State::Compacting);
        assert_eq!(w.assess(true), State::WaitingForYou);
        w.compacting_since = None;
        w.sight = Sight::None;
        assert_eq!(w.assess(false), State::Unknown);
        w.sight = Sight::Stale;
        w.state = State::Unknown;
        assert!(w.line().starts_with("box not connected"));
        w.turn = None;
        w.tasks.clear();
        assert_eq!(w.assess(false), State::Idle);
        w.sight = Sight::Full;
        w.tasks = vec![task("b", "build", TaskKind::Background, 0)];
        assert_eq!(w.assess(false), State::Background);
    }

    /// The detailed lines say each call and task, liveness apart from progress, and when.
    #[test]
    fn lines_say_progress_apart_from_heartbeats() {
        let mut w = turn(700, 100);
        w.calls = vec![call("c1", "Wait for round 4 answer", 100, Some("bjj"))];
        let mut review = task(
            "b8g",
            "Clippy, commit, re-review round 4",
            TaskKind::Background,
            95,
        );
        review.alive_at = at(690);
        w.tasks = vec![review];
        w.alive_at = Some(at(690));
        w.state = w.assess(false);
        let lines = w.lines();
        assert_eq!(lines[0], "state: waiting_on_work");
        assert!(
            lines.contains(&"turn 15, started 11m ago".to_string()),
            "{lines:#?}"
        );
        assert!(
            lines.contains(
                &"call: Bash \"Wait for round 4 answer\" open 10m (task bjj)".to_string()
            ),
            "{lines:#?}"
        );
        assert!(
            lines.contains(
                &"background: Clippy, commit, re-review round 4 [b8g], running 10m, last \
                  progress 10m ago, heartbeat 10s ago"
                    .to_string()
            ),
            "{lines:#?}"
        );
        assert!(lines
            .iter()
            .any(|l| l.starts_with("last heartbeat: 10s ago")));
        let json = serde_json::to_value(&w).unwrap();
        assert_eq!(json["state"], "waiting_on_work");
        assert_eq!(json["tasks"][0]["kind"], "background");
        let back: Work = serde_json::from_value(json).unwrap();
        assert_eq!(back, w);
    }
}

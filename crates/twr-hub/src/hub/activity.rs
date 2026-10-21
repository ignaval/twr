//! What each thread is doing right now, in one line, and what its agent last said
//! (`thread.activity`). Nothing here is recorded: it is a view of the log.
//!
//! Two lines say what a thread does. `doing` is its latest action as the runtime reported it
//! (`Bash: cargo test`, the line it is writing). `activity`, on every card and tile and in the
//! thread object, says it in words one level more specific than the thread's task
//! ([`Core::activity_line`]): the step its agent's plan has in progress, or what the agent
//! said it is about to do, then what its latest action does (`implementing scroll anchoring ·
//! running tests`); else what it waits on or came to (`waiting for permission: …`, `waiting on
//! background: …`, `done: …`, `failed: …`, `parked`). Its phrases are
//! [`twr_core::activity`]'s, folded from the same events, so it costs no model call.
//!
//! Only what no reducer keeps is folded here, from the thread's events (so a replay at
//! startup rebuilds it): the agent's latest action in the open turn (thinking, the line it is
//! writing, a tool call), the end of what it last said, and the turn's prompt. Everything else
//! `doing` says (waiting for you, idle, parked, stopped, crashed, attached) is read from the
//! registry's status and the open attention items when asked ([`Core::doing`]), so it can
//! never drift from what the cards show.
//!
//! A worker's permission or question carries the context it was raised in: when an adapter
//! raises one, the hub adds `ext.twr.context{said, task}` (what the agent last said in the
//! turn and the turn's prompt, both cut short) to the event before it is recorded, so the
//! user can judge the request without asking the orchestrator ([`Core::request_context`]).

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use twr_core::activity;
use twr_core::event::*;
use twr_core::reduce::{ThreadStatus, ThreadView};

use super::Core;

/// Longest `doing` line, in characters.
pub(super) const DOING_MAX: usize = 200;

/// Longest `said` text, in characters (its end is kept).
pub(super) const SAID_MAX: usize = 600;

/// Longest `task` line, in characters.
pub(super) const TASK_MAX: usize = 200;

/// Longest streamed text kept while a message is being written, in bytes (its end is kept).
const TEXT_KEEP: usize = 4096;

/// Longest start of a message kept, in bytes (what a finished turn came to is read from it).
const HEAD_KEEP: usize = 1024;

/// What the log says about one thread's agent that no reducer keeps ([`Core::track_activity`]).
#[derive(Debug, Clone, Default)]
pub(super) struct Activity {
    /// Its latest action in the open turn, in one line: `thinking`, `Bash: cargo test`, the
    /// last line of what it is writing. `None` before its first one.
    pub last: Option<String>,
    /// What its agent last said in the open (or last) turn, cut to its last [`SAID_MAX`]
    /// characters.
    pub said: Option<String>,
    /// The first line of the open (or last) turn's prompt.
    pub task: Option<String>,
    /// When the thread's agent or turn last did something.
    pub at: Option<DateTime<Utc>>,
    /// What its latest action in the open turn does ([`activity::tool_phrase`]: `running
    /// tests`; `thinking`, `writing`).
    action: Option<String>,
    /// What its agent last said it is about to do in the open turn ([`activity::intent`]).
    intent: Option<String>,
    /// The step its agent's own plan has in progress in the open turn (`ext.twr.step`).
    step: Option<String>,
    /// What its last finished turn came to ([`activity::result`] of its last message).
    result: Option<String>,
    /// The agent message being streamed (deltas joined), its end kept.
    text: String,
    /// The start of the agent's latest message, at most [`HEAD_KEEP`] bytes.
    head: String,
}

impl Activity {
    /// What the agent is at in its open turn, from its own words and actions: the step of its
    /// plan in progress or what it said it is about to do, then what its latest action does.
    fn working(&self) -> Option<String> {
        let lead = self.step.as_deref().or(self.intent.as_deref());
        match (lead, self.action.as_deref()) {
            // (`committing part 1`, not `committing part 1 · committing`.)
            (Some(l), Some(x)) if !l.starts_with(x) => Some(format!("{l} · {x}")),
            (Some(l), _) => Some(l.to_string()),
            (None, Some(x)) => Some(x.to_string()),
            (None, None) => None,
        }
    }

    /// What the message streamed so far says the agent is about to do, when it says so: the
    /// thread's intent from now on ([`activity::intent`]).
    fn note_intent(&mut self) {
        if let Some(intent) = activity::intent(&self.text) {
            self.intent = Some(intent);
        }
    }
}

/// `s` on one line (whitespace runs, line breaks included, become one space), cut to `max`
/// characters with `…`. Reads `s` only as far as the line it gives: it runs for every bit of
/// text an agent streams, and a hub start folds them all.
pub(super) fn one_line(s: &str, max: usize) -> String {
    let mut flat = String::new();
    let mut n = 0;
    for word in s.split_whitespace() {
        if n > 0 {
            flat.push(' ');
            n += 1;
        }
        flat.push_str(word);
        n += word.chars().count();
        if n > max {
            let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            return out;
        }
    }
    flat
}

/// The end of `s` on one line, at most `max` characters, with `…` before it when cut. Reads
/// `s` from its end only as far as that, as [`one_line`] does from its start.
fn tail(s: &str, max: usize) -> String {
    // The last words, the last first, until they are longer than `max`.
    let mut words = Vec::new();
    let mut n = 0;
    for word in s.split_whitespace().rev() {
        if !words.is_empty() {
            n += 1;
        }
        n += word.chars().count();
        words.push(word);
        if n > max {
            break;
        }
    }
    words.reverse();
    let flat = words.join(" ");
    if n <= max {
        return flat;
    }
    let mut out = String::from("…");
    out.extend(flat.chars().skip(n - max.saturating_sub(1)));
    out
}

/// The last line of `s` with any text on it.
fn last_line(s: &str) -> &str {
    s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Appends `add` to `s` while `s` is shorter than `max` bytes, at a char boundary.
fn push_head(s: &mut String, add: &str, max: usize) {
    for c in add.chars() {
        if s.len() + c.len_utf8() > max {
            break;
        }
        s.push(c);
    }
}

/// Keeps the end of `s` within `max` bytes, at a char boundary.
fn keep_end(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut start = s.len() - max;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    s.drain(..start);
}

impl Core {
    /// Folds `ev` into its thread's [`Activity`].
    pub(super) fn track_activity(&mut self, ev: &Event) {
        if ev.thread == HUB_THREAD {
            return;
        }
        // (A deleted thread keeps its entry, unlisted, for a restore, as its runtime
        // bookkeeping does.) An agent event of a turn that is no longer open (a late frame
        // after the hub ended it) says nothing about the open one.
        let open = self.registry.get(&ev.thread).and_then(|v| v.current_turn);
        if ev.body.is_agent() && ev.turn.is_some() && ev.turn != open {
            return;
        }
        let at = ev.ts;
        let act = |a: &mut Activity, last: String| {
            a.last = Some(one_line(&last, DOING_MAX));
            a.at = Some(at);
        };
        match &ev.body {
            EventBody::TurnStarted(p) => {
                let a = self.activity.entry(ev.thread.clone()).or_default();
                let task = one_line(first_line(&p.prompt), TASK_MAX);
                a.task = (!task.is_empty()).then_some(task);
                a.said = None;
                a.last = None;
                a.action = None;
                a.intent = None;
                a.step = None;
                a.result = None;
                a.text.clear();
                a.head.clear();
                a.at = Some(at);
            }
            EventBody::AgentThinking(_) => {
                let a = self.activity.entry(ev.thread.clone()).or_default();
                act(a, "thinking".into());
                a.action = Some("thinking".into());
            }
            EventBody::AgentText(t) => {
                let a = self.activity.entry(ev.thread.clone()).or_default();
                // A message starts with what streams after the last one ended.
                if a.text.is_empty() || !t.delta {
                    a.head.clear();
                }
                push_head(&mut a.head, &t.text, HEAD_KEEP);
                if t.delta {
                    a.text.push_str(&t.text);
                } else {
                    a.text = t.text.clone();
                }
                keep_end(&mut a.text, TEXT_KEEP);
                a.action = Some("writing".into());
                let said = tail(&a.text, SAID_MAX);
                if !said.is_empty() {
                    a.said = Some(said);
                }
                let line = last_line(&a.text);
                if !line.trim().is_empty() {
                    a.last = Some(one_line(line, DOING_MAX));
                    a.at = Some(at);
                }
                // A final message is complete: what streams next is another one.
                if t.is_final {
                    a.note_intent();
                    a.text.clear();
                }
            }
            EventBody::AgentToolCall(c) => {
                let a = self.activity.entry(ev.thread.clone()).or_default();
                // What it says next is a new message.
                a.note_intent();
                a.text.clear();
                a.action = Some(activity::tool_phrase(
                    &c.tool,
                    &c.input_summary,
                    twr_str(c.ext.as_ref(), PURPOSE_KEY),
                ));
                // A step begun or done: what the agent said before is about the last one.
                if let Some(step) = twr_str(c.ext.as_ref(), STEP_KEY) {
                    let step = activity::gerund(step.trim());
                    a.step = (!step.is_empty()).then(|| activity::cut(&step, activity::PHRASE_MAX));
                    a.intent = None;
                }
                let last = if c.input_summary.trim().is_empty() {
                    c.tool.clone()
                } else {
                    format!("{}: {}", c.tool, c.input_summary)
                };
                act(a, last);
            }
            // The turn is over: its actions are; what it said stays.
            EventBody::TurnEnded(_) => {
                let a = self.activity.entry(ev.thread.clone()).or_default();
                a.text.clear();
                a.last = None;
                a.action = None;
                a.intent = None;
                a.result = activity::result(&a.head);
                a.at = Some(at);
            }
            _ => {}
        }
    }

    /// What thread `v` is doing now, in one line: its status (from the registry and its open
    /// items), and while it runs, its agent's latest action ([`Activity`]).
    pub(super) fn doing(&self, v: &ThreadView) -> String {
        let a = self.activity.get(&v.id);
        let said = a.and_then(|a| a.said.as_deref());
        let doing = match v.status() {
            ThreadStatus::Attached => "attached: you are driving it in a terminal".to_string(),
            ThreadStatus::Parked => "parked".into(),
            ThreadStatus::Waiting => match self.open_request(&v.id) {
                Some(summary) => format!("waiting for you: {summary}"),
                None => "waiting for you".into(),
            },
            ThreadStatus::Background => format!(
                "working in background · {} task(s): {}",
                v.background_tasks.len(),
                v.background_tasks
                    .values()
                    .map(|t| t.description.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            ThreadStatus::Running => match a.and_then(|a| a.last.as_deref()) {
                Some(last) => last.to_string(),
                None => match a.and_then(|a| a.task.as_deref()) {
                    Some(task) => format!("starting: {task}"),
                    None => "working".into(),
                },
            },
            ThreadStatus::Error if v.crashed => "crashed".into(),
            ThreadStatus::Error => "idle · the last turn failed".into(),
            ThreadStatus::Done if v.stopped => "stopped".into(),
            ThreadStatus::Done | ThreadStatus::Idle => {
                let interrupted = v
                    .last_turn
                    .as_ref()
                    .is_some_and(|t| t.result == TurnResult::Interrupted);
                match said {
                    _ if interrupted => "idle · the last turn was interrupted".into(),
                    Some(s) => format!("idle · {s}"),
                    None => "idle".into(),
                }
            }
        };
        one_line(&doing, DOING_MAX)
    }

    /// What thread `v` is doing now, one level more specific than its task, in at most
    /// [`activity::ACTIVITY_MAX`] characters (see the module's doc).
    pub(super) fn activity_line(&self, v: &ThreadView) -> String {
        let a = self.activity.get(&v.id);
        let line = match v.status() {
            ThreadStatus::Attached | ThreadStatus::Parked => activity::fallback(v).to_string(),
            ThreadStatus::Waiting => match self.open_request_item(&v.id) {
                Some((AttentionReason::Permission, summary)) => {
                    format!("waiting for permission: {summary}")
                }
                Some((_, summary)) => format!("waiting for your answer: {summary}"),
                None => activity::fallback(v).to_string(),
            },
            ThreadStatus::Background => {
                let tasks: Vec<&str> = v
                    .background_tasks
                    .values()
                    .map(|t| t.description.as_str())
                    .filter(|d| !d.trim().is_empty())
                    .collect();
                match (v.background_tasks.len(), tasks.as_slice()) {
                    (1, [d]) => format!("waiting on background: {d}"),
                    (n, []) => format!("waiting on {n} background task(s)"),
                    (n, ds) => format!("waiting on {n} background tasks: {}", ds.join(", ")),
                }
            }
            ThreadStatus::Running => match a.and_then(Activity::working) {
                Some(line) => line,
                None => match a.and_then(|a| a.task.as_deref()) {
                    Some(task) => format!("starting: {task}"),
                    None => activity::fallback(v).to_string(),
                },
            },
            // The hub's error item (`api: turn 3 failed: …`), without the thread's name, and
            // with what happened when it does not say.
            ThreadStatus::Error => match self.open_reason(&v.id, AttentionReason::Error) {
                Some(why) => {
                    let why = why
                        .strip_prefix(&format!("{}: ", v.name))
                        .unwrap_or(&why)
                        .to_string();
                    let lower = why.to_lowercase();
                    if ["fail", "crash", "error"].iter().any(|w| lower.contains(w)) {
                        why
                    } else {
                        format!("{}: {why}", activity::fallback(v))
                    }
                }
                None => activity::fallback(v).to_string(),
            },
            ThreadStatus::Done if v.stopped => activity::fallback(v).to_string(),
            ThreadStatus::Done | ThreadStatus::Idle => {
                let last = v.last_turn.as_ref().map(|t| t.result);
                let report = v
                    .last_report
                    .as_ref()
                    .filter(|_| v.reported)
                    .and_then(|r| activity::result(&r.summary));
                match last {
                    Some(TurnResult::Interrupted) => "interrupted".into(),
                    Some(_) => match report.or_else(|| a.and_then(|a| a.result.clone())) {
                        Some(r) => format!("done: {r}"),
                        None => "done".into(),
                    },
                    None => activity::fallback(v).to_string(),
                }
            }
        };
        // Idle, but a prompt waits for other threads to finish (`hub::after`): that is next.
        let line = match v.status() {
            ThreadStatus::Done | ThreadStatus::Idle if !v.stopped => {
                match self.gated_names(&v.id).as_slice() {
                    [] => line,
                    names => format!(
                        "queued until {} finish{}",
                        names.join(", "),
                        if names.len() == 1 { "es" } else { "" }
                    ),
                }
            }
            _ => line,
        };
        activity::cut(&line, activity::ACTIVITY_MAX)
    }

    /// [`Core::activity_line`] as a thread summary may read it (`hub::summary`): only what the
    /// agent's own words and actions, its own background tasks and its reports say; `None` while
    /// that line would quote an item (waiting on the user, failed) or the turn's prompt (a
    /// turn just started), and for a thread attached, parked or stopped.
    ///
    /// Read off [`own_work`]`(v)`: the `!` commands the user runs on the thread are theirs, as
    /// they are to the orchestrator, so no line names one (a thread waiting on none but those
    /// reads as it would without them).
    pub(super) fn summary_activity(&self, v: &ThreadView) -> Option<String> {
        let v = own_work(v);
        let v = v.as_ref();
        match v.status() {
            ThreadStatus::Running => self
                .activity
                .get(&v.id)
                .and_then(Activity::working)
                .map(|l| activity::cut(&l, activity::ACTIVITY_MAX)),
            ThreadStatus::Background => Some(self.activity_line(v)),
            ThreadStatus::Done | ThreadStatus::Idle if !v.stopped => Some(self.activity_line(v)),
            _ => None,
        }
    }

    /// The summary of `thread`'s first open permission or question (in attention order), if
    /// any: what it waits on.
    fn open_request(&self, thread: &str) -> Option<String> {
        self.open_request_item(thread).map(|(_, summary)| summary)
    }

    /// The reason and summary of `thread`'s first open permission or question (in attention
    /// order), if any.
    fn open_request_item(&self, thread: &str) -> Option<(AttentionReason, String)> {
        self.attention
            .for_thread(thread, &std::collections::HashSet::new())
            .into_iter()
            .find(|i| {
                matches!(
                    i.reason,
                    AttentionReason::Permission | AttentionReason::Question
                )
            })
            .map(|i| (i.reason, i.summary.clone()))
    }

    /// The summary of `thread`'s first open item of `reason`, if any.
    fn open_reason(&self, thread: &str, reason: AttentionReason) -> Option<String> {
        self.attention
            .for_thread(thread, &std::collections::HashSet::new())
            .into_iter()
            .find(|i| i.reason == reason)
            .map(|i| i.summary.clone())
    }

    /// The `ext.twr.context` an adapter's permission or question on worker `thread` is
    /// recorded with: what the agent last said and the turn's prompt, when known.
    pub(super) fn request_context(&self, thread: &str) -> Option<Value> {
        let a = self.activity.get(thread)?;
        let mut m = Map::new();
        if let Some(s) = &a.said {
            m.insert("said".into(), json!(s));
        }
        if let Some(t) = &a.task {
            m.insert("task".into(), json!(t));
        }
        (!m.is_empty()).then_some(Value::Object(m))
    }

    /// `thread.activity`: every listed thread's activity, by thread id.
    pub(super) fn activity_json(&self) -> Value {
        let mut out = Map::new();
        for v in self.registry.list() {
            let a = self.activity.get(&v.id);
            out.insert(
                v.id.clone(),
                json!({
                    "activity": self.activity_line(v),
                    "doing": self.doing(v),
                    "said": a.and_then(|a| a.said.as_deref()),
                    "task": a.and_then(|a| a.task.as_deref()),
                    "at": a.and_then(|a| a.at),
                }),
            );
        }
        Value::Object(out)
    }
}

/// `v` as its own work has it: without the `!` commands the user runs on it
/// ([`twr_core::reduce::BackgroundTask::shell`], shared or not: a shared one's command goes to
/// the thread only when it ends), so its status and activity line say nothing of them. `v`
/// itself when it runs none.
pub(super) fn own_work(v: &ThreadView) -> Cow<'_, ThreadView> {
    if v.background_tasks.values().all(|t| !t.shell) {
        return Cow::Borrowed(v);
    }
    let mut own = v.clone();
    own.background_tasks.retain(|_, t| !t.shell);
    Cow::Owned(own)
}

/// The first line of `s` with any text on it.
fn first_line(s: &str) -> &str {
    s.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Adds `context` under `ext.twr.context` of `ext`.
pub(super) fn add_context(ext: &mut Option<JsonMap>, context: Value) {
    let ext = ext.get_or_insert_with(JsonMap::new);
    let twr = ext.entry("twr").or_insert_with(|| json!({}));
    if let Some(twr) = twr.as_object_mut() {
        twr.insert("context".into(), context);
    }
}

#[cfg(test)]
mod tests {

    /// [`one_line`] and [`tail`] give what flattening all of `s` gives, though they read
    /// only as much of it as they show: random text of words, runs of every kind of
    /// whitespace and characters of several widths, at limits around its length.
    #[test]
    fn one_line_and_tail_read_only_what_they_show() {
        fn flat(s: &str) -> String {
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        fn one_line_whole(s: &str, max: usize) -> String {
            let flat = flat(s);
            if flat.chars().count() <= max {
                return flat;
            }
            let mut out: String = flat.chars().take(max.saturating_sub(1)).collect();
            out.push('…');
            out
        }
        fn tail_whole(s: &str, max: usize) -> String {
            let flat = flat(s);
            let n = flat.chars().count();
            if n <= max {
                return flat;
            }
            let mut out = String::from("…");
            out.extend(flat.chars().skip(n - max.saturating_sub(1)));
            out
        }
        let alphabet = ['a', 'b', ' ', ' ', '\n', '\t', '\u{3000}', 'é', '😀', '…'];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..3000 {
            let len = next(40);
            let s: String = (0..len).map(|_| alphabet[next(alphabet.len())]).collect();
            for max in [0, 1, 2, 3, 5, 8, 13, 21, 40, 600] {
                assert_eq!(one_line(&s, max), one_line_whole(&s, max), "{s:?} {max}");
                assert_eq!(tail(&s, max), tail_whole(&s, max), "{s:?} {max}");
            }
        }
    }

    use super::*;
    use crate::hub::{Hub, HubOptions};
    use twr_core::config::Config;

    /// A worker's activity follows its events (a turn, text streamed in chunks, a tool call,
    /// a request, its end), a request it raises carries what it said and its task, and a
    /// restarted hub rebuilds it from the log.
    #[tokio::test]
    async fn activity_follows_the_log_and_requests_carry_their_context() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let adapter = || Source::Adapter("fake".into());
        let user = Source::User("test".into());
        let append = |source: Source, turn: Option<u64>, body: EventBody| {
            hub.append("t1", turn, source, body, None).unwrap()
        };
        let doing = |hub: &Hub| hub.thread_activity()["t1"]["doing"].clone();
        append(
            user.clone(),
            None,
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "idle");
        append(
            Source::Hub,
            Some(1),
            EventBody::TurnStarted(TurnStarted {
                prompt: "fix the login bug\nand add a test".into(),
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "starting: fix the login bug");
        for chunk in ["Found it: the token", " expires early.\nRunning the tests"] {
            append(
                adapter(),
                Some(1),
                EventBody::AgentText(AgentText {
                    text: chunk.into(),
                    delta: true,
                    ..Default::default()
                }),
            );
        }
        assert_eq!(doing(&hub), "Running the tests");
        append(
            adapter(),
            Some(1),
            EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: "cargo test".into(),
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "Bash: cargo test");
        let raised = append(
            adapter(),
            Some(1),
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "p1".into(),
                reason: AttentionReason::Permission,
                summary: "Allow Bash: git push".into(),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "waiting for you: Allow Bash: git push");
        // A second request; answering one leaves it waiting on the other.
        append(
            adapter(),
            Some(1),
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "p2".into(),
                reason: AttentionReason::Permission,
                summary: "Allow Bash: rm x".into(),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            }),
        );
        append(
            Source::User("test".into()),
            Some(1),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "p2".into(),
                by: ResolvedBy::User,
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "waiting for you: Allow Bash: git push");
        let EventBody::AttentionRaised(p) = &raised.body else {
            panic!("{raised:?}")
        };
        let context = &p.ext.as_ref().unwrap()["twr"]["context"];
        assert_eq!(
            context["said"],
            "Found it: the token expires early. Running the tests"
        );
        assert_eq!(context["task"], "fix the login bug");
        let item = hub.inner.core().attention.get("p1").cloned();
        let c = item.and_then(|i| i.context).expect("the item's context");
        assert_eq!(c.task.as_deref(), Some("fix the login bug"));
        // Answered, it runs on: its last action again.
        append(
            Source::User("test".into()),
            Some(1),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "p1".into(),
                by: ResolvedBy::User,
                ..Default::default()
            }),
        );
        assert_eq!(doing(&hub), "Bash: cargo test");
        append(
            adapter(),
            Some(1),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        let ended = doing(&hub);
        assert!(
            ended.as_str().unwrap().starts_with("idle · Found it"),
            "{ended}"
        );
        // A hub-raised item carries no agent context.
        let hub_item = hub
            .append(
                "t1",
                None,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "e1".into(),
                    reason: AttentionReason::Question,
                    summary: "hub question".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        let EventBody::AttentionRaised(p) = &hub_item.body else {
            panic!()
        };
        assert!(p.ext.is_none(), "{:?}", p.ext);
        // The thread waits on it, as its card says.
        let before = doing(&hub);
        assert_eq!(before, "waiting for you: hub question");
        hub.shutdown().await;
        drop(hub);
        let replayed = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        assert_eq!(doing(&replayed), before);
        assert_eq!(
            replayed.thread_activity()["t1"]["task"],
            "fix the login bug"
        );
        replayed.shutdown().await;
    }

    /// A message too long for the log (kept as a blob) is folded whole on replay: `said` keeps
    /// its end. A parked thread resumed is idle again.
    #[tokio::test]
    async fn replay_folds_long_text_whole_and_resume_clears_parked() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        // (Outside a turn: these messages come between turns.)
        let append = |hub: &Hub, source: Source, body: EventBody| {
            hub.append("t1", None, source, body, None).unwrap()
        };
        append(
            &hub,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        let long = format!("{} THE END", "word ".repeat(4000));
        let ev = append(
            &hub,
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: long,
                is_final: true,
                ..Default::default()
            }),
        );
        let EventBody::AgentText(t) = &ev.body else {
            panic!()
        };
        assert!(!t.extra.is_empty(), "capped in the log");
        let said = |hub: &Hub| hub.thread_activity()["t1"]["said"].clone();
        assert!(said(&hub).as_str().unwrap().ends_with("THE END"));
        // The next message streams on its own.
        append(
            &hub,
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: "Next".into(),
                delta: true,
                ..Default::default()
            }),
        );
        assert_eq!(said(&hub), "Next");
        append(
            &hub,
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: format!("{} THE END", "word ".repeat(4000)),
                is_final: true,
                ..Default::default()
            }),
        );
        append(
            &hub,
            Source::Hub,
            EventBody::ThreadParked(Default::default()),
        );
        assert_eq!(hub.thread_activity()["t1"]["doing"], "parked");
        append(
            &hub,
            Source::Hub,
            EventBody::ThreadResumed(Default::default()),
        );
        let idle = |hub: &Hub| {
            hub.thread_activity()["t1"]["doing"]
                .as_str()
                .unwrap()
                .starts_with("idle · ")
        };
        assert!(idle(&hub), "{}", hub.thread_activity());
        hub.shutdown().await;
        drop(hub);
        let replayed = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        assert!(said(&replayed).as_str().unwrap().ends_with("THE END"));
        assert!(idle(&replayed));
        replayed.shutdown().await;
    }

    /// A thread deleted and restored keeps what it last said; a late frame of an ended turn
    /// does not touch the open one.
    #[tokio::test]
    async fn restore_keeps_activity_and_late_frames_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let append = |turn: Option<u64>, source: Source, body: EventBody| {
            hub.append("t1", turn, source, body, None).unwrap()
        };
        let adapter = || Source::Adapter("fake".into());
        append(
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        let text = |s: &str| {
            EventBody::AgentText(AgentText {
                text: s.into(),
                is_final: true,
                ..Default::default()
            })
        };
        append(
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted {
                prompt: "one".into(),
                ..Default::default()
            }),
        );
        append(Some(1), adapter(), text("first"));
        append(
            Some(1),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        append(
            Some(2),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted {
                prompt: "two".into(),
                ..Default::default()
            }),
        );
        append(Some(2), adapter(), text("second"));
        append(Some(1), adapter(), text("late from one"));
        let said = |hub: &Hub| hub.thread_activity()["t1"]["said"].clone();
        assert_eq!(said(&hub), "second");
        append(
            Some(2),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        append(
            None,
            Source::Hub,
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "api".into(),
                ..Default::default()
            }),
        );
        assert!(hub.thread_activity().get("t1").is_none());
        append(
            None,
            Source::Hub,
            EventBody::ThreadRestored(ThreadRestored {
                name: "api".into(),
                ..Default::default()
            }),
        );
        assert_eq!(said(&hub), "second");
        hub.shutdown().await;
    }

    #[test]
    fn one_line_flattens_and_cuts() {
        assert_eq!(one_line("a\n  b\tc", 10), "a b c");
        assert_eq!(one_line("abcdef", 4), "abc…");
        assert_eq!(tail("abcdef", 4), "…def");
        assert_eq!(tail("ab", 4), "ab");
        let mut s = "ééé".to_string();
        keep_end(&mut s, 3);
        assert_eq!(s, "é");
    }

    /// The activity line says what a worker does one level more specific than its task: the
    /// step its plan has in progress or what it said it is about to do, then what its latest
    /// action does; what it waits on; what its turn came to. It is in `thread.activity` and in
    /// the thread object (`thread.list`, `thread.get`), and a restarted hub rebuilds it.
    #[tokio::test]
    async fn the_activity_line_says_what_a_thread_does() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let adapter = || Source::Adapter("fake".into());
        let append = |source: Source, turn: Option<u64>, body: EventBody| {
            hub.append("t1", turn, source, body, None).unwrap()
        };
        let line = |hub: &Hub| {
            let a = hub.thread_activity()["t1"]["activity"].clone();
            let listed = hub
                .list_threads()
                .into_iter()
                .find(|t| t["id"] == "t1")
                .unwrap();
            assert_eq!(listed["activity"], a, "the thread object says the same");
            a.as_str().unwrap().to_string()
        };
        let call = |tool: &str, input: &str, ext: Option<Value>| {
            EventBody::AgentToolCall(AgentToolCall {
                call_id: format!("c-{tool}-{input}"),
                tool: tool.into(),
                input_summary: input.into(),
                ext: ext.and_then(|e| e.as_object().cloned()),
                ..Default::default()
            })
        };
        let text = |s: &str| {
            EventBody::AgentText(AgentText {
                text: s.into(),
                is_final: true,
                ..Default::default()
            })
        };
        append(
            Source::User("test".into()),
            None,
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        assert_eq!(line(&hub), "not started");
        append(
            Source::Hub,
            Some(1),
            EventBody::TurnStarted(TurnStarted {
                prompt: "make the view keep its place\nwhile text streams".into(),
                ..Default::default()
            }),
        );
        assert_eq!(line(&hub), "starting: make the view keep its place");
        append(
            adapter(),
            Some(1),
            call("Read", "/src/twr/crates/twr-tui/src/app.rs", None),
        );
        assert_eq!(line(&hub), "reading app.rs");
        // What it says it is about to do leads, until it says something else.
        append(
            adapter(),
            Some(1),
            text("Found the anchor. Let me look at how scrolling works."),
        );
        assert_eq!(line(&hub), "looking at how scrolling works · writing");
        append(
            adapter(),
            Some(1),
            call("Grep", "fn scroll_up in crates", None),
        );
        assert_eq!(
            line(&hub),
            "looking at how scrolling works · searching for fn scroll_up"
        );
        // The step its plan has in progress leads instead.
        let step = json!({"twr": {"step": "Implementing scroll anchoring"}});
        append(adapter(), Some(1), call("TaskUpdate", "", Some(step)));
        append(adapter(), Some(1), call("Edit", "/src/ui.rs", None));
        assert_eq!(line(&hub), "implementing scroll anchoring · editing ui.rs");
        // The purpose the agent gave a command says what it does.
        let purpose = json!({"twr": {"purpose": "Run the TUI tests"}});
        append(
            adapter(),
            Some(1),
            call("Bash", "cargo test -p twr-tui", Some(purpose)),
        );
        assert_eq!(
            line(&hub),
            "implementing scroll anchoring · running the TUI tests"
        );
        append(
            adapter(),
            Some(1),
            call("Bash", "cargo fmt && cargo test", None),
        );
        assert_eq!(line(&hub), "implementing scroll anchoring · running tests");
        // Waiting: on what.
        append(
            adapter(),
            Some(1),
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "p1".into(),
                reason: AttentionReason::Permission,
                summary: "Bash: git push".into(),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            }),
        );
        assert_eq!(line(&hub), "waiting for permission: Bash: git push");
        append(
            Source::User("test".into()),
            Some(1),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "p1".into(),
                by: ResolvedBy::User,
                ..Default::default()
            }),
        );
        assert_eq!(line(&hub), "implementing scroll anchoring · running tests");
        // The step done (an empty one), what it says next leads.
        let none = json!({"twr": {"step": ""}});
        append(adapter(), Some(1), call("TaskUpdate", "", Some(none)));
        append(
            adapter(),
            Some(1),
            text("Tests pass. Now I'll commit part 1."),
        );
        append(
            adapter(),
            Some(1),
            call("Bash", "git commit -m 'part 1'", None),
        );
        assert_eq!(line(&hub), "committing part 1");
        // Its last message says what the turn came to.
        append(
            adapter(),
            Some(1),
            text("Scroll anchoring is in and tested.\n\nDetails follow."),
        );
        append(
            adapter(),
            Some(1),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        assert_eq!(line(&hub), "done: Scroll anchoring is in and tested.");
        let before = line(&hub);
        hub.shutdown().await;
        drop(hub);
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        assert_eq!(line(&hub), before, "rebuilt from the log");
        // A report logged when the hub ended reports with ` — twr attach <name>` reads without
        // it (its first sentence, which has no full stop, ran on into the command).
        hub.append(
            "t1",
            Some(1),
            Source::Hub,
            EventBody::ReportReady(ReportReady {
                summary: "Scroll anchoring is in — twr attach api".into(),
                turn: Some(1),
                written_by: "hub".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(line(&hub), "done: Scroll anchoring is in");
        // A failed turn says why; a parked thread says so.
        hub.append(
            "t1",
            Some(2),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted {
                prompt: "again".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            "t1",
            Some(2),
            adapter(),
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Error,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(line(&hub), "failed");
        hub.append(
            "t1",
            Some(2),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "e1".into(),
                reason: AttentionReason::Error,
                summary: "api: turn 2 failed: rate limited".into(),
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(line(&hub), "turn 2 failed: rate limited");
        hub.append(
            "t1",
            None,
            Source::Hub,
            EventBody::ThreadParked(Default::default()),
            None,
        )
        .unwrap();
        assert_eq!(line(&hub), "parked");
        hub.shutdown().await;
    }

    /// A tool call too big for the log (its input kept as a blob, a preview in the event) is
    /// read whole again on replay: the line says what the whole command does, as it did live.
    /// Text whose lowercase is shorter than itself replays as it was folded.
    #[tokio::test]
    async fn a_capped_tool_call_replays_to_the_same_line() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let adapter = || Source::Adapter("fake".into());
        let append = |hub: &Hub, source: Source, turn: Option<u64>, body: EventBody| {
            hub.append("t1", turn, source, body, None).unwrap()
        };
        append(
            &hub,
            Source::User("test".into()),
            None,
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        append(
            &hub,
            Source::Hub,
            Some(1),
            EventBody::TurnStarted(TurnStarted {
                prompt: "test it".into(),
                ..Default::default()
            }),
        );
        append(
            &hub,
            adapter(),
            Some(1),
            EventBody::AgentText(AgentText {
                text: "o\u{212A}, I'll run the tests.".into(),
                is_final: true,
                ..Default::default()
            }),
        );
        let command = format!("echo {} && cargo test", "x".repeat(6000));
        let ev = append(
            &hub,
            adapter(),
            Some(1),
            EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: command,
                ..Default::default()
            }),
        );
        let EventBody::AgentToolCall(c) = &ev.body else {
            panic!("{ev:?}")
        };
        assert!(c.input_ref.is_some(), "capped in the log");
        let line = |hub: &Hub| hub.thread_activity()["t1"]["activity"].clone();
        assert_eq!(line(&hub), "running tests");
        hub.shutdown().await;
        drop(hub);
        // The log replayed (as a hub starting does, before it closes the turn the stopped
        // hub's runtime was in): the call is folded whole.
        let (core, _) = crate::hub::Core::replay(&db).unwrap();
        assert_eq!(core.activity["t1"].action.as_deref(), Some("running tests"));
        assert_eq!(
            core.activity_line(core.registry.get("t1").unwrap()),
            "running tests"
        );
    }

    /// With its turn over and background tasks left, a thread says what it waits on.
    #[tokio::test]
    async fn the_activity_line_names_the_background_work() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let adapter = || Source::Adapter("fake".into());
        let append = |source: Source, turn: Option<u64>, body: EventBody| {
            hub.append("t1", turn, source, body, None).unwrap()
        };
        append(
            Source::User("test".into()),
            None,
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
        );
        append(
            Source::Hub,
            Some(1),
            EventBody::TurnStarted(TurnStarted {
                prompt: "run the suite".into(),
                ..Default::default()
            }),
        );
        let task = |id: &str, running: bool| {
            EventBody::TaskUpdated(TaskUpdated {
                task_id: id.into(),
                running,
                description: format!("{id}: cargo test --workspace"),
                ..Default::default()
            })
        };
        append(adapter(), None, task("bg1", true));
        append(
            adapter(),
            Some(1),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        let line = |hub: &Hub| hub.thread_activity()["t1"]["activity"].clone();
        assert_eq!(
            line(&hub),
            "waiting on background: bg1: cargo test --workspace"
        );
        append(adapter(), None, task("bg2", true));
        assert_eq!(
            line(&hub),
            "waiting on 2 background tasks: bg1: cargo test --workspace, bg2: cargo test --workspace"
        );
        hub.shutdown().await;
    }
}

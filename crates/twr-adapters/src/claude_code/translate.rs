//! Pure translation between Claude Code's stream-json protocol and twr events.
//!
//! [`Translator`] has no IO: the driver feeds it every stdout line ([`Translator::translate`])
//! and asks it for the stdin lines to write ([`Translator::user_message`],
//! [`Translator::respond`], [`Translator::interrupt`]). The fixture tests replay recorded
//! sessions through it.
//!
//! Wire facts it relies on (Claude Code 2.1.284, see the recorded fixtures):
//! - `system/init` starts every turn and carries the session id. Only the first one of a
//!   process becomes `thread.started`. Its `permissionMode` (and that of any other `system`
//!   message carrying one) is checked against the mode the process was started with
//!   ([`mode_acceptable`]); the one widening accepted is plan → an ask-level mode after the
//!   user approved an `ExitPlanMode` request (recorded as `ext.claude.mode_changed` on the next
//!   `turn.ended`). Any other wider (or unknown) mode interrupts the running turn, ends every
//!   waiting turn with an error, reads nothing more and stops the session (`fatal`).
//! - Assistant messages arrive one per content block; `stream_event` wraps the raw API
//!   stream (`message_start` identifies an autonomous turn; `text_delta` streams text).
//! - `result` ends a turn. `total_cost_usd` is cumulative per process (per-turn cost is the
//!   delta); `usage`, `duration_ms` and `num_turns` are per turn. An interrupted turn's cost
//!   is not included (the total stays put, or reads 0), so it is reported as unknown. A
//!   `result` with `origin.kind: task-notification` and `num_turns: 0` (Claude 2.1.291: a
//!   resumed session consuming a notice of a task its previous process left, before the
//!   prompt) asked the model nothing: it ends no twr turn (the turn waits for the model's
//!   answer) and opens none. Its total, before any run of this process reached the model, is
//!   what the session spent before (the baseline); after, what it adds goes to the next
//!   charged turn.
//! - Permission prompts are `control_request{subtype: can_use_tool}`; `AskUserQuestion`
//!   arrives the same way with `requires_user_interaction: true`. Only a request whose
//!   `tool_name` is `AskUserQuestion` becomes a question. Every other `can_use_tool` is a plain
//!   allow/deny permission, whatever its input looks like (an MCP tool with a `questions`
//!   field, too) and whether or not it is flagged `requires_user_interaction` (`ExitPlanMode`):
//!   relaying a free-text or option answer must never turn into an allow for an arbitrary tool.
//! - One `AskUserQuestion` request can carry several questions. Each becomes its own
//!   `attention.raised{question}` item: `attn_id` `<request_id>#q<i>` (0-based), summary
//!   `(i+1/n) <question>`, that question's option labels, `call_id` the tool_use id. A request
//!   with a single question keeps `attn_id = request_id` (the recorded fixtures, and the id
//!   Claude uses on the wire). The `control_response` is only sent once every question has an
//!   answer, with all of them in `updatedInput.answers`; declining any one declines the whole
//!   request (one deny) and withdraws the still-open siblings as
//!   `attention.resolved{by: timeout, answer: null}`.
//! - A tool call says what it is for when the agent said: a Bash call's `description`
//!   becomes `ext.twr.purpose` ([`PURPOSE_KEY`]). The step the agent's own plan has in
//!   progress goes on the call that changes it as `ext.twr.step` ([`STEP_KEY`]; empty: none):
//!   a `TodoWrite`'s `in_progress` todo (its `activeForm`, else its `content`), or the task a
//!   `TaskUpdate` sets `in_progress` (its `activeForm`, else its `subject`, as the call or the
//!   `TaskCreate` that made it gave them; the id comes from the `TaskCreate`'s result, `Task
//!   #<id> created …`). A `TaskUpdate` that rewords that task gives its new words; one that
//!   moves it out of `in_progress` (completed, deleted, back to pending) clears it. Only the
//!   main agent's calls count, not a subagent's ([`Steps`]).
//! - `control_cancel_request` withdraws a pending `can_use_tool`; each of its open items
//!   becomes `attention.resolved{by: timeout, answer: null}`.
//! - Open items are immutable (see [`crate::attn`]): a `control_request` whose `request_id` is
//!   still pending replaces that request. Its open items are withdrawn
//!   (`attention.resolved{by: timeout, answer: null}`, `ext.claude.superseded = true`) and the
//!   new request is raised under a new incarnation id (`<request_id>~<n>`, questions
//!   `<request_id>~<n>#q<i>`). Every answer is built from the content its own id was raised
//!   with (a permission's `updatedInput` is that incarnation's input), and an answer to a
//!   withdrawn incarnation is ignored: it never answers the replacement.
//! - An interrupted turn ends with an `is_error` `result`, normally with
//!   `terminal_reason: aborted_*`, `stop_reason: null`, and a preceding
//!   `[Request interrupted by user]` user text.
//! - Usage ([`twr_core::usage`]): a `rate_limit_event`'s `unifiedWindows` (both windows, as
//!   fractions with `resetsAt` in epoch seconds; Claude Code 2.1.295) becomes `runtime.limits`
//!   (`source: event`); without them its top-level `utilization` of a `five_hour` or
//!   `seven_day` `rateLimitType` does. The `get_usage` control request (`skip_behaviors`: the
//!   plan windows only, no transcript scan; percentages with ISO `resets_at`) is the read
//!   that costs no turn: written ([`Translated::requests`]) after the first `system/init`, and
//!   after an `init` or `result` once the last report is [`twr_core::usage::REFRESH_AFTER`]
//!   old. Its answer becomes `runtime.limits` (`source: read`, `plan` from
//!   `subscription_type`). An error answer or `rate_limits_available: false` (an API key
//!   login) stops the reads.
//! - Any other `is_error` (or `error_*` subtype) `result` is a failed turn. Its cause, put in
//!   `ext.twr.error` ([`error_line`]) for the hub's error item, is the `result` text (an API
//!   error: `subtype: success`, `api_error_status`, `terminal_reason: api_error`), else
//!   `errors` (`error_max_turns`, `error_max_budget_usd`, `error_during_execution`), else
//!   `api_error_status` with the turn's API error message (an `assistant` message carrying
//!   `error`), else `terminal_reason` / the subtype.
//!
//! Turn stamping: every user message written for turn `n` queues `n`; the first in-turn line
//! after a `result` (normally the next `system/init`) opens the oldest queued turn, and the
//! `result` closes it. Claude runs queued user messages as separate turns in order, so FIFO
//! matches its behavior. Model output with no turn running or queued (Claude resuming on its
//! own after a background task), or a `result` of such a run that showed none, opens a
//! `from: runtime` turn whose events carry no turn: the hub numbers it; its start and end
//! carry its id (`ext.twr.runtime_turn`). A `system/init` alone opens none: Claude also sends
//! one before a run that never reaches the model. A user message written after that `init`
//! or meanwhile stays queued until the run's `result`.
//!
//! Claude's own runs (2.1.304): the end of a task the main agent put in the background
//! (`task_started` with `is_backgrounded: true`, or a later `task_updated` patch saying so; not
//! `owned_by_subagent`) makes Claude start a run of its own for its notification, with no
//! `init`, and its first output may come a minute later (time to first token). So that end,
//! with no turn running or queued, opens the runtime's turn at once ([`Translator::task`]):
//! the hub sees the thread busy and writes no prompt into that run. A prompt written into a
//! run Claude started itself is folded into it, and since 2.1.295 such a run ends with no
//! `result` at all, which left the turn open for hours. A turn so opened that sees no line of
//! a run (output, a request, a `result`) within [`TranslatorOptions::notice_run_after`] is
//! ended `completed` with `ext.twr.no_run` (Claude ran nothing); one ended by a `result` that
//! asked the model nothing (`num_turns: 0`) ends then.
//!
//! A missing `result`: once the main agent's message stops for good (`message_delta` with a
//! `stop_reason` other than `tool_use` / `pause_turn`) in a running turn, the `result` is
//! due. When nothing else comes within [`TranslatorOptions::missing_result_after`] (no new
//! model message, request, `init` or `result`), [`Translator::expire`] ends the turn
//! `completed` with `ext.twr.missing_result` (no usage), and so does each turn queued meanwhile
//! (its prompt was folded into that run: Claude starts no run for it), and a `result` that still comes
//! before the next run shows (an `init` or model output), a prompt queued since or not, is
//! absorbed (its total read, charged to no turn; no turn ended or opened).

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use twr_core::adapter::Answer;
use twr_core::usage::{UsageWindow, FIVE_HOUR, SEVEN_DAY};

use crate::acp::translate::{mode_acceptable, mode_level, ModeLevel};
use crate::attn::IssuedIds;
use twr_core::event::{
    set_twr_ext, AgentText, AgentThinking, AgentToolCall, AgentToolResult, AttentionRaised,
    AttentionReason, AttentionResolved, EventBody, JsonMap, ResolvedBy, SessionPayload, TurnEnded,
    TurnFrom, TurnResult, TurnStarted, Usage, PURPOSE_KEY, RUNTIME_TURN_KEY, STEP_KEY,
};

/// Key of this runtime's extras (`data.ext.claude`).
pub const EXT: &str = "claude";

/// Tools whose calls always count as risky.
pub const RISKY_TOOLS: &[&str] = &["Bash", "Write", "Edit", "MultiEdit", "NotebookEdit"];

/// Substrings (matched lowercase against a tool input) that make any call risky.
pub const DESTRUCTIVE_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r ",
    "git push --force",
    "git push -f",
    "git reset --hard",
    "git clean -f",
    "git checkout -- ",
    "sudo ",
    "mkfs",
    "dd if=",
    "chmod -r 777",
    "> /dev/sd",
];

/// Maximum length (chars) of a tool call's `input_summary`.
pub const INPUT_SUMMARY_MAX: usize = 160;

/// Maximum length (chars) of a tool result's `output_summary`.
pub const OUTPUT_SUMMARY_MAX: usize = 200;

/// Message sent with a deny when the user gave none.
pub const DEFAULT_DENY_MESSAGE: &str = "Denied by user via twr";

/// Message sent when the user declines to answer an `AskUserQuestion`.
pub const DEFAULT_DECLINE_MESSAGE: &str = "The user declined to answer via twr";

/// `ext.twr.error` of a turn whose `result` line was too long to read.
pub const OVERSIZED_ERROR: &str = "oversized runtime message";

/// `turn.ended` `ext.twr` key: the turn was ended because its `result` never came.
pub const MISSING_RESULT_KEY: &str = "missing_result";
/// `turn.ended` `ext.twr` key: the turn opened for a notification saw no run.
pub const NO_RUN_KEY: &str = "no_run";

/// `d` as a chrono duration (the longest one when it does not fit).
fn chrono_of(d: std::time::Duration) -> chrono::Duration {
    chrono::Duration::from_std(d).unwrap_or(chrono::Duration::MAX)
}

/// Prefix of the `request_id` of twr's `get_usage` reads.
pub const USAGE_REQUEST: &str = "twr_usage_";

/// Start of the user text Claude records when a turn is interrupted.
const INTERRUPTED_MARKER: &str = "[Request interrupted by user";

/// Static facts about the process a translator serves.
#[derive(Debug, Clone, Default)]
pub struct TranslatorOptions {
    /// Emit `agent.thinking` for non-empty thinking blocks.
    pub thinking: bool,
    /// Runtime process id (`ext.twr.pid` on `thread.started`).
    pub pid: Option<u32>,
    /// Program the process was started as (`ext.twr.exe`).
    pub exe: Option<String>,
    /// The process resumed an existing session (`ext.claude.resumed`).
    pub resumed: bool,
    /// The `--permission-mode` the process was started with (empty: `default`): every
    /// `permissionMode` Claude reports is checked against it ([`Translator::translate`]).
    pub mode: String,
    /// How long after the main agent's message stopped a missing `result` ends the turn
    /// (`None`: [`MISSING_RESULT_AFTER`]).
    pub missing_result_after: Option<std::time::Duration>,
    /// How long a turn opened for a background task's notification waits for its run
    /// (`None`: [`NOTICE_RUN_AFTER`]).
    pub notice_run_after: Option<std::time::Duration>,
}

/// Default [`TranslatorOptions::missing_result_after`].
pub const MISSING_RESULT_AFTER: std::time::Duration = std::time::Duration::from_secs(120);
/// Default [`TranslatorOptions::notice_run_after`].
pub const NOTICE_RUN_AFTER: std::time::Duration = std::time::Duration::from_secs(180);

/// What the translator waits for, with a deadline ([`Translator::deadline`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// The running turn's `result`, after the main agent's message stopped.
    Result,
    /// The run of a notification the runtime's turn was opened for.
    NoticeRun,
}

/// One normalized event produced from a stdout line.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// Kind + payload.
    pub body: EventBody,
    /// Turn it belongs to (None for `thread.started`, or when no turn was queued).
    pub turn: Option<u64>,
    /// The raw stdout line, kept for non-delta events.
    pub raw: Option<String>,
}

/// Result of translating one stdout line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translated {
    /// Events to emit, in order.
    pub events: Vec<Emitted>,
    /// A line to write to stdin right away (reply to a control request twr does not handle,
    /// or the interrupt of a session that cannot go on).
    pub reply: Option<String>,
    /// Requests of twr's own to write after `reply` (the `get_usage` read).
    pub requests: Vec<String>,
    /// The session cannot go on (Claude reports a permission mode wider than twr allows): the
    /// driver stops the process. The text says why.
    pub fatal: Option<String>,
}

/// A pending request: the incarnation of its `request_id` the user sees (the base of its
/// attention ids) and exactly the content raised under it.
#[derive(Debug, Clone)]
struct Request {
    /// Base of the attention ids of this incarnation ([`question_attn_id`]).
    base: String,
    kind: Pending,
}

#[derive(Debug, Clone)]
enum Pending {
    Permission {
        /// The tool asking (an approved `ExitPlanMode` is the user's consent to leave plan
        /// mode).
        tool: String,
        input: Value,
        /// Whether `allow` is offered: not when the input is too long to show in full
        /// ([`RenderedCommand::too_long`]), when any answer denies.
        allow: bool,
    },
    /// An MCP elicitation (a form or URL the server wants the user to fill or visit). twr
    /// cannot show forms: the user may only decline or cancel it.
    Elicitation,
    /// An `AskUserQuestion` request: one attention item per question.
    Question {
        input: Value,
        /// Question texts (one empty entry when the input had none).
        questions: Vec<String>,
        /// The answer given to each question so far.
        answers: Vec<Option<String>>,
    },
}

impl Pending {
    /// Number of attention items.
    fn items(&self) -> usize {
        match self {
            Pending::Permission { .. } | Pending::Elicitation => 1,
            Pending::Question { answers, .. } => answers.len(),
        }
    }

    /// Whether item `i` still waits for an answer.
    fn open_at(&self, i: usize) -> bool {
        match self {
            Pending::Permission { .. } | Pending::Elicitation => i == 0,
            Pending::Question { answers, .. } => answers.get(i).is_some_and(Option::is_none),
        }
    }
}

impl Request {
    /// Attention ids of the items still waiting for an answer.
    fn open_ids(&self) -> Vec<String> {
        let n = self.kind.items();
        (0..n)
            .filter(|&i| self.kind.open_at(i))
            .map(|i| question_attn_id(&self.base, i, n))
            .collect()
    }
}

/// Attention id of question `i` of the `n` in a request incarnation with id base `base`: the
/// base itself when there is only one question, else `<base>#q<i>`.
fn question_attn_id(base: &str, i: usize, n: usize) -> String {
    if n <= 1 {
        base.to_string()
    } else {
        format!("{base}#q{i}")
    }
}

/// Stateful stream-json → twr translator for one Claude Code process.
#[derive(Debug)]
pub struct Translator {
    tasks: crate::background::Tasks,
    /// The running turn is the runtime's own (`from: runtime`), with this id
    /// (`ext.twr.runtime_turn` of its start and end): its events carry no turn, the hub
    /// numbers it.
    runtime_turn: Option<u64>,
    /// Runtime turns opened so far (their ids).
    runtime_turns: u64,
    opts: TranslatorOptions,
    harness_metadata: Map<String, Value>,
    started: bool,
    session_id: Option<String>,
    queued: VecDeque<u64>,
    /// A turn is running inside Claude (between its first line and its `result`).
    active: bool,
    turn: Option<u64>,
    /// The process's cumulative cost (`total_cost_usd`) up to which turns were charged, or
    /// which was the session's spend before this process (the baseline).
    last_total_cost: f64,
    /// The model worked for the parent session in this process (its message started or showed
    /// output, or a run reached its `result`), whether or not its cost was known: a later total
    /// is this process's spend, never the session's earlier one. Child agents' output does not
    /// count.
    model_ran: bool,
    /// Spend a `result` that asked the model nothing reported after a charged turn: the next
    /// charged turn's.
    uncharged_cost: f64,
    tools: HashMap<String, String>,
    /// The main agent's plan, for the step it has in progress ([`Steps`]).
    steps: Steps,
    /// Pending requests by their wire `request_id`.
    pending: HashMap<String, Request>,
    /// Attention ids handed out (never reused while remembered).
    issued: IssuedIds,
    interrupt_requested: bool,
    interrupts_sent: u64,
    /// The running turn carried Claude's `[Request interrupted by user]` user text.
    interrupted_marker: bool,
    transient_hint: Option<String>,
    /// The running turn reported an authentication failure (an `assistant` or `api_retry`
    /// message with `error: authentication_failed`).
    auth_failed: bool,
    /// The text of the running turn's API error message (an `assistant` message with an
    /// `error`), the cause of a failed `result` that carries no text of its own.
    api_error: Option<String>,
    /// The permission mode the session may be in: the one it was started with, or the ask-level
    /// mode Claude switched to after the user approved leaving plan mode.
    allowed_mode: String,
    /// The user approved an `ExitPlanMode` request in this process.
    plan_exit_approved: bool,
    /// A mode change the user consented to, recorded on the next `turn.ended`
    /// (`ext.claude.mode_changed`).
    mode_changed: Option<Value>,
    /// The session ran in a mode twr does not accept and was ended: nothing more is read.
    failed: bool,
    /// When to read the plan's usage again (`get_usage`).
    usage: crate::usage::Reads,
    /// Tasks the main agent put in the background: their end starts a run of Claude's own.
    notifying: std::collections::HashSet<String>,
    /// Tasks a subagent owns.
    subagent_tasks: std::collections::HashSet<String>,
    /// What is awaited, and until when.
    watch: Option<(Watch, DateTime<Utc>)>,
    /// A turn was ended for its missing `result`: a `result` before the next run shows (its
    /// `init` or model output) is that run's, absorbed.
    absorb_result: bool,
    /// The runtime's turn was opened for a background task's notification and the main agent
    /// has shown no output in it yet.
    notice_quiet: bool,
    /// The clock usage reads are timed by.
    clock: fn() -> DateTime<Utc>,
}

/// What the user's approval of `ExitPlanMode` lets the session do (`ext.claude.mode_note`).
pub const PLAN_EXIT_NOTE: &str = "the user approved leaving plan mode (ExitPlanMode), so the \
    session continues in the ask-level mode Claude switched to";

impl Translator {
    /// A translator for a fresh process.
    pub fn new(opts: TranslatorOptions) -> Self {
        Translator {
            tasks: crate::background::Tasks::default(),
            runtime_turn: None,
            runtime_turns: 0,
            started: false,
            session_id: None,
            queued: VecDeque::new(),
            active: false,
            turn: None,
            last_total_cost: 0.0,
            model_ran: false,
            uncharged_cost: 0.0,
            tools: HashMap::new(),
            steps: Steps::default(),
            pending: HashMap::new(),
            issued: IssuedIds::default(),
            interrupt_requested: false,
            interrupts_sent: 0,
            interrupted_marker: false,
            transient_hint: None,
            auth_failed: false,
            api_error: None,
            allowed_mode: match opts.mode.trim() {
                "" => "default".to_string(),
                m => m.to_string(),
            },
            plan_exit_approved: false,
            mode_changed: None,
            failed: false,
            usage: crate::usage::Reads::default(),
            notifying: Default::default(),
            subagent_tasks: Default::default(),
            watch: None,
            absorb_result: false,
            notice_quiet: false,
            clock: Utc::now,
            opts,
            harness_metadata: Map::new(),
        }
    }

    /// Times usage reads by `clock` instead of the system clock (tests).
    pub fn set_clock(&mut self, clock: fn() -> DateTime<Utc>) {
        self.clock = clock;
    }

    /// Startup facts supplied by the harness, recorded with thread.started.
    pub fn set_harness_metadata(&mut self, metadata: Map<String, Value>) {
        self.harness_metadata = metadata;
    }

    /// Session id reported by `system/init`, once seen.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// Whether a turn is running or queued.
    pub fn busy(&self) -> bool {
        self.active || !self.queued.is_empty()
    }

    /// Attention ids waiting for an answer.
    pub fn pending_attention(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.pending.values().flat_map(Request::open_ids).collect();
        ids.sort();
        ids
    }

    /// The pending request (its wire `request_id`) and question index an open attention id
    /// refers to. Only the current incarnation's ids match: an id of a replaced request, or of
    /// an item already answered, refers to nothing.
    fn locate(&self, attn_id: &str) -> Option<(String, usize)> {
        self.pending.iter().find_map(|(rid, r)| {
            let n = r.kind.items();
            (0..n)
                .find(|&i| r.kind.open_at(i) && question_attn_id(&r.base, i, n) == attn_id)
                .map(|i| (rid.clone(), i))
        })
    }

    /// A fresh id base for the `n` items of request `request_id` (see [`crate::attn`]).
    fn fresh_base(&mut self, request_id: &str, n: usize) -> String {
        let open: Vec<String> = self.pending_attention();
        self.issued.fresh(
            request_id,
            n,
            |b, i| question_attn_id(b, i, n),
            |id| open.iter().any(|o| o == id),
        )
    }

    /// A `control_request` reused the id of pending request `old`: the runtime replaced it.
    /// Its open items are withdrawn (`ext.claude.superseded`), so no answer to them is ever
    /// sent (the replacement is raised under a new id).
    fn supersede(&self, request_id: &str, old: &Request, line: &str, out: &mut Translated) {
        tracing::info!(
            request_id,
            "claude-code: runtime replaced a pending request; withdrawing its items"
        );
        for attn_id in old.open_ids() {
            out.events
                .push(self.withdrawal(attn_id, "superseded", Some(line)));
        }
    }

    /// `attention.resolved{by: timeout, answer: null}` for an item this adapter withdraws.
    fn withdrawal(&self, attn_id: String, cause: &str, raw: Option<&str>) -> Emitted {
        let mut claude = Map::new();
        claude.insert("cancelled_by".into(), json!(cause));
        if cause == "superseded" {
            claude.insert("superseded".into(), json!(true));
        }
        Emitted {
            body: EventBody::AttentionResolved(AttentionResolved {
                attn_id,
                by: ResolvedBy::Timeout,
                answer: None,
                ext: ext_of(claude),
                ..Default::default()
            }),
            // Stamp the running turn; never open a queued one for a withdrawal.
            turn: if self.active { self.turn } else { None },
            raw: raw.map(str::to_string),
        }
    }

    /// The stdin line starting turn `turn` with `text`; queues the turn number.
    pub fn user_message(&mut self, text: &str, turn: u64) -> String {
        self.queued.push_back(turn);
        json!({
            "type": "user",
            "message": {"role": "user", "content": text},
            "parent_tool_use_id": null,
            "session_id": "",
        })
        .to_string()
    }

    /// The stdin line interrupting the running turn, if one is running or queued.
    pub fn interrupt(&mut self) -> Option<String> {
        if !self.busy() {
            return None;
        }
        self.interrupt_requested = true;
        self.interrupts_sent += 1;
        Some(
            json!({
                "type": "control_request",
                "request_id": format!("twr_interrupt_{}", self.interrupts_sent),
                "request": {"subtype": "interrupt"},
            })
            .to_string(),
        )
    }

    /// Handles the user's answer to attention `attn_id`. Returns None if it is not pending
    /// (unknown or already answered); otherwise `reply` is the stdin line to write, if any, and
    /// `events` are events to emit (withdrawn sibling questions).
    ///
    /// Permission: `allow` allows with the original input; anything else (including an option
    /// label or free text) denies with `answer.message` (default [`DEFAULT_DENY_MESSAGE`]).
    ///
    /// Question (one item per `AskUserQuestion` question), in order:
    /// - `answer.extra.answers[<question text>]`, when it is a non-empty string, is the answer;
    /// - `deny` / `decline` (when not one of this question's option labels) declines the whole
    ///   request: a deny with `answer.message` (default [`DEFAULT_DECLINE_MESSAGE`]), and the
    ///   other still-open questions of the request are withdrawn
    ///   (`attention.resolved{by: timeout}`);
    /// - otherwise the answer text is the decision, or `answer.message` when the decision is
    ///   empty or `allow`. A bare `allow` with no message carries no answer and declines.
    ///
    /// Nothing is written until every question of the request has an answer; then one allow
    /// carries all of them in `updatedInput.answers` (keyed by question text).
    ///
    /// The answer text may be an option label or free text. Free text is sent as the answer
    /// on purpose: `AskUserQuestion` always offers an "Other" choice whose answer is whatever
    /// the user typed, so a non-label answer is a legitimate reply, not an approval.
    ///
    /// `attn_id` must be an open item of the request's current incarnation: an answer to an
    /// item of a request the runtime replaced since (see [`crate::attn`]) returns None, and
    /// the reply is built only from the content `attn_id` was raised with.
    pub fn respond(&mut self, attn_id: &str, answer: &Answer) -> Option<Translated> {
        if self.failed {
            return None;
        }
        let (rid, i) = self.locate(attn_id)?;
        let Request { base, kind } = self.pending.remove(&rid)?;
        let mut pending = kind;
        let mut withdrawn = Vec::new();
        let response = match &mut pending {
            // The only answers an elicitation can get from twr: `cancel`, else `decline`
            // (never `accept`: there is no form content to send).
            Pending::Elicitation => Some(json!({
                "action": if answer.decision.trim() == "cancel" { "cancel" } else { "decline" },
                "content": null,
            })),
            Pending::Permission { tool, input, allow } => Some(if *allow && answer.is_allow() {
                if tool == "ExitPlanMode" {
                    self.plan_exit_approved = true;
                }
                json!({"behavior": "allow", "updatedInput": input})
            } else {
                json!({
                    "behavior": "deny",
                    "message": answer.message.clone()
                        .unwrap_or_else(|| DEFAULT_DENY_MESSAGE.into()),
                })
            }),
            Pending::Question {
                input,
                questions,
                answers,
            } => match question_answer(input, i, &questions[i], answer) {
                None => {
                    let n = answers.len();
                    withdrawn.extend(
                        (0..n)
                            .filter(|&j| j != i && answers[j].is_none())
                            .map(|j| question_attn_id(&base, j, n)),
                    );
                    Some(json!({
                        "behavior": "deny",
                        "message": answer.message.clone()
                            .filter(|m| !m.trim().is_empty())
                            .unwrap_or_else(|| DEFAULT_DECLINE_MESSAGE.into()),
                    }))
                }
                Some(text) => {
                    answers[i] = Some(text);
                    if answers.iter().all(Option::is_some) {
                        let map: Map<String, Value> = questions
                            .iter()
                            .zip(answers.iter())
                            .filter(|(q, _)| !q.is_empty())
                            .map(|(q, a)| (q.clone(), json!(a.as_deref().unwrap_or_default())))
                            .collect();
                        let mut updated = input.as_object().cloned().unwrap_or_default();
                        updated.insert("answers".into(), Value::Object(map));
                        Some(json!({"behavior": "allow", "updatedInput": updated}))
                    } else {
                        None
                    }
                }
            },
        };
        let mut out = Translated::default();
        let Some(response) = response else {
            // Waiting for the other questions of the request.
            self.pending.insert(
                rid,
                Request {
                    base,
                    kind: pending,
                },
            );
            return Some(out);
        };
        for id in withdrawn {
            tracing::info!(attn_id = %id, "claude-code: withdrawing a question of a declined request");
            out.events
                .push(self.withdrawal(id, "sibling_declined", None));
        }
        out.reply = Some(
            json!({
                "type": "control_response",
                "response": {"subtype": "success", "request_id": rid, "response": response},
            })
            .to_string(),
        );
        Some(out)
    }

    /// A stdout line longer than the reader's cap was dropped; `head` is its start and `len`
    /// its length. A `result` still ends the turn (`turn.ended{error}` with
    /// `ext.twr.error = "oversized runtime message"`); anything else is only logged.
    pub fn oversized(&mut self, head: &str, len: usize) -> Translated {
        let mut out = Translated::default();
        if !crate::process::head_has(head, "type", "result") {
            return out;
        }
        let synthetic = json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "stop_reason": "oversized",
        });
        self.on_result(&synthetic, "", &mut out);
        for e in &mut out.events {
            e.raw = None;
            if let EventBody::TurnEnded(t) = &mut e.body {
                t.result = TurnResult::Error;
                let ext = t.ext.get_or_insert_with(JsonMap::new);
                ext.insert(
                    "twr".into(),
                    json!({"error": OVERSIZED_ERROR, "oversized_len": len}),
                );
            }
        }
        out
    }

    /// Translates one stdout line. Lines that are not JSON objects are ignored (logged).
    pub fn translate(&mut self, line: &str) -> Translated {
        let mut out = Translated::default();
        let line = line.trim();
        if line.is_empty() || self.failed {
            return out;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) | Err(_) => {
                tracing::warn!(line = %truncate(line, 200), "claude-code: ignoring non-JSON stdout line");
                return out;
            }
        };
        // A line of the main agent's run: the notification's run started. Not a subagent's
        // line, a task's, a rate limit's, the answer to a request of twr's, nor anything
        // unknown (a `result` is [`Translator::on_result`]'s to judge).
        let kind = str_at(&v, "type");
        let main = v.get("parent_tool_use_id").is_none_or(Value::is_null);
        let run_line = match kind {
            "system" => matches!(str_at(&v, "subtype"), "init" | "api_retry"),
            "stream_event" | "assistant" | "user" | "control_request" => main,
            _ => false,
        };
        if run_line && matches!(self.watch, Some((Watch::NoticeRun, _))) {
            self.watch = None;
        }
        match kind {
            "system" => {
                // Every system message carrying the mode (each turn's `init`, a status
                // change) is checked: a session in a wider mode must not go on.
                if let Some(mode) = v.get("permissionMode").and_then(Value::as_str) {
                    if let Some(why) = self.check_mode(mode) {
                        self.fail(why, line, &mut out);
                        return out;
                    }
                }
                if str_at(&v, "subtype") == "init" {
                    if matches!(self.watch, Some((Watch::Result, _))) {
                        self.watch = None;
                    }
                    // A new run: a `result` now is its own.
                    self.absorb_result = false;
                    self.on_init(&v, line, &mut out);
                    self.read_usage(&mut out);
                }
                self.on_task(&v, line, &mut out);
                // Claude retries a request refused for authentication before giving up.
                if str_at(&v, "subtype") == "api_retry" && str_at(&v, "error") == AUTH_FAILED {
                    self.in_turn();
                    self.auth_failed = true;
                }
            }
            "stream_event" => self.on_stream_event(&v, line, &mut out),
            "assistant" => self.on_assistant(&v, line, &mut out),
            "user" => self.on_user(&v, line, &mut out),
            "control_request" => {
                // Waiting on the user, not on a `result`.
                if matches!(self.watch, Some((Watch::Result, _))) {
                    self.watch = None;
                }
                self.on_control_request(&v, line, &mut out)
            }
            "control_cancel_request" => self.on_cancel(&v, line, &mut out),
            "rate_limit_event" => {
                let status = v
                    .pointer("/rate_limit_info/status")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if status == "rejected" {
                    self.transient_hint = Some("rate_limit_event status rejected".into());
                }
                let windows = rate_limit_windows(&v["rate_limit_info"]);
                if !windows.is_empty() {
                    self.usage.heard((self.clock)(), &windows);
                    out.events.push(Emitted {
                        body: crate::usage::limits(windows, "event", None),
                        turn: None,
                        raw: None,
                    });
                }
            }
            "result" => {
                self.on_result(&v, line, &mut out);
                self.read_usage(&mut out);
            }
            "control_response" => self.on_control_response(&v, &mut out),
            // Anything newer: nothing to show.
            _ => {}
        }
        out
    }

    // --- usage -------------------------------------------------------------------------------

    /// Asks for the plan's usage (`get_usage`) when a read is due.
    fn read_usage(&mut self, out: &mut Translated) {
        let now = (self.clock)();
        if self.failed || !self.usage.due(now) {
            return;
        }
        let n = self.usage.ask(now);
        out.requests.push(
            json!({
                "type": "control_request",
                "request_id": format!("{USAGE_REQUEST}{n}"),
                "request": {"subtype": "get_usage", "skip_behaviors": true},
            })
            .to_string(),
        );
    }

    /// A `control_response`: the answer to a `get_usage` read becomes `runtime.limits`; the
    /// acks of interrupts are ignored.
    fn on_control_response(&mut self, v: &Value, out: &mut Translated) {
        let r = &v["response"];
        if !str_at(r, "request_id").starts_with(USAGE_REQUEST) {
            return;
        }
        if str_at(r, "subtype") != "success" {
            tracing::debug!(error = %str_at(r, "error"), "claude-code: get_usage refused; no more usage reads");
            self.usage.stop();
            return;
        }
        let resp = &r["response"];
        if resp.get("rate_limits_available") == Some(&Value::Bool(false)) {
            // An API key (or cloud provider) session: plan limits never apply.
            self.usage.stop();
            return;
        }
        let windows = usage_read_windows(&resp["rate_limits"]);
        self.usage.answered(&windows);
        if windows.is_empty() {
            return;
        }
        out.events.push(Emitted {
            body: crate::usage::limits(windows, "read", opt_str(resp, "subscription_type")),
            turn: None,
            raw: None,
        });
    }

    // --- permission mode ---------------------------------------------------------------------

    /// Why the session must not go on in the `permissionMode` Claude reports, if it must not:
    /// the report must be acceptable for the allowed mode ([`mode_acceptable`]). The one
    /// widening accepted is plan → an ask-level mode after the user approved an `ExitPlanMode`
    /// request (Claude leaves plan mode then); it becomes the allowed mode and is recorded on
    /// the next `turn.ended` (`ext.claude.mode_changed`, `ext.claude.mode_note`).
    fn check_mode(&mut self, report: &str) -> Option<String> {
        let report = report.trim();
        if mode_acceptable(report, &self.allowed_mode) {
            return None;
        }
        let from_plan = self.allowed_mode.eq_ignore_ascii_case("plan");
        if from_plan && self.plan_exit_approved && mode_level(report) == Some(ModeLevel::Ask) {
            tracing::info!(
                from = %self.allowed_mode,
                to = report,
                "claude-code: the session left plan mode after the user approved the plan"
            );
            self.mode_changed = Some(json!({
                "from": self.allowed_mode,
                "to": report,
                "consent": "ExitPlanMode approved by the user",
            }));
            self.allowed_mode = report.to_string();
            return None;
        }
        Some(format!(
            "Claude Code reports permission mode {report:?}, which allows more than the \
             thread's {:?} (or twr does not know it); the session is ended",
            self.allowed_mode
        ))
    }

    /// The session runs in a mode twr does not accept: the running turn is interrupted, every
    /// waiting turn ends with an error, no further line is read and the driver stops the
    /// process (`fatal`).
    fn fail(&mut self, why: String, line: &str, out: &mut Translated) {
        tracing::error!(%why, "claude-code: refusing a session in a mode twr does not accept");
        if self.busy() {
            self.interrupts_sent += 1;
            out.reply = Some(
                json!({
                    "type": "control_request",
                    "request_id": format!("twr_interrupt_{}", self.interrupts_sent),
                    "request": {"subtype": "interrupt"},
                })
                .to_string(),
            );
        }
        let mut turns: Vec<u64> = Vec::new();
        if self.active {
            turns.extend(self.turn);
        }
        turns.extend(self.queued.drain(..));
        self.active = false;
        self.turn = None;
        self.runtime_turn = None;
        self.failed = true;
        for turn in turns {
            let mut claude = Map::new();
            claude.insert("error".into(), json!({"message": why}));
            claude.insert("transient".into(), json!(false));
            let mut ext = ext_of(claude);
            ext.get_or_insert_with(JsonMap::new)
                .insert("twr".into(), json!({ "error": error_line(&why) }));
            out.events.push(Emitted {
                body: EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Error,
                    session_id: self.session_id.clone(),
                    ext,
                    ..Default::default()
                }),
                turn: Some(turn),
                raw: Some(line.to_string()),
            });
        }
        out.fatal = Some(why);
    }

    // --- turn bookkeeping --------------------------------------------------------------------

    /// The turn in-turn events belong to, opening the next queued turn if none is running.
    fn in_turn(&mut self) -> Option<u64> {
        if !self.active {
            self.active = true;
            self.turn = self.queued.pop_front();
            self.transient_hint = None;
            self.auth_failed = false;
            self.api_error = None;
            self.interrupted_marker = false;
        }
        self.turn
    }

    /// Opens a turn of the runtime's own when it works with no turn of twr's running or
    /// queued, or in a run its `init` started with no turn of twr's (a prompt written since
    /// waits for that run's `result`). The hub numbers it: its events, `turn.ended` included,
    /// carry no turn, and its start and end carry its id (`Adapter`).
    fn start_runtime_turn(&mut self, out: &mut Translated, raw: Option<&str>) {
        if self.started && self.runtime_turn.is_none() && self.own_run() {
            self.turn = None;
            self.active = true;
            self.runtime_turns += 1;
            self.runtime_turn = Some(self.runtime_turns);
            let mut ext = None;
            set_twr_ext(
                &mut ext,
                RUNTIME_TURN_KEY,
                json!(self.runtime_turns.to_string()),
            );
            out.events.push(Emitted {
                body: EventBody::TurnStarted(TurnStarted {
                    from: TurnFrom::Runtime,
                    prompt: "[runtime resumed after background activity]".into(),
                    ext,
                    ..Default::default()
                }),
                turn: None,
                raw: raw.map(str::to_string),
            });
        }
    }

    /// Whether what Claude does now is no turn of twr's: no twr turn runs and none is queued,
    /// or the running one is a run its `init` started with none (a prompt written since waits
    /// for that run's `result`).
    fn own_run(&self) -> bool {
        match self.active {
            true => self.turn.is_none(),
            false => self.queued.is_empty(),
        }
    }

    fn push(&mut self, out: &mut Translated, body: EventBody, raw: Option<&str>) {
        let child = raw
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .is_some_and(|v| v.get("parent_tool_use_id").is_some_and(|p| !p.is_null()));
        // A child agent's output belongs to the parent's twr turn when one runs, and never
        // opens one of the runtime's own.
        if child && self.own_run() {
            out.events.push(Emitted {
                body,
                turn: None,
                raw: raw.map(str::to_string),
            });
            return;
        }
        if matches!(
            body,
            EventBody::AgentText(_) | EventBody::AgentThinking(_) | EventBody::AgentToolCall(_)
        ) {
            // The model works for the parent session, in a turn of twr's or its own.
            self.model_ran |= !child;
            if !child {
                self.absorb_result = false;
                self.notice_quiet = false;
            }
            self.start_runtime_turn(out, raw);
        }
        let turn = match &body {
            EventBody::ThreadStarted(_) => None,
            _ if self.active || !self.queued.is_empty() => self.in_turn(),
            _ => None,
        };
        out.events.push(Emitted {
            body,
            turn,
            raw: raw.map(str::to_string),
        });
    }

    // --- handlers ----------------------------------------------------------------------------

    fn task(
        &mut self,
        id: &str,
        running: bool,
        description: &str,
        progress: bool,
        line: &str,
        out: &mut Translated,
    ) {
        let event = if progress {
            self.tasks.progress(id, description)
        } else {
            self.tasks.update(id, running, description)
        };
        if let Some(task) = event {
            out.events.push(Emitted {
                body: EventBody::TaskUpdated(task),
                turn: None,
                raw: Some(line.into()),
            });
        }
    }

    fn on_task(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let subtype = str_at(v, "subtype");
        if !matches!(
            subtype,
            "task_started" | "task_progress" | "task_updated" | "task_notification"
        ) {
            return;
        }
        let status = v
            .pointer("/patch/status")
            .and_then(Value::as_str)
            .unwrap_or_else(|| str_at(v, "status"));
        let description = v
            .pointer("/patch/description")
            .and_then(Value::as_str)
            .unwrap_or_else(|| str_at(v, "description"));
        let terminal = crate::background::terminal(status);
        let id = str_at(v, "task_id");
        // Whose task it is: one the main agent put in the background notifies it when it ends
        // (a run of Claude's own); a subagent's, or one in the foreground of a turn, does not.
        if subtype == "task_started" && !id.is_empty() {
            if v.get("owned_by_subagent").and_then(Value::as_bool) == Some(true) {
                self.subagent_tasks.insert(id.to_string());
            } else if v.get("is_backgrounded").and_then(Value::as_bool) == Some(true) {
                self.notifying.insert(id.to_string());
            }
        }
        if v.pointer("/patch/is_backgrounded").and_then(Value::as_bool) == Some(true)
            && !self.subagent_tasks.contains(id)
            && !id.is_empty()
        {
            self.notifying.insert(id.to_string());
        }
        // A resumed subagent can reuse its task id. An explicit new launch overrides the
        // terminal tombstone; late progress alone never does.
        if subtype == "task_started"
            && !terminal
            && !self.tasks.running.contains_key(str_at(v, "task_id"))
        {
            self.tasks.reopen(str_at(v, "task_id"));
        }
        if subtype == "task_updated"
            && status.is_empty()
            && !self.tasks.running.contains_key(str_at(v, "task_id"))
        {
            return;
        }
        self.task(
            str_at(v, "task_id"),
            !terminal,
            description,
            subtype == "task_progress",
            line,
            out,
        );
        if terminal {
            self.subagent_tasks.remove(id);
            if self.notifying.remove(id) {
                self.open_notice_turn(out, line);
            }
        }
    }

    /// A background task of the main agent's ended: Claude starts a run of its own for its
    /// notification. With no turn running or queued, the runtime's turn opens now, not at the
    /// run's first output (see the module docs).
    fn open_notice_turn(&mut self, out: &mut Translated, line: &str) {
        if !self.started || self.busy() || self.failed {
            return;
        }
        self.start_runtime_turn(out, Some(line));
        if self.runtime_turn.is_some() {
            self.notice_quiet = true;
            let after = self.opts.notice_run_after.unwrap_or(NOTICE_RUN_AFTER);
            self.watch = Some((Watch::NoticeRun, (self.clock)() + chrono_of(after)));
        }
    }

    /// When [`Translator::expire`] has something to do (a turn's `result` or a notification's
    /// run awaited), if anything is awaited.
    pub fn deadline(&self) -> Option<DateTime<Utc>> {
        self.watch.map(|(_, at)| at)
    }

    /// What is awaited did not come by `now` (see the module docs): a turn whose `result` is
    /// missing ends `completed` with `ext.twr.missing_result`; one opened for a notification
    /// whose run never started ends with `ext.twr.no_run`. Nothing while the user is asked
    /// something.
    pub fn expire(&mut self, now: DateTime<Utc>) -> Translated {
        let mut out = Translated::default();
        let Some((watch, at)) = self.watch else {
            return out;
        };
        if now < at {
            return out;
        }
        self.watch = None;
        if !self.active || !self.pending.is_empty() || self.failed {
            return out;
        }
        match watch {
            Watch::Result => {
                tracing::warn!(turn = ?self.turn, runtime_turn = ?self.runtime_turn, queued = ?self.queued, "claude-code: no `result` came after the turn's last message; ending the turn");
                self.end_without_result(MISSING_RESULT_KEY, &mut out);
                // Prompts written during that run that Claude never started: it folded them
                // into the run, which answered them. Their turns end with it.
                while !self.queued.is_empty() {
                    self.end_without_result(MISSING_RESULT_KEY, &mut out);
                }
                self.absorb_result = true;
            }
            Watch::NoticeRun if self.turn.is_none() && self.runtime_turn.is_some() => {
                tracing::info!(runtime_turn = ?self.runtime_turn, "claude-code: no run followed a background task's notification; ending the runtime's turn");
                self.end_without_result(NO_RUN_KEY, &mut out);
            }
            Watch::NoticeRun => {}
        }
        out
    }

    /// Ends the running turn without a `result` of Claude's: `completed`, no usage, and
    /// `ext.twr.<why> = true`.
    fn end_without_result(&mut self, why: &str, out: &mut Translated) {
        self.notice_quiet = false;
        let model_ran = self.model_ran;
        let synthetic = json!({"type": "result", "subtype": "success", "stop_reason": "end_turn"});
        let mut ended = Translated::default();
        self.end_turn_with(&synthetic, "", &mut ended);
        self.model_ran = model_ran;
        for mut e in ended.events {
            e.raw = None;
            if let EventBody::TurnEnded(t) = &mut e.body {
                t.usage = None;
                if let Some(claude) = t.ext.as_mut().and_then(|x| x.get_mut(EXT)) {
                    if let Some(c) = claude.as_object_mut() {
                        c.retain(|k, _| k == "mode_changed" || k == "mode_note");
                    }
                }
                set_twr_ext(&mut t.ext, why, json!(true));
            }
            out.events.push(e);
        }
    }

    fn on_init(&mut self, v: &Value, line: &str, out: &mut Translated) {
        self.in_turn();
        if self.started {
            return;
        }
        self.started = true;
        let session_id = opt_str(v, "session_id");
        self.session_id = session_id.clone();
        let mut twr = self.harness_metadata.clone();
        if let Some(pid) = self.opts.pid {
            twr.insert("pid".into(), json!(pid));
        }
        if let Some(exe) = &self.opts.exe {
            twr.insert("exe".into(), json!(exe));
        }
        let mut claude = Map::new();
        for (from, to) in [
            ("model", "model"),
            ("permissionMode", "permission_mode"),
            ("claude_code_version", "version"),
        ] {
            if let Some(s) = v.get(from).filter(|x| !x.is_null()) {
                claude.insert(to.into(), s.clone());
            }
        }
        claude.insert("resumed".into(), json!(self.opts.resumed));
        let mut ext = JsonMap::new();
        if !twr.is_empty() {
            ext.insert("twr".into(), Value::Object(twr));
        }
        ext.insert(EXT.into(), Value::Object(claude));
        let body = EventBody::ThreadStarted(SessionPayload {
            session_id,
            ext: Some(ext),
            ..Default::default()
        });
        self.push(out, body, Some(line));
    }

    fn on_stream_event(&mut self, v: &Value, line: &str, out: &mut Translated) {
        if !v["parent_tool_use_id"].is_null() {
            return;
        }
        let ev = &v["event"];
        if str_at(ev, "type") == "message_start" {
            self.model_ran = true;
            self.absorb_result = false;
            self.notice_quiet = false;
            if matches!(self.watch, Some((Watch::Result, _))) {
                self.watch = None;
            }
            self.start_runtime_turn(out, Some(line));
        }
        // The main agent's message stopped for good: the run's `result` is due.
        if str_at(ev, "type") == "message_delta" && self.active {
            let stop = ev.pointer("/delta/stop_reason").and_then(Value::as_str);
            if stop.is_some_and(|r| !matches!(r, "tool_use" | "pause_turn")) {
                let after = self
                    .opts
                    .missing_result_after
                    .unwrap_or(MISSING_RESULT_AFTER);
                self.watch = Some((Watch::Result, (self.clock)() + chrono_of(after)));
            }
        }
        if str_at(ev, "type") != "content_block_delta" {
            return;
        }
        let delta = &ev["delta"];
        if str_at(delta, "type") == "text_delta" {
            let text = str_at(delta, "text");
            if !text.is_empty() {
                let body = EventBody::AgentText(AgentText {
                    text: text.into(),
                    delta: true,
                    ..Default::default()
                });
                self.push(out, body, None);
            }
        }
    }

    fn on_assistant(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let parent = opt_str(v, "parent_tool_use_id");
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            if matches!(e, "rate_limit" | "server_error" | "overloaded") {
                self.transient_hint = Some(format!("assistant error {e}"));
            }
            if e == AUTH_FAILED {
                self.in_turn();
                self.auth_failed = true;
            }
            let text = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|b| str_at(b, "text"))
                .filter(|t| !t.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            // A subagent's own API error is not the turn's cause.
            if !text.is_empty() && parent.is_none() {
                self.in_turn();
                self.api_error = Some(text);
            }
        }
        let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        for block in blocks {
            match str_at(block, "type") {
                "text" if parent.is_none() => {
                    let text = str_at(block, "text");
                    if !text.is_empty() {
                        let body = EventBody::AgentText(AgentText {
                            text: text.into(),
                            is_final: true,
                            ..Default::default()
                        });
                        self.push(out, body, Some(line));
                    }
                }
                "thinking" if parent.is_none() && self.opts.thinking => {
                    let text = str_at(block, "thinking");
                    if !text.is_empty() {
                        let body = EventBody::AgentThinking(AgentThinking {
                            text: text.into(),
                            ..Default::default()
                        });
                        self.push(out, body, Some(line));
                    }
                }
                "tool_use" => {
                    let id = str_at(block, "id").to_string();
                    let tool = str_at(block, "name").to_string();
                    if id.is_empty() || self.tools.contains_key(&id) {
                        continue;
                    }
                    self.tools.insert(id.clone(), tool.clone());
                    let input = block.get("input").cloned().unwrap_or(Value::Null);
                    let destructive = is_destructive(&input);
                    let mut claude = Map::new();
                    if let Some(p) = &parent {
                        claude.insert("parent_tool_use_id".into(), json!(p));
                    }
                    if destructive {
                        claude.insert("destructive".into(), json!(true));
                    }
                    let mut ext = ext_of(claude);
                    if let Some(purpose) = opt_str(&input, "description").filter(|_| tool == "Bash")
                    {
                        set_twr_ext(&mut ext, PURPOSE_KEY, json!(one_line(&purpose, 200)));
                    }
                    if parent.is_none() {
                        if let Some(step) = self.steps.call(&id, &tool, &input) {
                            set_twr_ext(&mut ext, STEP_KEY, json!(one_line(&step, 200)));
                        }
                    }
                    let body = EventBody::AgentToolCall(AgentToolCall {
                        call_id: id,
                        input_summary: input_summary(&tool, &input),
                        input_ref: None,
                        risky: RISKY_TOOLS.contains(&tool.as_str()) || destructive,
                        tool,
                        ext,
                        ..Default::default()
                    });
                    self.push(out, body, Some(line));
                }
                _ => {}
            }
        }
    }

    fn on_user(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let parent = opt_str(v, "parent_tool_use_id");
        let content = v.pointer("/message/content").unwrap_or(&Value::Null);
        // Older CLIs can deliver terminal notifications as injected user text.
        let text = match content {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => String::new(),
        };
        if parent.is_none() && text.trim_start().starts_with("<task-notification>") {
            let tag = |name: &str| -> Option<&str> {
                text.split_once(&format!("<{name}>"))?
                    .1
                    .split_once(&format!("</{name}>"))
                    .map(|(s, _)| s.trim())
            };
            if let (Some(id), Some(status)) = (tag("task-id"), tag("status")) {
                if crate::background::terminal(status) {
                    self.task(id, false, "", false, line, out);
                }
            }
        }
        // Bash and Agent tools return explicit task ids after successfully launching work.
        let result = &v["tool_use_result"];
        if result.get("isAsync").and_then(Value::as_bool) == Some(true) {
            if let Some(id) = result.get("agentId").and_then(Value::as_str) {
                self.task(id, true, "background subagent", false, line, out);
            }
        }
        if let Some(id) = ["backgroundTaskId", "task_id"]
            .iter()
            .find_map(|key| result.get(*key).and_then(Value::as_str))
        {
            let status = str_at(result, "status");
            self.task(
                id,
                !crate::background::terminal(status),
                "background task",
                false,
                line,
                out,
            );
        }
        if let Some(task) = result.get("task") {
            let status = str_at(task, "status");
            if !status.is_empty() {
                self.task(
                    str_at(task, "task_id"),
                    !crate::background::terminal(status),
                    "",
                    false,
                    line,
                    out,
                );
            }
        }
        let marker = |s: &str| s.trim_start().starts_with(INTERRUPTED_MARKER);
        let interrupted = match content {
            Value::String(s) => marker(s),
            Value::Array(blocks) => blocks
                .iter()
                .any(|b| str_at(b, "type") == "text" && marker(str_at(b, "text"))),
            _ => false,
        };
        if interrupted && parent.is_none() {
            self.in_turn();
            self.interrupted_marker = true;
        }
        let Some(blocks) = content.as_array() else {
            return;
        };
        for block in blocks {
            if str_at(block, "type") != "tool_result" {
                continue;
            }
            let call_id = str_at(block, "tool_use_id").to_string();
            let is_error = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut claude = Map::new();
            if let Some(tool) = self.tools.get(&call_id) {
                claude.insert("tool".into(), json!(tool));
            }
            if !is_error {
                self.steps.result(&call_id, &block["content"]);
            }
            if let Some(p) = &parent {
                claude.insert("parent_tool_use_id".into(), json!(p));
            }
            let body = EventBody::AgentToolResult(AgentToolResult {
                call_id,
                ok: !is_error,
                output_summary: output_summary(&block["content"]),
                output_ref: None,
                ext: ext_of(claude),
                ..Default::default()
            });
            self.push(out, body, Some(line));
        }
    }

    fn on_control_request(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let request_id = str_at(v, "request_id").to_string();
        let req = &v["request"];
        let subtype = str_at(req, "subtype");
        // Open items never change: a request id that is still pending was replaced.
        if let Some(old) = self.pending.remove(&request_id) {
            self.supersede(&request_id, &old, line, out);
        }
        if !request_id.is_empty() && is_elicitation(subtype) {
            self.on_elicitation(&request_id, req, line, out);
            return;
        }
        if subtype != "can_use_tool" || request_id.is_empty() {
            tracing::warn!(
                subtype,
                "claude-code: answering an unsupported control request with an error"
            );
            // Recorded, so the error reply is never silent: an item raised and closed at once.
            let attn_id = if request_id.is_empty() {
                format!("unsupported-{}", uuid::Uuid::new_v4())
            } else {
                self.fresh_base(&format!("unsupported-{request_id}"), 1)
            };
            let mut claude = Map::new();
            claude.insert("subtype".into(), json!(subtype));
            claude.insert("request_id".into(), json!(request_id));
            claude.insert("unsupported".into(), json!(true));
            let turn = if self.active { self.turn } else { None };
            out.events.push(Emitted {
                body: EventBody::AttentionRaised(AttentionRaised {
                    attn_id: attn_id.clone(),
                    reason: AttentionReason::Error,
                    summary: one_line(
                        &format!(
                            "Claude Code sent a control request twr does not handle \
                             ({subtype:?}); twr answered it with an error"
                        ),
                        400,
                    ),
                    ext: ext_of(claude),
                    ..Default::default()
                }),
                turn,
                raw: Some(line.to_string()),
            });
            out.events
                .push(self.withdrawal(attn_id, "unsupported_request", None));
            out.reply = Some(
                json!({
                    "type": "control_response",
                    "response": {
                        "subtype": "error",
                        "request_id": request_id,
                        "error": format!("twr does not handle control request {subtype:?}"),
                    },
                })
                .to_string(),
            );
            return;
        }
        let tool = str_at(req, "tool_name").to_string();
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let call_id = opt_str(req, "tool_use_id");
        let mut claude = Map::new();
        claude.insert("tool".into(), json!(tool));
        if req
            .get("requires_user_interaction")
            .and_then(Value::as_bool)
            == Some(true)
        {
            claude.insert("requires_user_interaction".into(), json!(true));
        }
        // Only AskUserQuestion is a question. Any other tool (an MCP tool whose input has a
        // `questions` array, ExitPlanMode with `requires_user_interaction`) is an allow/deny
        // permission: relaying an arbitrary answer to it must never turn into an allow.
        if tool == "AskUserQuestion" {
            let mut questions: Vec<String> = input["questions"]
                .as_array()
                .map(|qs| {
                    qs.iter()
                        .map(|q| str_at(q, "question").to_string())
                        .collect()
                })
                .unwrap_or_default();
            if questions.is_empty() {
                questions.push(String::new());
            }
            let n = questions.len();
            let base = self.fresh_base(&request_id, n);
            for (i, q) in questions.iter().enumerate() {
                let mut claude = claude.clone();
                let spec = input
                    .pointer(&format!("/questions/{i}"))
                    .unwrap_or(&Value::Null);
                if let Some(h) = spec.get("header").and_then(Value::as_str) {
                    claude.insert("header".into(), json!(h));
                }
                if spec.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                    claude.insert("multi_select".into(), json!(true));
                }
                if n > 1 {
                    claude.insert("request_id".into(), json!(request_id));
                    claude.insert("question_index".into(), json!(i));
                    claude.insert("question_count".into(), json!(n));
                }
                let text = if q.is_empty() {
                    format!("{tool} asks a question")
                } else {
                    q.clone()
                };
                let summary = if n > 1 {
                    one_line(&format!("({}/{n}) {text}", i + 1), 400)
                } else {
                    one_line(&text, 400)
                };
                let body = AttentionRaised {
                    attn_id: question_attn_id(&base, i, n),
                    reason: AttentionReason::Question,
                    summary,
                    options: question_labels_at(&input, i),
                    call_id: call_id.clone(),
                    ext: ext_of(claude),
                    ..Default::default()
                };
                self.push(out, EventBody::AttentionRaised(body), Some(line));
            }
            self.pending.insert(
                request_id,
                Request {
                    base,
                    kind: Pending::Question {
                        input,
                        answers: vec![None; n],
                        questions,
                    },
                },
            );
            return;
        }
        for key in ["description", "blocked_path"] {
            if let Some(s) = req.get(key).and_then(Value::as_str) {
                claude.insert(key.into(), json!(s));
            }
        }
        // As it runs: its newlines and whitespace visible, the input's other fields after it
        // (a flag such as `dangerouslyDisableSandbox`, a write's content), and all of it in the
        // item's context when the summary shows only its start ([`render_request`]). The tool
        // name is the runtime's too: marked the same way.
        let (subject, extras) = card_input(&tool, &input);
        let what = render_request(&subject, extras.as_ref(), PERMISSION_INPUT_MAX);
        if let Some(full) = &what.full {
            claude.insert(FULL_INPUT_KEY.into(), json!(full));
        }
        let label = mark_visible(&tool);
        let mut summary = if what.line.is_empty() {
            format!("Allow {label}?")
        } else {
            format!("Allow {label}: {}", what.line)
        };
        let allow = !what.too_long;
        let options = if allow {
            vec!["allow".into(), "deny".into()]
        } else {
            summary = format!("[deny only: the input is too long to show in full] {summary}");
            vec!["deny".into()]
        };
        let base = self.fresh_base(&request_id, 1);
        let body = AttentionRaised {
            attn_id: base.clone(),
            reason: AttentionReason::Permission,
            summary,
            options,
            call_id,
            ext: ext_of(claude),
            ..Default::default()
        };
        self.pending.insert(
            request_id,
            Request {
                base,
                kind: Pending::Permission { tool, input, allow },
            },
        );
        self.push(out, EventBody::AttentionRaised(body), Some(line));
    }

    /// An MCP elicitation (`control_request{subtype: elicitation}`: a form to fill or a URL to
    /// open). twr has no form UI, so it becomes a question whose only answers are `decline`
    /// and `cancel`; nothing is sent until the user picks one.
    fn on_elicitation(&mut self, request_id: &str, req: &Value, line: &str, out: &mut Translated) {
        let server = ["mcp_server_name", "server_name", "serverName"]
            .iter()
            .find_map(|k| opt_str(req, k))
            .unwrap_or_default();
        let message = opt_str(req, "message").unwrap_or_default();
        let url = opt_str(req, "url");
        let mut claude = Map::new();
        claude.insert("subtype".into(), json!(str_at(req, "subtype")));
        claude.insert("elicitation".into(), json!(true));
        if !server.is_empty() {
            claude.insert("server".into(), json!(server));
        }
        for (from, to) in [
            ("mode", "mode"),
            ("url", "url"),
            ("elicitation_id", "elicitation_id"),
            ("requested_schema", "requested_schema"),
        ] {
            if let Some(x) = req.get(from).filter(|x| !x.is_null()) {
                claude.insert(to.into(), x.clone());
            }
        }
        let who = if server.is_empty() {
            "An MCP server".to_string()
        } else {
            format!("MCP server {server}")
        };
        let mut summary = if message.is_empty() {
            format!("{who} asks for input twr cannot show")
        } else {
            format!("{who}: {message}")
        };
        if let Some(u) = &url {
            summary.push_str(&format!(" ({u})"));
        }
        summary.push_str(" [twr cannot fill forms: decline or cancel]");
        let base = self.fresh_base(request_id, 1);
        let body = AttentionRaised {
            attn_id: base.clone(),
            reason: AttentionReason::Question,
            summary: one_line(&summary, 400),
            options: vec!["decline".into(), "cancel".into()],
            ext: ext_of(claude),
            ..Default::default()
        };
        self.pending.insert(
            request_id.to_string(),
            Request {
                base,
                kind: Pending::Elicitation,
            },
        );
        self.push(out, EventBody::AttentionRaised(body), Some(line));
    }

    /// Claude withdrew a pending `can_use_tool` (e.g. the turn was interrupted): the item can
    /// no longer be answered, so it is resolved as timed out.
    fn on_cancel(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let id = str_at(v, "request_id");
        let Some(r) = self.pending.remove(id) else {
            return;
        };
        tracing::info!(
            request_id = id,
            "claude-code: runtime cancelled a pending request"
        );
        for attn_id in r.open_ids() {
            out.events
                .push(self.withdrawal(attn_id, "runtime", Some(line)));
        }
    }

    fn on_result(&mut self, v: &Value, line: &str, out: &mut Translated) {
        // The run of a turn ended for its missing `result` ends after all: absorbed, whatever
        // is open or queued now (no run showed since: no `init`, no model output; a turn opened
        // since for another task's notification is still waiting for its run). Its spend was
        // that turn's (reported unknown): the total moves on, charged to no other turn.
        if std::mem::take(&mut self.absorb_result) {
            if let Some(t) = v.get("total_cost_usd").and_then(Value::as_f64) {
                self.cost_since(t);
            }
            return;
        }
        // Claude consumed a task notification without asking the model anything (a resumed
        // session hears of a task its previous process left behind before it reads the
        // prompt). That `result` answers no prompt and ends no turn: a twr turn running goes
        // on with what it saw so far (an interruption, an error's cause) and keeps waiting for
        // its own `result`, and so does a runtime turn that showed output; only an `init` that
        // opened neither is undone.
        if v.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
            && v.get("num_turns").and_then(Value::as_u64) == Some(0)
        {
            if self.active && self.turn.is_none() && self.runtime_turn.is_none() {
                self.active = false;
            }
            // The turn opened for a notification whose run never showed: nothing more comes
            // for it.
            if self.notice_quiet
                && self.active
                && self.turn.is_none()
                && self.runtime_turn.is_some()
            {
                self.watch = None;
                self.end_without_result(NO_RUN_KEY, out);
            }
            if let Some(t) = v.get("total_cost_usd").and_then(Value::as_f64) {
                self.skip_cost(t);
            }
            return;
        }
        self.watch = None;
        self.notice_quiet = false;
        self.end_turn_with(v, line, out);
    }

    /// Ends the running turn (or the queued one it opens) as `result` `v` says.
    fn end_turn_with(&mut self, v: &Value, line: &str, out: &mut Translated) {
        // A run of the runtime's own that showed nothing before its `result` (an API error).
        self.start_runtime_turn(out, Some(line));
        let turn = self.in_turn();
        self.active = false;
        self.turn = None;
        self.model_ran = true;
        let runtime_turn = self.runtime_turn.take();
        // Tool ids are per turn: nothing refers to them once the turn is over.
        self.tools.clear();
        let interrupted = std::mem::take(&mut self.interrupt_requested);
        let marker = std::mem::take(&mut self.interrupted_marker);
        let hint = self.transient_hint.take();
        let auth_failed = std::mem::take(&mut self.auth_failed);
        let api_error = self.api_error.take();

        let subtype = str_at(v, "subtype");
        let is_error = v.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        // After our own interrupt, an error result is the interrupt when any of Claude's
        // interrupt signs is there (`terminal_reason` is not present in every version).
        let aborted = str_at(v, "terminal_reason").starts_with("aborted")
            || v.get("stop_reason").is_none_or(Value::is_null)
            || marker;
        let result = if interrupted && is_error && aborted {
            TurnResult::Interrupted
        } else if is_error || subtype.starts_with("error") {
            TurnResult::Error
        } else {
            TurnResult::Completed
        };

        // Cost: cumulative per process. An interrupted turn's spend is not in the total (it
        // stays put, or reads 0 in a fresh process), and an errored turn reporting 0 did not
        // report at all: both are unknown, and the running total is left alone.
        let total = v.get("total_cost_usd").and_then(Value::as_f64);
        let cost = match total {
            Some(_) if result == TurnResult::Interrupted => None,
            Some(t) if t == 0.0 && result != TurnResult::Completed => None,
            Some(t) => {
                let delta = self.cost_since(t) + std::mem::take(&mut self.uncharged_cost);
                Some((delta * 1e10).round() / 1e10)
            }
            None => None,
        };
        let u = &v["usage"];
        let tok = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
        let usage = Usage {
            input: tok("input_tokens")
                + tok("cache_creation_input_tokens")
                + tok("cache_read_input_tokens"),
            output: tok("output_tokens"),
            cost,
        };

        let mut claude = Map::new();
        claude.insert("subtype".into(), json!(subtype));
        for key in [
            "num_turns",
            "stop_reason",
            "terminal_reason",
            "permission_denials",
            "total_cost_usd",
            "api_error_status",
            "errors",
        ] {
            if let Some(x) = v.get(key).filter(|x| !x.is_null()) {
                claude.insert(key.into(), x.clone());
            }
        }
        for key in ["cache_read_input_tokens", "cache_creation_input_tokens"] {
            claude.insert(key.into(), json!(tok(key)));
        }
        if let Some(change) = self.mode_changed.take() {
            claude.insert("mode_changed".into(), change);
            claude.insert("mode_note".into(), json!(PLAN_EXIT_NOTE));
        }
        // Every failed turn says why (`ext.twr.error`, what the hub quotes). Not signed in (or
        // the login expired): said with what to do, and never transient (retrying cannot help
        // until the user signs in).
        let mut twr_error = None;
        if result == TurnResult::Error && (auth_failed || auth_failure(v)) {
            let text = error_text(v, api_error.as_deref())
                .unwrap_or_else(|| "Claude Code could not authenticate".into());
            twr_error = Some(format!("{}{AUTH_HINT}", error_line(&text)));
            claude.insert("auth_failed".into(), json!(true));
            claude.insert("transient".into(), json!(false));
        } else if result == TurnResult::Error {
            let text = error_text(v, api_error.as_deref())
                .unwrap_or_else(|| "Claude Code reported an error without details".into());
            twr_error = Some(error_line(&text));
            let reason = transient_reason(v).or(hint);
            claude.insert("transient".into(), json!(reason.is_some()));
            if let Some(r) = reason {
                claude.insert("transient_reason".into(), json!(r));
            }
        }
        let mut ext = ext_of(claude);
        if let Some(e) = twr_error {
            ext.get_or_insert_with(JsonMap::new)
                .insert("twr".into(), json!({ "error": e }));
        }
        if let Some(id) = runtime_turn {
            set_twr_ext(&mut ext, RUNTIME_TURN_KEY, json!(id.to_string()));
        }
        let session_id = opt_str(v, "session_id").or_else(|| self.session_id.clone());
        out.events.push(Emitted {
            body: EventBody::TurnEnded(TurnEnded {
                result,
                session_id,
                usage: Some(usage),
                duration_ms: v.get("duration_ms").and_then(Value::as_u64),
                ext,
                ..Default::default()
            }),
            turn,
            raw: Some(line.to_string()),
        });
    }

    /// The spend cumulative total `t` adds since the last one, which `t` replaces: all of it
    /// when it went down (the count started again).
    fn cost_since(&mut self, t: f64) -> f64 {
        let delta = if t >= self.last_total_cost {
            t - self.last_total_cost
        } else {
            t
        };
        self.last_total_cost = t;
        delta
    }

    /// Cumulative total `t` of a `result` that asked the model nothing: before any run reached
    /// the model in this process, the session's spend before it (the baseline); after, spend
    /// the next charged turn gets.
    fn skip_cost(&mut self, t: f64) {
        if self.model_ran {
            self.uncharged_cost += self.cost_since(t);
        } else {
            self.last_total_cost = t;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Plan steps
// ---------------------------------------------------------------------------------------------

/// The main agent's plan, as its `TodoWrite`, `TaskCreate` and `TaskUpdate` calls keep it: for
/// the step it has in progress (`ext.twr.step`, see the module's doc).
#[derive(Debug, Default)]
struct Steps {
    /// `TaskCreate` calls whose result (the new task's id) has not come yet: call id → the
    /// task's step.
    creating: HashMap<String, String>,
    /// Task id → its step (`activeForm`, else `subject`).
    tasks: HashMap<String, String>,
    /// The task in progress, by id.
    current: Option<String>,
}

impl Steps {
    /// Notes the call `id` of `tool` with `input`; returns the step in progress when it changes
    /// it (empty: none is).
    fn call(&mut self, id: &str, tool: &str, input: &Value) -> Option<String> {
        let step_of = |v: &Value, name: &str| opt_str(v, "activeForm").or_else(|| opt_str(v, name));
        match tool {
            "TodoWrite" => {
                let todos = input.get("todos").and_then(Value::as_array)?;
                let doing = todos.iter().find(|t| str_at(t, "status") == "in_progress");
                Some(
                    doing
                        .and_then(|t| step_of(t, "content"))
                        .unwrap_or_default(),
                )
            }
            "TaskCreate" => {
                if let Some(step) = step_of(input, "subject") {
                    self.creating.insert(id.to_string(), step);
                }
                None
            }
            "TaskUpdate" => {
                let task = input
                    .get("taskId")
                    .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))?;
                let reworded = step_of(input, "subject");
                if let Some(step) = &reworded {
                    self.tasks.insert(task.clone(), step.clone());
                }
                let current = self.current.as_deref() == Some(task.as_str());
                match str_at(input, "status") {
                    "in_progress" => {
                        self.current = Some(task.clone());
                        Some(self.tasks.get(&task).cloned().unwrap_or_default())
                    }
                    // The task in progress reworded: its new words.
                    "" if current => reworded,
                    "" => None,
                    // It left `in_progress` (completed, deleted, back to pending): none is.
                    _ if current => {
                        self.current = None;
                        Some(String::new())
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Notes the result `content` of call `id`: a `TaskCreate`'s says the new task's id
    /// (`Task #3 created successfully: …`).
    fn result(&mut self, id: &str, content: &Value) {
        let Some(step) = self.creating.remove(id) else {
            return;
        };
        let text = output_summary(content);
        let digits: String = text
            .split_once('#')
            .map(|(_, rest)| rest.chars().take_while(char::is_ascii_digit).collect())
            .unwrap_or_default();
        if !digits.is_empty() {
            self.tasks.insert(digits, step);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// Whether a control request subtype asks the user for input (MCP elicitations).
fn is_elicitation(subtype: &str) -> bool {
    subtype.to_ascii_lowercase().contains("elicit")
}

fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn ext_of(claude: Map<String, Value>) -> Option<JsonMap> {
    if claude.is_empty() {
        return None;
    }
    let mut m = JsonMap::new();
    m.insert(EXT.into(), Value::Object(claude));
    Some(m)
}

/// `s` cut to `max_chars` chars, `…` added when cut.
pub fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// Collapses whitespace runs to single spaces and truncates to `max_chars`.
pub fn one_line(s: &str, max_chars: usize) -> String {
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&joined, max_chars)
}

/// The longest whole input of a request kept for the user ([`RenderedCommand::full`]), in
/// bytes of its marked form: past it, only `deny` is offered.
pub const FULL_INPUT_MAX: usize = 64 * 1024;

/// The key, in a runtime's `ext` section of a permission, of the request's whole input as
/// [`render_command`] (for Claude Code, [`render_request`]) marks it, when the summary shows
/// only its start: the item's
/// `context.input` (`twr_core::reduce::RequestContext`), which `twr respond` and the TUI's
/// request popup show where the user approves.
pub const FULL_INPUT_KEY: &str = "full_input";

/// A command (or other input the user judges a request by) as an approval card shows it
/// ([`render_command`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedCommand {
    /// For the summary: all of it, or (past `max_chars`) its start, cut between marks, then
    /// `… N more characters; open the card to see all`.
    pub line: String,
    /// All of it, when `line` is cut: for the item's context ([`FULL_INPUT_KEY`]). Itself cut
    /// at [`FULL_INPUT_MAX`] (`too_long`), ending `… N more characters not shown`.
    pub full: Option<String>,
    /// Even the whole input is past [`FULL_INPUT_MAX`]: the user cannot see all of what an
    /// `allow` would run, so only `deny` may be offered.
    pub too_long: bool,
}

/// A command (or other text the user judges a request by: an approval card's summary) on one
/// line, as it runs. Unlike [`one_line`], whitespace is kept as it is, nothing trimmed: quoted
/// spaces mean something, collapsing a newline made `# note` and the line after it read as
/// one comment, and a trailing space can matter (`value\ `, a directory `/safe `).
///
/// What shows as itself, by a rule that needs no Unicode tables: printable ASCII (`!` to
/// `~`), the ASCII space except at either end of the text, and non-ASCII letters and digits
/// (`char::is_alphanumeric`). A newline shows as `⏎`, a carriage return `␍`, a tab `⇥`;
/// everything else as `⟨U+XXXX⟩`: an end space, every other control character, non-ASCII
/// whitespace and separators (a no-break or thin space looks like a space), format characters
/// (zero-width ones, bidirectional controls, the soft hyphen, the BOM, `U+1BCA0`..), combining
/// marks, symbols and punctuation outside ASCII, private use and unassigned code points. The
/// marks themselves are not letters, so in the text they are written as `⟨U+XXXX⟩` too: two
/// different commands never read the same. A letter that looks like another (Cyrillic `а`
/// for Latin `a`) still shows as itself: see docs/security-model.md.
///
/// Past `max_chars` (of the marked text) the line is cut between two marks, never inside
/// one, and says how much is left; the whole marked text is then in
/// [`RenderedCommand::full`], up to [`FULL_INPUT_MAX`]. `max_chars` is far below that cap.
pub fn render_command(s: &str, max_chars: usize) -> RenderedCommand {
    let mut full = String::new();
    let (mut kept, mut total) = (0usize, 0usize);
    let mut cut: Option<usize> = None;
    let mut too_long = false;
    let mut buf = [0u8; 4];
    // The spaces at either end: before `inner_start` or from `inner_end` on.
    let inner_start = s.find(|c| c != ' ').unwrap_or(s.len());
    let inner_end = s
        .char_indices()
        .rev()
        .find(|&(_, c)| c != ' ')
        .map_or(0, |(i, c)| i + c.len_utf8());
    for (i, c) in s.char_indices() {
        let escaped;
        let shown = match c {
            ' ' => i >= inner_start && i < inner_end,
            '!'..='~' => true,
            c => !c.is_ascii() && c.is_alphanumeric(),
        };
        let mark: &str = match c {
            '\n' => "⏎",
            '\r' => "␍",
            '\t' => "⇥",
            c if shown => c.encode_utf8(&mut buf),
            c => {
                escaped = format!("⟨U+{:04X}⟩", u32::from(c));
                &escaped
            }
        };
        let n = mark.chars().count();
        if cut.is_none() && kept + n > max_chars {
            cut = Some(full.len());
        }
        total += n;
        if !too_long {
            if full.len() + mark.len() > FULL_INPUT_MAX {
                too_long = true;
                cut.get_or_insert(full.len());
            } else {
                full.push_str(mark);
                kept += n;
            }
        }
    }
    let Some(at) = cut else {
        return RenderedCommand {
            line: full,
            full: None,
            too_long: false,
        };
    };
    let shown = full[..at].chars().count();
    let line = format!(
        "{}… {} more characters; open the card to see all",
        &full[..at],
        total - shown
    );
    if too_long {
        full.push_str(&format!("… {} more characters not shown", total - kept));
    }
    RenderedCommand {
        line,
        full: Some(full),
        too_long,
    }
}

/// The longest part of a Claude permission's summary that shows the input's fields beyond its
/// subject ([`render_request`]), in marked chars.
pub const INPUT_EXTRAS_MAX: usize = 300;

/// What a Claude Code permission card shows of a tool's input: its subject (the command of
/// `Bash`, the path of a file tool, a search's pattern and path, the URL of `WebFetch`, the
/// query of `WebSearch`, the description of `Task` / `Agent`, the plan of `ExitPlanMode`;
/// the JSON of any other tool's whole input), and the input's other fields as a JSON object:
/// a flag that changes what runs (`dangerouslyDisableSandbox`, `run_in_background`,
/// `allowed_domains`), a write's content, an edit's strings, a field twr does not know. A
/// subject field that is not a string is not shown by the subject, so it is among the
/// others. Only a `Bash` call's `description` (its purpose, shown apart) and `timeout` are
/// left out. None when the subject shows the whole input.
pub fn card_input(tool: &str, input: &Value) -> (String, Option<Value>) {
    let subject = input_text(tool, input);
    let shown: &[&str] = match tool {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" | "MultiEdit" => &["file_path"],
        "NotebookEdit" => &["notebook_path"],
        "Glob" | "Grep" => &["pattern", "path"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        "Task" | "Agent" => &["description"],
        "ExitPlanMode" => &["plan"],
        "TodoWrite" => &[],
        _ => return (subject, None),
    };
    let quiet: &[&str] = if tool == "Bash" {
        &["description", "timeout"]
    } else {
        &[]
    };
    let rest: Map<String, Value> = match input {
        Value::Object(m) => m
            .iter()
            .filter(|(k, v)| {
                !(shown.contains(&k.as_str()) && v.is_string()) && !quiet.contains(&k.as_str())
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        Value::Null => return (subject, None),
        other => return (subject, Some(other.clone())),
    };
    (subject, (!rest.is_empty()).then_some(Value::Object(rest)))
}

/// A request as a permission card shows it: `subject` marked ([`render_command`], its line
/// cut at `max_chars`), then, when the input has fields the subject does not show
/// ([`card_input`]), those fields as compact JSON, marked too (cut at [`INPUT_EXTRAS_MAX`]),
/// between `⟦` and `⟧`. The marks never show those two as themselves, so the parts cannot be
/// confused: `ls ⟦{"dangerouslyDisableSandbox":true}⟧`. When either part is cut, `full`
/// holds both whole, joined the same way (each capped at [`FULL_INPUT_MAX`]); `too_long`
/// when the two together are past [`FULL_INPUT_MAX`].
pub fn render_request(subject: &str, extras: Option<&Value>, max_chars: usize) -> RenderedCommand {
    let main = render_command(subject, max_chars);
    let Some(extras) = extras else {
        return main;
    };
    let more = render_command(&extras.to_string(), INPUT_EXTRAS_MAX);
    let join = |a: &str, b: &str| {
        if a.is_empty() {
            format!("⟦{b}⟧")
        } else {
            format!("{a} ⟦{b}⟧")
        }
    };
    let whole = |r: &RenderedCommand| r.full.clone().unwrap_or_else(|| r.line.clone());
    let (main_whole, more_whole) = (whole(&main), whole(&more));
    let too_long =
        main.too_long || more.too_long || main_whole.len() + more_whole.len() > FULL_INPUT_MAX;
    let cut = main.full.is_some() || more.full.is_some();
    RenderedCommand {
        line: join(&main.line, &more.line),
        full: cut.then(|| join(&main_whole, &more_whole)),
        too_long,
    }
}

/// [`render_command`]'s line for `s` cut at `max_chars`.
pub fn command_line(s: &str, max_chars: usize) -> String {
    render_command(s, max_chars).line
}

/// `s` marked as [`render_command`] marks it, whole (for short text that must be shown in
/// full, such as a directory a grant names; the caller checks its length).
pub fn mark_visible(s: &str) -> String {
    let r = render_command(s, usize::MAX);
    r.full.unwrap_or(r.line)
}

/// Short one-line description of a tool input: the command for Bash, the path for file
/// tools, the pattern for searches, compact JSON otherwise.
pub fn input_summary(tool: &str, input: &Value) -> String {
    one_line(&input_text(tool, input), INPUT_SUMMARY_MAX)
}

/// The longest input a permission's summary shows, in chars: the user judges the request
/// from it, so it is much longer than a tool call's [`INPUT_SUMMARY_MAX`].
pub const PERMISSION_INPUT_MAX: usize = 900;

/// What [`input_summary`] describes a tool input with, uncut.
fn input_text(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    let text = match tool {
        "Bash" => s("command").to_string(),
        "Read" | "Write" | "Edit" | "MultiEdit" => s("file_path").to_string(),
        "NotebookEdit" => s("notebook_path").to_string(),
        "Glob" | "Grep" => match s("path") {
            "" => s("pattern").to_string(),
            p => format!("{} in {p}", s("pattern")),
        },
        "WebFetch" => s("url").to_string(),
        "WebSearch" => s("query").to_string(),
        "Task" | "Agent" => s("description").to_string(),
        "ExitPlanMode" => s("plan").to_string(),
        "AskUserQuestion" => input
            .pointer("/questions/0/question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        "TodoWrite" => match input.get("todos").and_then(Value::as_array) {
            Some(t) => format!("{} todos", t.len()),
            None => String::new(),
        },
        _ => match input {
            Value::Null => String::new(),
            Value::Object(m) if m.is_empty() => String::new(),
            other => other.to_string(),
        },
    };
    text
}

/// Whether any string in the input matches a [`DESTRUCTIVE_PATTERNS`] entry.
pub fn is_destructive(input: &Value) -> bool {
    match input {
        Value::String(s) => {
            let lower = s.to_lowercase();
            DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p))
        }
        Value::Array(a) => a.iter().any(is_destructive),
        Value::Object(m) => m.values().any(is_destructive),
        _ => false,
    }
}

/// First ~[`OUTPUT_SUMMARY_MAX`] chars of a tool_result `content` (a string or content blocks).
pub fn output_summary(content: &Value) -> String {
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .map(|b| match str_at(b, "type") {
                "text" => str_at(b, "text").to_string(),
                "" => String::new(),
                other => format!("[{other}]"),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    };
    one_line(&text, OUTPUT_SUMMARY_MAX)
}

/// Option labels of question `i` of an `AskUserQuestion` input.
fn question_labels_at(input: &Value, i: usize) -> Vec<String> {
    input
        .pointer(&format!("/questions/{i}/options"))
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| o.get("label").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// The answer to question `i` (text `question`) of an `AskUserQuestion` request, or None to
/// decline the request. See [`Translator::respond`] for the rules.
fn question_answer(input: &Value, i: usize, question: &str, answer: &Answer) -> Option<String> {
    if let Some(s) = answer
        .extra
        .get("answers")
        .and_then(|m| m.get(question))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(s.to_string());
    }
    let labels = question_labels_at(input, i);
    let is_label = |s: &str| labels.iter().any(|l| l == s);
    let decision = answer.decision.trim();
    if matches!(decision, "deny" | "decline") && !is_label(decision) {
        return None;
    }
    let text = if decision.is_empty() || (decision == "allow" && !is_label(decision)) {
        answer.message.as_deref().map(str::trim).unwrap_or_default()
    } else {
        decision
    };
    if text.is_empty() {
        return None;
    }
    if !is_label(text) {
        // AskUserQuestion's "Other" choice: free text is a valid answer.
        tracing::debug!(%question, "claude-code: relaying a free-text answer");
    }
    Some(text.to_string())
}

/// The `error` Claude Code puts on the `assistant` message (and `api_retry` notices) of a
/// request refused for authentication.
const AUTH_FAILED: &str = "authentication_failed";

/// Appended to the error of a turn that failed because Claude Code is not signed in (or its
/// login expired, or its API key is invalid): `turn.ended` `ext.twr.error`.
pub const AUTH_HINT: &str = " (sign in again: run `claude`, then /login)";

/// Whether a failed `result` says Claude Code could not authenticate. Claude Code 2.1.287
/// (checked with an invalid API key, expired OAuth credentials and none at all) ends such a
/// turn with `is_error`, `terminal_reason: api_error` and `result` "Failed to authenticate.
/// API Error: 401 API key is invalid.", "Failed to authenticate: OAuth session expired and
/// could not be refreshed" or "Not logged in · Please run /login" (`api_error_status` 401 with
/// an API key), after an `assistant` message with `error: authentication_failed`.
pub fn auth_failure(v: &Value) -> bool {
    if v.get("api_error_status").and_then(Value::as_u64) == Some(401) {
        return true;
    }
    const PATTERNS: &[&str] = &[
        "failed to authenticate",
        "not logged in",
        "run /login",
        "invalid api key",
        "api key is invalid",
        "oauth token has expired",
        "oauth session expired",
        "authentication_error",
        "authentication_failed",
        "api error: 401",
    ];
    let mut texts: Vec<String> = Vec::new();
    if let Some(s) = v.get("result").and_then(Value::as_str) {
        texts.push(s.to_lowercase());
    }
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        texts.extend(errs.iter().filter_map(Value::as_str).map(str::to_lowercase));
    }
    texts.iter().any(|t| PATTERNS.iter().any(|p| t.contains(p)))
}

/// Longest `ext.twr.error` (chars) a failed turn gets.
pub const ERROR_MAX: usize = 500;

/// `s` as an `ext.twr.error`: one line, no terminal escapes or other control characters, at
/// most [`ERROR_MAX`] chars.
pub fn error_line(s: &str) -> String {
    let mut clean = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // A CSI sequence (`ESC [ ... final`) goes whole; any other escape loses its ESC.
            if chars.as_str().starts_with('[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        clean.push(if c.is_control() { ' ' } else { c });
    }
    one_line(&clean, ERROR_MAX)
}

/// The cause a failed `result` reports, in this order: its `result` text (Claude Code
/// 2.1.287 puts an API error there, e.g. "API Error: Repeated 529 Overloaded errors"), its
/// `errors` ("Reached maximum number of turns (1)"), `api_error_status` with the text of the
/// turn's API error message (`api_error`), then `terminal_reason` / an `error_*` `subtype`.
fn error_text(v: &Value, api_error: Option<&str>) -> Option<String> {
    if let Some(s) = v
        .get("result")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
    {
        return Some(s.to_string());
    }
    let errs: Vec<&str> = v
        .get("errors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if !errs.is_empty() {
        return Some(errs.join("; "));
    }
    let api_error = api_error.filter(|s| !s.trim().is_empty());
    match (v.get("api_error_status").and_then(Value::as_u64), api_error) {
        (Some(code), Some(text)) if text.contains(&code.to_string()) => {
            return Some(text.to_string())
        }
        (Some(code), Some(text)) => return Some(format!("API error {code}: {text}")),
        (Some(code), None) => return Some(format!("API error {code}")),
        (None, Some(text)) => return Some(text.to_string()),
        (None, None) => {}
    }
    let reason = str_at(v, "terminal_reason");
    if !reason.is_empty() {
        return Some(format!("Claude Code ended the turn: {reason}"));
    }
    let subtype = str_at(v, "subtype");
    subtype
        .starts_with("error")
        .then(|| format!("Claude Code ended the turn: {subtype}"))
}

/// Why a failed `result` looks transient (rate limit, overload, 5xx, network), if it does.
///
/// Structural first: `api_error_status` 408/429/5xx (529 = overloaded). Then the texts the
/// CLI puts in `result` / `errors` (e.g. `API Error: 529 {"type":"overloaded_error"...}`).
/// None of the recorded fixtures contain a real API failure, so the text patterns follow the
/// CLI's documented error strings and are deliberately narrow.
pub fn transient_reason(v: &Value) -> Option<String> {
    if let Some(code) = v.get("api_error_status").and_then(Value::as_u64) {
        if code == 408 || code == 429 || (500..600).contains(&code) {
            return Some(format!("api_error_status {code}"));
        }
    }
    let mut texts: Vec<String> = Vec::new();
    if let Some(s) = v.get("result").and_then(Value::as_str) {
        texts.push(s.to_lowercase());
    }
    if let Some(errs) = v.get("errors").and_then(Value::as_array) {
        texts.extend(errs.iter().filter_map(Value::as_str).map(str::to_lowercase));
    }
    const PATTERNS: &[&str] = &[
        "api error: 408",
        "api error: 429",
        "api error: 5",
        "overloaded",
        "rate limit",
        "rate_limit",
        "econnreset",
        "econnrefused",
        "etimedout",
        "enotfound",
        "socket hang up",
        "fetch failed",
        "network error",
        "connection error",
    ];
    texts.iter().find_map(|t| {
        PATTERNS
            .iter()
            .find(|p| t.contains(*p))
            .map(|p| format!("error text matches {p:?}"))
    })
}

/// The windows of a `rate_limit_event`'s `rate_limit_info`: its `unifiedWindows` (fractions
/// used), else the top-level `utilization` of a `five_hour` or `seven_day` `rateLimitType`.
pub fn rate_limit_windows(info: &Value) -> Vec<UsageWindow> {
    let window = |id: &str, w: &Value, used: &str, resets: &str| {
        crate::usage::pct(&w[used]).map(|f| UsageWindow {
            window: id.into(),
            used_pct: f * 100.0,
            resets_at: crate::usage::epoch(&w[resets]),
            minutes: None,
            observed_at: None,
        })
    };
    let unified = &info["unifiedWindows"];
    let mut out: Vec<UsageWindow> = [(FIVE_HOUR, "five_hour"), (SEVEN_DAY, "seven_day")]
        .into_iter()
        .filter_map(|(id, key)| window(id, &unified[key], "utilization", "resetsAt"))
        .collect();
    if out.is_empty() {
        let id = match str_at(info, "rateLimitType") {
            "five_hour" => Some(FIVE_HOUR),
            "seven_day" => Some(SEVEN_DAY),
            _ => None,
        };
        out.extend(id.and_then(|id| window(id, info, "utilization", "resetsAt")));
    }
    out
}

/// The windows of a `get_usage` answer's `rate_limits` (percentages used, ISO `resets_at`).
pub fn usage_read_windows(limits: &Value) -> Vec<UsageWindow> {
    [(FIVE_HOUR, "five_hour"), (SEVEN_DAY, "seven_day")]
        .into_iter()
        .filter_map(|(id, key)| {
            let w = &limits[key];
            crate::usage::pct(&w["utilization"]).map(|used| UsageWindow {
                window: id.into(),
                used_pct: used,
                resets_at: crate::usage::rfc3339(&w["resets_at"]),
                minutes: None,
                observed_at: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries() {
        assert_eq!(
            input_summary("Bash", &json!({"command": "ls -la\n  && echo hi"})),
            "ls -la && echo hi"
        );
        assert_eq!(
            input_summary("Write", &json!({"file_path": "/a/b.txt", "content": "x"})),
            "/a/b.txt"
        );
        assert_eq!(
            input_summary("Grep", &json!({"pattern": "fn main", "path": "src"})),
            "fn main in src"
        );
        assert_eq!(input_summary("mcp__x__y", &json!({"a": 1})), "{\"a\":1}");
        let long = "x".repeat(500);
        assert_eq!(
            input_summary("Bash", &json!({ "command": long }))
                .chars()
                .count(),
            INPUT_SUMMARY_MAX + 1
        );
        // A permission shows all of it.
        let what = command_line(
            &input_text("Bash", &json!({ "command": long })),
            PERMISSION_INPUT_MAX,
        );
        assert_eq!(what, long);
        assert_eq!(output_summary(&json!("a\n\nb")), "a b");
        assert_eq!(
            output_summary(&json!([{"type": "text", "text": "hi"}, {"type": "image"}])),
            "hi [image]"
        );
    }

    /// An approval shows the command as it runs: a newline after a comment is not hidden in a
    /// space (the audit's case: `# note` then `rm -rf ~` read as one comment), quoted
    /// whitespace stays, and invisible characters show. Two different commands never read
    /// the same.
    #[test]
    fn command_lines_show_what_runs() {
        assert_eq!(
            command_line("echo hi # tidy up\nrm -rf ~", 900),
            "echo hi # tidy up⏎rm -rf ~"
        );
        assert_eq!(
            command_line("echo 'a   b'\t|\r cat", 900),
            "echo 'a   b'⇥|␍ cat"
        );
        // Nothing is trimmed: an end that matters shows.
        assert_eq!(command_line("\n  ls -la  \n", 900), "⏎  ls -la  ⏎");
        assert_eq!(command_line("echo value\\ ", 900), "echo value\\⟨U+0020⟩");
        assert_ne!(
            command_line("echo value\\ ", 900),
            command_line("echo value\\", 900)
        );
        assert_eq!(command_line("  ls", 900), "⟨U+0020⟩⟨U+0020⟩ls");
        assert_eq!(command_line(" ", 900), "⟨U+0020⟩");
        // Whitespace that looks like a space but is not one.
        assert_eq!(
            command_line(
                "rm\u{a0}-rf a\u{2007}b\u{202f}c\u{3000}d\u{1680}e\u{2028}f",
                900
            ),
            "rm⟨U+00A0⟩-rf a⟨U+2007⟩b⟨U+202F⟩c⟨U+3000⟩d⟨U+1680⟩e⟨U+2028⟩f"
        );
        assert_eq!(
            command_line("ls \u{202e}cod.exe\u{200b} \u{7}", 900),
            "ls ⟨U+202E⟩cod.exe⟨U+200B⟩ ⟨U+0007⟩"
        );
        // The marks in the text are not the characters they stand for.
        assert_eq!(
            command_line("echo ⏎ ⟨x⟩", 900),
            "echo ⟨U+23CE⟩ ⟨U+27E8⟩x⟨U+27E9⟩"
        );
        // No table of invisible characters: anything non-ASCII that is not a letter or a
        // digit is marked, the zero-width format characters a hand list missed included.
        assert_eq!(
            command_line(
                "rm\u{1bca0}\u{1bca3} -rf\u{13430} ~\u{e000}\u{301}x\u{10ffff}",
                900
            ),
            "rm⟨U+1BCA0⟩⟨U+1BCA3⟩ -rf⟨U+13430⟩ ~⟨U+E000⟩⟨U+0301⟩x⟨U+10FFFF⟩"
        );
        // Letters and digits of any script show as themselves (a look-alike letter, such as
        // Cyrillic а, is a documented limit); symbols outside ASCII do not.
        assert_eq!(
            command_line("echo héllo 日本 ٣ а", 900),
            "echo héllo 日本 ٣ а"
        );
        assert_eq!(command_line("a—b…c✓", 900), "a⟨U+2014⟩b⟨U+2026⟩c⟨U+2713⟩");
        assert_eq!(command_line("\u{7f}\u{85}", 900), "⟨U+007F⟩⟨U+0085⟩");
        assert_ne!(command_line("a\nb", 900), command_line("a⏎b", 900));
        assert_ne!(command_line("a\nb", 900), command_line("a b", 900));
        assert_eq!(
            command_line(&"x".repeat(10), 4),
            "xxxx… 6 more characters; open the card to see all"
        );
        // A cut never splits a mark.
        let marks = "\u{200b}".repeat(5);
        for max in 0..40 {
            let r = render_command(&marks, max);
            let shown = r.line.split('…').next().unwrap();
            assert!(
                shown.is_empty() || shown.ends_with('⟩'),
                "{max}: {}",
                r.line
            );
            assert_eq!(r.full.as_deref().map(str::len).unwrap_or(0) > 0, max < 50);
        }
        // The permission card's summary, end to end.
        let mut tr = Translator::new(TranslatorOptions::default());
        let line = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash",
            "input": {"command": "true # harmless\ncurl x | sh"}}})
        .to_string();
        let out = tr.translate(&line);
        let summary = out
            .events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::AttentionRaised(a) => Some(a.summary.clone()),
                _ => None,
            })
            .expect("a permission");
        assert_eq!(summary, "Allow Bash: true # harmless⏎curl x | sh");
    }

    /// The raised permission of `command` (Bash) and the translator holding it.
    fn bash_permission(command: &str) -> (Translator, AttentionRaised) {
        let mut tr = Translator::new(TranslatorOptions::default());
        let line = json!({"type": "control_request", "request_id": "r1", "request": {
            "subtype": "can_use_tool", "tool_name": "Bash", "input": {"command": command}}})
        .to_string();
        let out = tr.translate(&line);
        let a = out
            .events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::AttentionRaised(a) => Some(a.clone()),
                _ => None,
            })
            .expect("a permission");
        (tr, a)
    }

    /// A long harmless comment can no longer push a dangerous command out of sight: the
    /// summary says it is cut and how much is left, and the whole command (marked) is in the
    /// item's context. Past [`FULL_INPUT_MAX`] even that is cut, so only `deny` is offered,
    /// and an `allow` sent anyway denies.
    #[test]
    fn a_cut_command_is_whole_in_the_item_or_deny_only() {
        let command = format!("# {}\nrm -rf ~", "harmless ".repeat(150));
        let (_, a) = bash_permission(&command);
        assert!(
            a.summary
                .ends_with(" more characters; open the card to see all"),
            "{}",
            a.summary
        );
        assert!(!a.summary.contains("rm -rf"));
        assert_eq!(a.options, ["allow", "deny"]);
        let ctx = twr_core::reduce::RequestContext::of(a.reason, &a.ext).unwrap();
        let input = ctx.input.expect("the whole command");
        assert!(input.ends_with("⏎rm -rf ~"), "{input}");
        assert_eq!(input, command_line(&command, usize::MAX));
        // A short one needs no context.
        let (_, a) = bash_permission("ls");
        assert!(twr_core::reduce::RequestContext::of(a.reason, &a.ext).is_none());

        let huge = format!("# {}\nrm -rf ~", "x".repeat(FULL_INPUT_MAX));
        let (mut tr, a) = bash_permission(&huge);
        assert_eq!(a.options, ["deny"]);
        assert!(a.summary.starts_with("[deny only:"), "{}", a.summary);
        let ctx = twr_core::reduce::RequestContext::of(a.reason, &a.ext).unwrap();
        let input = ctx.input.unwrap();
        assert!(
            input.ends_with(" more characters not shown"),
            "{}",
            &input[input.len() - 60..]
        );
        assert!(input.len() <= FULL_INPUT_MAX + 64);
        let out = tr.respond(&a.attn_id, &Answer::allow()).unwrap();
        let reply = out.reply.expect("the answer");
        assert!(reply.contains("\"deny\""), "{reply}");
    }

    #[test]
    fn risk() {
        assert!(is_destructive(&json!({"command": "sudo rm -rf /"})));
        assert!(is_destructive(
            &json!({"args": ["git push --force origin"]})
        ));
        assert!(!is_destructive(&json!({"command": "ls"})));
    }

    #[test]
    fn transient() {
        assert!(transient_reason(&json!({"api_error_status": 529})).is_some());
        assert!(transient_reason(&json!({"api_error_status": 400})).is_none());
        assert!(transient_reason(&json!({"result": "API Error: 503 upstream"})).is_some());
        assert!(transient_reason(&json!({"errors": ["Overloaded"]})).is_some());
        assert!(transient_reason(&json!({"result": "Invalid API key"})).is_none());
    }

    #[test]
    fn unsupported_control_request_gets_an_error_reply() {
        let mut t = Translator::new(TranslatorOptions::default());
        let out = t.translate(
            r#"{"type":"control_request","request_id":"r1","request":{"subtype":"hook_callback"}}"#,
        );
        // Never silent: recorded as an item raised and closed at once.
        match &out.events[..] {
            [Emitted {
                body: EventBody::AttentionRaised(a),
                ..
            }, Emitted {
                body: EventBody::AttentionResolved(r),
                ..
            }] => {
                assert_eq!(a.attn_id, "unsupported-r1");
                assert_eq!(a.reason, AttentionReason::Error);
                assert!(a.summary.contains("hook_callback"), "{}", a.summary);
                assert_eq!(r.attn_id, a.attn_id);
                assert_eq!(r.by, ResolvedBy::Timeout);
                assert_eq!(
                    r.ext.as_ref().unwrap()["claude"]["cancelled_by"],
                    "unsupported_request"
                );
            }
            other => panic!("{other:?}"),
        }
        let reply: Value = serde_json::from_str(&out.reply.unwrap()).unwrap();
        assert_eq!(reply["response"]["subtype"], "error");
        assert_eq!(reply["response"]["request_id"], "r1");
        assert!(t.pending_attention().is_empty());
        assert!(t.translate("not json").events.is_empty());
    }

    /// A tool call carries what the agent said it is for (a Bash `description`) and, when it
    /// changes the step the main agent's plan has in progress, that step: a `TodoWrite`'s
    /// item in progress, or the task a `TaskUpdate` starts (its id from its `TaskCreate`'s
    /// result), cleared once that task is completed. A subagent's plan is not the thread's.
    #[test]
    fn tool_calls_carry_their_purpose_and_the_plans_step() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("go", 1);
        let call = |t: &mut Translator, id: &str, name: &str, input: Value, parent: &str| {
            let parent = if parent.is_empty() {
                Value::Null
            } else {
                json!(parent)
            };
            let line = json!({
                "type": "assistant",
                "parent_tool_use_id": parent,
                "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]},
            });
            let out = t.translate(&line.to_string());
            let ext = out.events.iter().find_map(|e| match &e.body {
                EventBody::AgentToolCall(c) => Some(c.ext.clone()),
                _ => None,
            });
            let ext = ext.expect("a tool call");
            (
                twr_core::event::twr_str(ext.as_ref(), STEP_KEY).map(str::to_string),
                twr_core::event::twr_str(ext.as_ref(), PURPOSE_KEY).map(str::to_string),
            )
        };
        let result = |t: &mut Translator, id: &str, text: &str| {
            let line = json!({
                "type": "user",
                "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]},
            });
            t.translate(&line.to_string());
        };
        let bash = json!({"command": "cargo test", "description": "Run the TUI tests"});
        assert_eq!(
            call(&mut t, "b1", "Bash", bash, ""),
            (None, Some("Run the TUI tests".into()))
        );
        let todos = json!({"todos": [
            {"content": "Plan", "activeForm": "Planning", "status": "completed"},
            {"content": "Add the arrow keys", "activeForm": "Adding the arrow keys", "status": "in_progress"},
        ]});
        assert_eq!(
            call(&mut t, "w1", "TodoWrite", todos, "").0.as_deref(),
            Some("Adding the arrow keys")
        );
        let created = json!({"subject": "Anchor the view", "activeForm": "Anchoring the view"});
        assert_eq!(call(&mut t, "c1", "TaskCreate", created, "").0, None);
        result(
            &mut t,
            "c1",
            "Task #3 created successfully: Anchor the view",
        );
        // A subagent's own task is not the thread's step.
        let theirs = json!({"subject": "Theirs", "activeForm": "Doing theirs"});
        call(&mut t, "c2", "TaskCreate", theirs, "agent-1");
        result(&mut t, "c2", "Task #4 created successfully: Theirs");
        let start = |id: &str| json!({"taskId": id, "status": "in_progress"});
        assert_eq!(
            call(&mut t, "u0", "TaskUpdate", start("4"), "agent-1").0,
            None
        );
        assert_eq!(
            call(&mut t, "u1", "TaskUpdate", start("3"), "")
                .0
                .as_deref(),
            Some("Anchoring the view")
        );
        // Reworded while in progress: its new words; back to pending: none is in progress.
        let reword = json!({"taskId": "3", "activeForm": "Verifying the fix"});
        assert_eq!(
            call(&mut t, "u4", "TaskUpdate", reword, "").0.as_deref(),
            Some("Verifying the fix")
        );
        let other = json!({"taskId": "5", "activeForm": "Elsewhere"});
        assert_eq!(call(&mut t, "u5", "TaskUpdate", other, "").0, None);
        let pending = json!({"taskId": "3", "status": "pending"});
        assert_eq!(
            call(&mut t, "u6", "TaskUpdate", pending, "").0.as_deref(),
            Some("")
        );
        assert_eq!(
            call(&mut t, "u7", "TaskUpdate", start("3"), "")
                .0
                .as_deref(),
            Some("Verifying the fix")
        );
        let done = |id: &str| json!({"taskId": id, "status": "completed"});
        assert_eq!(call(&mut t, "u2", "TaskUpdate", done("9"), "").0, None);
        assert_eq!(
            call(&mut t, "u3", "TaskUpdate", done("3"), "").0.as_deref(),
            Some("")
        );
    }

    /// Relay rule: an MCP elicitation is the user's to answer. It becomes a question offering
    /// only decline / cancel, and nothing is written until the user picks one.
    #[test]
    fn elicitations_wait_for_the_user() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("go", 1);
        let out = t.translate(
            r#"{"type":"control_request","request_id":"e1","request":{"subtype":"elicitation","mcp_server_name":"gh","message":"Sign in","mode":"url","url":"https://example.com/auth","elicitation_id":"x"}}"#,
        );
        assert!(out.reply.is_none(), "nothing is answered for the user");
        let a = match &out.events[..] {
            [Emitted {
                body: EventBody::AttentionRaised(a),
                turn: Some(1),
                ..
            }] => a.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(a.attn_id, "e1");
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.options, ["decline", "cancel"]);
        assert!(a.summary.contains("Sign in"), "{}", a.summary);
        assert!(
            a.summary.contains("https://example.com/auth"),
            "{}",
            a.summary
        );
        assert_eq!(t.pending_attention(), ["e1"]);
        // Even an `allow` cannot accept a form twr never showed: it declines.
        let out = t.respond("e1", &Answer::allow()).unwrap();
        let r: Value = serde_json::from_str(&out.reply.unwrap()).unwrap();
        assert_eq!(r["response"]["request_id"], "e1");
        assert_eq!(r["response"]["response"]["action"], "decline");
        assert!(t.respond("e1", &Answer::allow()).is_none());
        // Cancel is passed on as such; a runtime cancel withdraws it.
        t.translate(r#"{"type":"control_request","request_id":"e2","request":{"subtype":"elicitation","message":"Name?","mode":"form"}}"#);
        let out = t.respond("e2", &Answer::new("cancel")).unwrap();
        let r: Value = serde_json::from_str(&out.reply.unwrap()).unwrap();
        assert_eq!(r["response"]["response"]["action"], "cancel");
        t.translate(r#"{"type":"control_request","request_id":"e3","request":{"subtype":"elicitation","message":"Name?"}}"#);
        let out = t.translate(r#"{"type":"control_cancel_request","request_id":"e3"}"#);
        assert!(matches!(
            &out.events[..],
            [Emitted {
                body: EventBody::AttentionResolved(_),
                ..
            }]
        ));
        assert!(t.pending_attention().is_empty());
    }

    /// A `result` line too long to read still ends the turn, and tool ids do not outlive
    /// their turn.
    #[test]
    fn an_oversized_result_still_ends_the_turn_and_tools_are_per_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("go", 1);
        t.translate(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"tu1","name":"Bash","input":{"command":"ls"}}]}}"#);
        assert_eq!(t.tools.len(), 1);
        assert!(t
            .oversized(r#"{"type":"assistant","message""#, 99)
            .events
            .is_empty());
        let out = t.oversized(
            r#"{"type":"result","subtype":"success","result":"xxxx"#,
            40_000_000,
        );
        match &out.events[..] {
            [Emitted {
                body: EventBody::TurnEnded(e),
                turn: Some(1),
                raw: None,
            }] => {
                assert_eq!(e.result, TurnResult::Error);
                let twr = &e.ext.as_ref().unwrap()["twr"];
                assert_eq!(twr["error"], OVERSIZED_ERROR);
                assert_eq!(twr["oversized_len"], 40_000_000);
            }
            other => panic!("{other:?}"),
        }
        assert!(!t.busy());
        assert!(t.tools.is_empty());
    }

    #[test]
    fn cost_is_a_per_turn_delta_and_zero_errors_are_unknown() {
        let mut t = Translator::new(TranslatorOptions::default());
        let cost = |t: &mut Translator, line: &str| match &t.translate(line).events[0].body {
            EventBody::TurnEnded(e) => e.usage.as_ref().unwrap().cost,
            other => panic!("{other:?}"),
        };
        t.user_message("a", 1);
        assert_eq!(
            cost(
                &mut t,
                r#"{"type":"result","subtype":"success","total_cost_usd":0.5}"#
            ),
            Some(0.5)
        );
        t.user_message("b", 2);
        assert_eq!(
            cost(
                &mut t,
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"total_cost_usd":0}"#
            ),
            None
        );
        t.user_message("c", 3);
        assert_eq!(
            cost(
                &mut t,
                r#"{"type":"result","subtype":"success","total_cost_usd":0.75}"#
            ),
            Some(0.25)
        );
        // Interrupted (as seen live on a resumed process): the total does not move.
        t.user_message("d", 4);
        assert!(t.interrupt().is_some());
        assert_eq!(
            cost(
                &mut t,
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","total_cost_usd":0.75}"#
            ),
            None
        );
        t.user_message("e", 5);
        assert_eq!(
            cost(
                &mut t,
                r#"{"type":"result","subtype":"success","total_cost_usd":1.0}"#
            ),
            Some(0.25)
        );
    }

    /// As recorded live (Claude Code 2.1.291): a resumed session first consumes the notice of a
    /// task its previous process left, with a `result` of its own, then answers the prompt in a
    /// second run. The prompt's turn ends with the answer, not with that first `result`.
    #[test]
    fn a_task_notification_result_with_no_model_turn_ends_no_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("is it merged?", 12);
        let mut events = Vec::new();
        for line in [
            r#"{"type":"system","subtype":"task_notification","task_id":"b1","status":"stopped","summary":"Background shell command didn't finish before the previous session ended","session_id":"s"}"#,
            r#"{"type":"system","subtype":"init","session_id":"s","permissionMode":"default"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":0,"duration_ms":173,"total_cost_usd":155.5,"usage":{"input_tokens":0,"output_tokens":0},"origin":{"kind":"task-notification"},"session_id":"s"}"#,
            r#"{"type":"system","subtype":"init","session_id":"s","permissionMode":"default"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Yes, all of it is in dev."}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"num_turns":2,"stop_reason":"end_turn","total_cost_usd":155.75,"usage":{"input_tokens":10,"output_tokens":5},"session_id":"s"}"#,
        ] {
            events.extend(t.translate(line).events);
        }
        let starts = events
            .iter()
            .filter(|e| matches!(e.body, EventBody::TurnStarted(_)))
            .count();
        assert_eq!(starts, 0, "no runtime turn opens: {events:?}");
        let ends: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::TurnEnded(p) => Some((e.turn, p.usage.as_ref().and_then(|u| u.cost))),
                _ => None,
            })
            .collect();
        assert_eq!(ends, vec![(Some(12), Some(0.25))]);
        let text = events
            .iter()
            .find(|e| matches!(e.body, EventBody::AgentText(_)))
            .unwrap();
        assert_eq!(text.turn, Some(12));
        assert!(!t.busy());

        // With no prompt waiting, such a result closes nothing either.
        let out = t.translate(
            r#"{"type":"result","subtype":"success","num_turns":0,"total_cost_usd":155.75,"origin":{"kind":"task-notification"}}"#,
        );
        assert!(out.events.is_empty(), "{:?}", out.events);
    }

    /// The turns `events` start and end, in order: `start <turn> <runtime turn>` and
    /// `end <turn> <runtime turn> <cost>`.
    fn turn_marks(events: &[Emitted]) -> Vec<String> {
        let runtime = |ext: &Option<JsonMap>| {
            ext.as_ref()
                .and_then(|e| e.get("twr"))
                .and_then(|t| t.get(RUNTIME_TURN_KEY))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::TurnStarted(p) => {
                    Some(format!("start {:?} {:?}", e.turn, runtime(&p.ext)))
                }
                EventBody::TurnEnded(p) => Some(format!(
                    "end {:?} {:?} {:?}",
                    e.turn,
                    runtime(&p.ext),
                    p.usage.as_ref().and_then(|u| u.cost)
                )),
                _ => None,
            })
            .collect()
    }

    fn feed(t: &mut Translator, lines: &[&str]) -> Vec<Emitted> {
        lines.iter().flat_map(|l| t.translate(l).events).collect()
    }

    const INIT: &str =
        r#"{"type":"system","subtype":"init","session_id":"s","permissionMode":"default"}"#;

    fn result(num_turns: u64, total: Option<f64>, notification: bool) -> String {
        let mut v = json!({"type": "result", "subtype": "success", "is_error": false,
            "num_turns": num_turns, "stop_reason": "end_turn", "session_id": "s",
            "usage": {"input_tokens": 1, "output_tokens": 1}});
        if let Some(t) = total {
            v["total_cost_usd"] = json!(t);
        }
        if notification {
            v["origin"] = json!({"kind": "task-notification"});
        }
        v.to_string()
    }

    fn reply(text: &str) -> String {
        json!({"type": "assistant", "message": {"content": [{"type": "text", "text": text}]}})
            .to_string()
    }

    /// An initialised process with no prompt waiting hears of a task and consumes the notice
    /// without the model: its `init` and `result` open and end no turn and charge nothing. The
    /// runtime's own run after it is a turn, charged only what it spent.
    #[test]
    fn a_notification_result_with_no_prompt_opens_no_runtime_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        let first = feed(
            &mut t,
            &[INIT, &reply("Hello."), &result(1, Some(1.0), false)],
        );
        assert_eq!(turn_marks(&first), ["end Some(1) None Some(1.0)"]);

        let events = feed(
            &mut t,
            &[
                r#"{"type":"system","subtype":"task_notification","task_id":"b1","status":"completed","summary":"done","session_id":"s"}"#,
                INIT,
                &result(0, Some(1.0), true),
            ],
        );
        assert!(turn_marks(&events).is_empty(), "{events:?}");
        assert!(!t.busy());

        let events = feed(
            &mut t,
            &[
                INIT,
                &reply("The tests passed."),
                &result(1, Some(1.5), false),
            ],
        );
        assert_eq!(
            turn_marks(&events),
            ["start None Some(\"1\")", "end None Some(\"1\") Some(0.5)"]
        );
    }

    /// A prompt written while the runtime works on its own (after the run's `init`, before
    /// any output) waits for that run's `result`; a notification `result` meanwhile ends
    /// neither.
    #[test]
    fn a_prompt_queued_during_runtime_activity_waits_for_its_result() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        feed(
            &mut t,
            &[INIT, &reply("Hello."), &result(1, Some(1.0), false)],
        );

        let mut events = feed(&mut t, &[INIT]);
        t.user_message("and now?", 2);
        events.extend(feed(
            &mut t,
            &[&reply("Checking the build."), &result(0, Some(1.0), true)],
        ));
        assert_eq!(turn_marks(&events), ["start None Some(\"1\")"]);
        assert!(t.busy());
        let events = feed(
            &mut t,
            &[
                &result(1, Some(1.25), false),
                INIT,
                &reply("Done."),
                &result(1, Some(1.5), false),
            ],
        );
        assert_eq!(
            turn_marks(&events),
            [
                "end None Some(\"1\") Some(0.25)",
                "end Some(2) None Some(0.25)"
            ]
        );
        let text = events
            .iter()
            .filter(|e| matches!(e.body, EventBody::AgentText(_)))
            .map(|e| e.turn)
            .collect::<Vec<_>>();
        assert_eq!(text, [Some(2)]);
        assert!(!t.busy());
    }

    fn task_line(subtype: &str, id: &str, extra: Value) -> String {
        let mut v = json!({"type": "system", "subtype": subtype, "task_id": id, "session_id": "s"});
        if let (Some(o), Some(x)) = (v.as_object_mut(), extra.as_object()) {
            o.extend(x.clone());
        }
        v.to_string()
    }

    fn stream(event: Value) -> String {
        json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}).to_string()
    }

    fn twr_key(e: &Emitted, key: &str) -> Option<Value> {
        match &e.body {
            EventBody::TurnEnded(p) => p.ext.as_ref()?.get("twr")?.get(key).cloned(),
            _ => None,
        }
    }

    const MESSAGE_START: &str =
        r#"{"type":"stream_event","event":{"type":"message_start"},"parent_tool_use_id":null}"#;

    /// A first turn that leaves a background task of the main agent's (`b1`) running.
    fn with_background_task(t: &mut Translator) {
        t.user_message("run the tests in the background", 1);
        feed(
            t,
            &[
                INIT,
                &task_line(
                    "task_started",
                    "b1",
                    json!({"is_backgrounded": true, "task_type": "local_bash", "description": "cargo test"}),
                ),
                &reply("Tests are running."),
                &result(1, Some(1.0), false),
            ],
        );
        assert!(!t.busy());
    }

    /// The end of a task the main agent put in the background starts a run of Claude's own,
    /// with no `init` and its first output much later: the runtime's turn opens at that end, so
    /// the hub writes no prompt into the run. The run's output and `result` are that turn's.
    #[test]
    fn a_background_task_end_opens_the_runtime_turn_at_once() {
        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        let events = feed(
            &mut t,
            &[&task_line(
                "task_updated",
                "b1",
                json!({"patch": {"status": "completed"}}),
            )],
        );
        assert!(matches!(
            events[0].body,
            EventBody::TaskUpdated(twr_core::event::TaskUpdated { running: false, .. })
        ));
        assert_eq!(turn_marks(&events), ["start None Some(\"1\")"]);
        assert!(t.busy() && t.deadline().is_some());
        let events = feed(
            &mut t,
            &[
                MESSAGE_START,
                &reply("All tests passed."),
                &result(2, Some(1.5), true),
            ],
        );
        assert!(t.deadline().is_none(), "the run started");
        assert_eq!(turn_marks(&events), ["end None Some(\"1\") Some(0.5)"]);
        assert!(!t.busy());
    }

    /// Only the main agent's background tasks notify it: a foreground task's end, or a
    /// subagent's task's (even one it put in the background), opens nothing; a task moved to
    /// the background later (a `task_updated` patch) does.
    #[test]
    fn only_the_main_agents_background_tasks_open_a_turn_when_they_end() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        feed(
            &mut t,
            &[
                INIT,
                &task_line("task_started", "fg", json!({"is_backgrounded": false})),
                &task_line(
                    "task_started",
                    "sub",
                    json!({"is_backgrounded": true, "owned_by_subagent": true, "parent_task_id": "a1"}),
                ),
                &task_line("task_started", "later", json!({"is_backgrounded": false})),
                &task_line(
                    "task_updated",
                    "later",
                    json!({"patch": {"is_backgrounded": true}}),
                ),
                &reply("Working."),
                &result(1, Some(1.0), false),
            ],
        );
        for id in ["fg", "sub"] {
            let events = feed(
                &mut t,
                &[&task_line(
                    "task_notification",
                    id,
                    json!({"status": "completed"}),
                )],
            );
            assert!(turn_marks(&events).is_empty(), "{id}: {events:?}");
            assert!(!t.busy());
        }
        let events = feed(
            &mut t,
            &[&task_line(
                "task_notification",
                "later",
                json!({"status": "killed"}),
            )],
        );
        assert_eq!(turn_marks(&events), ["start None Some(\"1\")"]);
    }

    /// A task's end with a prompt queued opens nothing: the run is that prompt's.
    #[test]
    fn a_background_task_end_with_a_prompt_queued_opens_no_runtime_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        t.user_message("and now?", 2);
        let events = feed(
            &mut t,
            &[
                &task_line("task_notification", "b1", json!({"status": "completed"})),
                INIT,
                &reply("Done."),
                &result(1, Some(1.5), false),
            ],
        );
        assert_eq!(turn_marks(&events), ["end Some(2) None Some(0.5)"]);
        assert!(t.deadline().is_none());
    }

    /// No run follows the notification: past its wait the turn ends `completed`, marked
    /// `no_run`, with no usage; a `result` that asked the model nothing ends it at once.
    #[test]
    fn a_notification_turn_with_no_run_ends() {
        let mut t = Translator::new(TranslatorOptions {
            notice_run_after: Some(std::time::Duration::from_secs(60)),
            ..Default::default()
        });
        with_background_task(&mut t);
        let end = task_line("task_notification", "b1", json!({"status": "completed"}));
        feed(&mut t, &[&end]);
        let at = t.deadline().unwrap();
        assert!(t
            .expire(at - chrono::Duration::seconds(1))
            .events
            .is_empty());
        // A task's line or a rate limit is no run.
        feed(&mut t, &[&task_line("task_progress", "x", json!({}))]);
        assert_eq!(t.deadline(), Some(at));
        let events = t.expire(at).events;
        assert_eq!(turn_marks(&events), ["end None Some(\"1\") None"]);
        assert_eq!(twr_key(&events[0], NO_RUN_KEY), Some(json!(true)));
        assert!(!t.busy() && t.deadline().is_none());

        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        feed(&mut t, &[&end]);
        let events = feed(&mut t, &[&result(0, Some(1.0), true)]);
        assert_eq!(turn_marks(&events), ["end None Some(\"1\") None"]);
        assert_eq!(twr_key(&events[0], NO_RUN_KEY), Some(json!(true)));
        assert!(!t.busy());
    }

    /// Claude 2.1.295+ folds a prompt written into a run it started itself into that run, and
    /// writes no `result` for it: once the main agent's message stopped (`end_turn`) and
    /// nothing came for the wait, the turn ends `completed`, marked `missing_result`. A late
    /// `result` of that run is absorbed; the next prompt's run is a turn as usual.
    #[test]
    fn a_missing_result_ends_the_turn_and_a_late_one_is_absorbed() {
        let mut t = Translator::new(TranslatorOptions {
            missing_result_after: Some(std::time::Duration::from_secs(90)),
            ..Default::default()
        });
        t.user_message("hi", 1);
        feed(
            &mut t,
            &[INIT, &reply("Hello."), &result(1, Some(1.0), false)],
        );
        t.user_message("Checking in", 2);
        let stop = |reason: &str| {
            stream(json!({"type": "message_delta", "delta": {"stop_reason": reason}}))
        };
        // A tool call's stop awaits no `result`; a new model message ends the wait.
        feed(&mut t, &[MESSAGE_START, &stop("tool_use")]);
        assert!(t.deadline().is_none());
        feed(&mut t, &[&reply("Read the findings."), &stop("end_turn")]);
        assert!(t.deadline().is_some());
        feed(&mut t, &[MESSAGE_START]);
        assert!(t.deadline().is_none(), "the model went on");
        // The folded prompt's answer, and no `result`.
        feed(
            &mut t,
            &[&reply("Done: fixed both findings."), &stop("end_turn")],
        );
        let at = t.deadline().unwrap();
        let events = t.expire(at);
        assert_eq!(turn_marks(&events.events), ["end Some(2) None None"]);
        assert_eq!(
            twr_key(&events.events[0], MISSING_RESULT_KEY),
            Some(json!(true))
        );
        assert!(!t.busy());
        // The next prompt is written before the late `result` comes: that `result` is still
        // the old run's, and the prompt's turn is not ended by it.
        t.user_message("next", 3);
        assert!(feed(&mut t, &[&result(3, Some(2.0), false)]).is_empty());
        let events = feed(
            &mut t,
            &[INIT, &reply("Next done."), &result(1, Some(2.5), false)],
        );
        assert_eq!(turn_marks(&events), ["end Some(3) None Some(0.5)"]);
    }

    /// A subagent's line is no run of the main agent's: it neither ends the wait for the
    /// notification's run nor keeps a `result` that asked the model nothing from ending it.
    #[test]
    fn a_subagents_line_is_no_run_of_the_notification() {
        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        feed(
            &mut t,
            &[&task_line(
                "task_notification",
                "b1",
                json!({"status": "completed"}),
            )],
        );
        let at = t.deadline().unwrap();
        let child = json!({"type": "stream_event", "parent_tool_use_id": "toolu_9",
                           "event": {"type": "message_start"}})
        .to_string();
        let child_text = json!({"type": "assistant", "parent_tool_use_id": "toolu_9",
                                "message": {"content": [{"type": "text", "text": "sub"}]}})
        .to_string();
        feed(
            &mut t,
            &[&child, &child_text, r#"{"type":"something_new"}"#],
        );
        assert_eq!(t.deadline(), Some(at));
        let events = feed(&mut t, &[&result(0, Some(1.0), true)]);
        assert_eq!(turn_marks(&events), ["end None Some(\"1\") None"]);
        assert_eq!(twr_key(&events[0], NO_RUN_KEY), Some(json!(true)));
    }

    /// After a turn ended for its missing `result`, another background task ends (its
    /// notification's turn opens) and then the old run's `result` comes: it is absorbed, and
    /// the new turn waits for its own run.
    #[test]
    fn a_late_result_does_not_end_a_notifications_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        feed(
            &mut t,
            &[&task_line(
                "task_started",
                "b2",
                json!({"is_backgrounded": true, "task_type": "local_bash"}),
            )],
        );
        t.user_message("Checking in", 2);
        feed(
            &mut t,
            &[
                INIT,
                MESSAGE_START,
                &reply("On it."),
                &stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
            ],
        );
        let events = t.expire(t.deadline().unwrap()).events;
        assert_eq!(turn_marks(&events), ["end Some(2) None None"]);
        let events = feed(
            &mut t,
            &[&task_line(
                "task_notification",
                "b2",
                json!({"status": "completed"}),
            )],
        );
        assert_eq!(turn_marks(&events), ["start None Some(\"1\")"]);
        let at = t.deadline().unwrap();
        assert!(feed(&mut t, &[&result(2, Some(2.0), false)]).is_empty());
        assert_eq!(t.deadline(), Some(at), "still waiting for its run");
        let events = feed(
            &mut t,
            &[
                MESSAGE_START,
                &reply("b2 done."),
                &result(1, Some(2.5), true),
            ],
        );
        assert_eq!(turn_marks(&events), ["end None Some(\"1\") Some(0.5)"]);
    }

    /// A `result` that asked the model nothing (another task's notice) while a turn waits for
    /// its missing `result` leaves that wait as it was.
    #[test]
    fn a_notice_result_keeps_the_wait_for_a_missing_result() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        feed(
            &mut t,
            &[
                INIT,
                MESSAGE_START,
                &reply("Done."),
                &stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
            ],
        );
        let at = t.deadline().unwrap();
        assert!(turn_marks(&feed(&mut t, &[&result(0, Some(1.0), true)])).is_empty());
        assert_eq!(t.deadline(), Some(at));
        let events = t.expire(at).events;
        assert_eq!(turn_marks(&events), ["end Some(1) None None"]);
    }

    /// The hub's prompt raced the turn a background task's end opened: Claude folds it into its
    /// own run and writes no `result`. Both the runtime's turn and the prompt's end.
    #[test]
    fn a_prompt_folded_into_a_runtime_run_ends_with_it() {
        let mut t = Translator::new(TranslatorOptions::default());
        with_background_task(&mut t);
        feed(
            &mut t,
            &[&task_line(
                "task_notification",
                "b1",
                json!({"status": "completed"}),
            )],
        );
        t.user_message("Checking in", 2);
        feed(
            &mut t,
            &[
                MESSAGE_START,
                &reply("The tests passed."),
                &stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
            ],
        );
        let events = t.expire(t.deadline().unwrap()).events;
        assert_eq!(
            turn_marks(&events),
            ["end None Some(\"1\") None", "end Some(2) None None"]
        );
        assert!(events
            .iter()
            .all(|e| twr_key(e, MISSING_RESULT_KEY) == Some(json!(true))));
        assert!(!t.busy());
    }

    /// A request for the user ends the wait for a `result`, and a stop with a request pending
    /// ends nothing.
    #[test]
    fn a_request_for_the_user_is_no_missing_result() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        feed(
            &mut t,
            &[
                INIT,
                MESSAGE_START,
                &reply("I need to run a command."),
                &stream(json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}})),
            ],
        );
        assert!(t.deadline().is_some());
        feed(
            &mut t,
            &[
                r#"{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Bash","input":{"command":"ls"},"tool_use_id":"tu1"}}"#,
            ],
        );
        assert!(t.deadline().is_none());
        assert!(t
            .expire(Utc::now() + chrono::Duration::hours(1))
            .events
            .is_empty());
        assert!(t.busy());
    }

    /// A child agent's tool call in a run its `init` started with no prompt never opens a turn
    /// of the runtime's own, also when a prompt was written after that `init`: a notification
    /// `result` then ends that run, and the prompt's turn ends with its own `result`.
    #[test]
    fn child_output_opens_no_runtime_turn_while_a_prompt_waits() {
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("hi", 1);
        let mut events = feed(
            &mut t,
            &[INIT, &reply("Hello."), &result(1, Some(1.0), false)],
        );
        events.extend(feed(&mut t, &[INIT]));
        t.user_message("and now?", 2);
        let child = r#"{"type":"assistant","parent_tool_use_id":"toolu_1","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"ls"}}]}}"#;
        let out = feed(&mut t, &[child]);
        assert!(
            out.iter()
                .all(|e| e.turn.is_none() && !matches!(e.body, EventBody::TurnStarted(_))),
            "{out:?}"
        );
        events.extend(out);
        events.extend(feed(
            &mut t,
            &[
                &result(0, Some(1.0), true),
                INIT,
                &reply("Done."),
                &result(1, Some(1.5), false),
            ],
        ));
        assert_eq!(
            turn_marks(&events),
            ["end Some(1) None Some(1.0)", "end Some(2) None Some(0.5)"]
        );
        assert!(!t.busy());
    }

    /// After a charged turn, what a notification `result` adds to the cumulative total is the
    /// next charged turn's; a total that went down counts whole, one that is missing changes
    /// nothing, and an interrupted turn leaves the spend for the next known one.
    #[test]
    fn notification_results_carry_their_spend_to_the_next_turn() {
        let mut t = Translator::new(TranslatorOptions::default());
        let mut marks = Vec::new();
        let turn = |t: &mut Translator, n: u64, total: f64, marks: &mut Vec<String>| {
            t.user_message("go", n);
            marks.extend(turn_marks(&feed(
                t,
                &[INIT, &reply("ok"), &result(1, Some(total), false)],
            )));
        };
        turn(&mut t, 1, 1.0, &mut marks);
        feed(&mut t, &[INIT, &result(0, Some(1.25), true)]);
        turn(&mut t, 2, 1.75, &mut marks);
        feed(&mut t, &[INIT, &result(0, None, true)]);
        feed(&mut t, &[INIT, &result(0, Some(0.5), true)]);
        turn(&mut t, 3, 0.75, &mut marks);
        feed(&mut t, &[INIT, &result(0, Some(1.0), true)]);
        t.user_message("go", 4);
        feed(&mut t, &[INIT, &reply("working")]);
        let _ = t.interrupt();
        marks.extend(turn_marks(&feed(
            &mut t,
            &[r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","stop_reason":null,"total_cost_usd":1.0,"session_id":"s"}"#],
        )));
        turn(&mut t, 5, 1.5, &mut marks);
        assert_eq!(
            marks,
            [
                "end Some(1) None Some(1.0)",
                "end Some(2) None Some(0.75)",
                "end Some(3) None Some(0.75)",
                "end Some(4) None None",
                "end Some(5) None Some(0.75)",
            ]
        );

        // A fresh process whose prompt's turn has model output when a notification comes: its
        // total is this process's spend, which that turn's result charges.
        let mut t = Translator::new(TranslatorOptions::default());
        t.user_message("go", 1);
        let mut events = feed(&mut t, &[INIT, &reply("working")]);
        events.extend(feed(&mut t, &[&result(0, Some(0.25), true)]));
        events.extend(feed(&mut t, &[&result(1, Some(0.5), false)]));
        assert_eq!(turn_marks(&events), ["end Some(1) None Some(0.5)"]);

        // A fresh process whose first run reports no total: a notification's total after it
        // is this process's spend, not the session's earlier one.
        let mut t = Translator::new(TranslatorOptions::default());
        let mut marks = Vec::new();
        t.user_message("go", 1);
        marks.extend(turn_marks(&feed(
            &mut t,
            &[INIT, &reply("ok"), &result(1, None, false)],
        )));
        feed(&mut t, &[INIT, &result(0, Some(0.25), true)]);
        turn(&mut t, 2, 0.5, &mut marks);
        assert_eq!(
            marks,
            ["end Some(1) None None", "end Some(2) None Some(0.5)"]
        );
    }

    #[test]
    fn question_answer_shapes() {
        let input = json!({"questions": [
            {"question": "A?", "options": [{"label": "x"}, {"label": "deny"}]},
            {"question": "B?", "options": [{"label": "y"}]},
        ]});
        let a0 = |a: &Answer| question_answer(&input, 0, "A?", a);
        let a1 = |a: &Answer| question_answer(&input, 1, "B?", a);
        assert_eq!(a0(&Answer::new("x")).as_deref(), Some("x"));
        // No `|` splitting: the text is this question's answer as given.
        assert_eq!(a0(&Answer::new("x | y")).as_deref(), Some("x | y"));
        // Free text (AskUserQuestion's "Other") is an answer.
        assert_eq!(a1(&Answer::new("green")).as_deref(), Some("green"));
        let with_msg = Answer {
            decision: "allow".into(),
            message: Some("typed".into()),
            ..Default::default()
        };
        assert_eq!(a0(&with_msg).as_deref(), Some("typed"));
        // A bare allow carries no answer; deny / decline decline unless they are this
        // question's labels.
        assert_eq!(a0(&Answer::allow()), None);
        assert_eq!(a0(&Answer::new("decline")), None);
        assert_eq!(a0(&Answer::deny()).as_deref(), Some("deny"));
        assert_eq!(a1(&Answer::deny()), None);
        let mut ans = Answer::new("ignored");
        ans.extra.insert("answers".into(), json!({"A?": "1"}));
        assert_eq!(a0(&ans).as_deref(), Some("1"));
        assert_eq!(a1(&ans).as_deref(), Some("ignored"));
    }
}

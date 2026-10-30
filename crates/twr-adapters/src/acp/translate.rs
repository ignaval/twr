//! Pure, vendor-neutral translation between an Agent Client Protocol (ACP) agent's stdio
//! stream and twr events.
//!
//! [`Translator`] has no IO: the driver ([`super::driver`]) writes [`Translator::start`]'s
//! lines, feeds it every stdout line ([`Translator::translate`]) and asks it for the lines of
//! each command ([`Translator::send`], [`Translator::respond`], [`Translator::stop`]). What
//! differs between agents lives in a [`Dialect`]: [`super::Standard`] for any ACP agent, and
//! `crate::grok_build::acp::Grok` for Grok Build's extensions on top.
//!
//! Handshake: `initialize{protocolVersion: 1, clientCapabilities: {fs: {readTextFile: false,
//! writeTextFile: false}, terminal: false}, clientInfo}`, then one of
//! - `session/new{cwd, mcpServers[, _meta]}` for a new session;
//! - `session/resume{sessionId, cwd, mcpServers[, _meta]}` when `initialize` advertises
//!   `agentCapabilities.sessionCapabilities.resume`; the result's `sessionId` may be absent (the
//!   requested one is kept);
//! - else `session/load{...}` when it advertises `loadSession`; the history the agent replays
//!   as `session/update` before answering is dropped (it was recorded when it happened), except
//!   `current_mode_update` / `config_option_update`, which are applied like the open result's
//!   own mode report (checked once the session is open);
//!
//! A `session/update` counts only when its `sessionId` is the session's: one naming none or
//! another is ignored. Mode reports that arrive while the session opens (before the open
//! result names it) are kept with the session they name, and only the open session's are
//! applied; nothing counts before the session opens.
//! - else the session fails ("advertises neither session/resume nor session/load").
//!
//! `_meta` is only sent when the dialect asks for it ([`Dialect::session_meta`]). A model
//! (`Options::model`) is then set with the request [`Dialect::model_request`] picks (the
//! standard one: `session/set_config_option{configId: <the model option>, value}` when the
//! session reports a `configOptions` entry of category `model`, else `session/set_model` when
//! it reports `models`, else the session fails: twr does not run a model it was not asked
//! for). A failed model request fails the session. The model the session runs on is the one
//! it reports ([`reported_model`]: the model option's `currentValue`, else
//! `models.currentModelId`) in the open result, a model request's answer (the requested model
//! when the answer reports none), a prompt result or a `config_option_update`:
//! `thread.started` `ext.<dialect>.model`, and a later change on the next `turn.ended` (where a
//! prompt result's `_meta.modelId` wins). A reasoning effort (`Options::effort`) is set next,
//! with the request [`Dialect::effort_request`] picks (the standard one:
//! `session/set_config_option{configId: <the effort option>, value}` for the `configOptions`
//! entry of category `thought_level`, else of id `reasoning_effort`, as the latest
//! configuration reports it; none: the session fails). A failed effort request fails the
//! session; the effort the session reports ([`reported_effort`]) is `thread.started`
//! `ext.<dialect>.effort`.
//!
//! Modes (fail closed): nothing is sent unless the agent reports
//! modes, as `modes{currentModeId, availableModes}` (set with `session/set_mode`) and/or as
//! `configOptions` entries of category `mode` (each set with `session/set_config_option`).
//! Each of these sources keeps the latest mode it reported, from wherever it came: the session
//! open result, the result of each `session/set_config_option` / `set_mode` / `set_model`
//! (ACP answers them with the whole current configuration; a model change may change the
//! mode), a prompt result, `current_mode_update` (the `modes` source) and
//! `config_option_update`. Every source is checked on its own ([`mode_acceptable`]): a report
//! is accepted when it is exactly the requested mode, or a known mode ([`mode_level`], whole
//! ids from explicit tables such as [`KNOWN_BYPASS`], never substrings) no wider than the
//! requested known mode; a mode twr does not know is accepted only when it is exactly the
//! requested one (or nothing was requested, see below), and for a requested mode twr does not
//! know only a [`KNOWN_RESTRICTED`] mode is narrower. With no requested mode (the agent's
//! default) only a [`KNOWN_BYPASS`] mode is refused: twr does not know the agent's vocabulary.
//! A requested bypass mode accepts anything. A source reporting a mode twr does not accept
//! gets a request (through that source) to [`narrowing_target`] of what it offers; when there
//! is none, the request fails, its result still reports such a mode, or the source reports
//! one again before every report was accepted, the session fails (a running prompt is
//! cancelled; no prompt is sent). Such a report while a prompt runs (the agent widened its own
//! mode mid-prompt, without the consent below) fails the session at once, the prompt
//! cancelled with `session/cancel`: a request to switch back would leave the prompt running in
//! the wider mode until the agent answered it. While the session opens, a requested mode the first source
//! offers under that id is also set once (a failure there is fine). The session becomes ready
//! only when every report is accepted, and the reports are checked again before each prompt
//! and whenever one changes. A session that reports no mode at all (and whose dialect does not
//! pin it, [`Dialect::mode_pinned`]) fails when a [`ModeLevel::Restricted`] mode was requested,
//! and otherwise runs with `ext.<dialect>.mode_unenforced = true`. A dialect can also refuse
//! the session ([`Dialect::veto`]), checked before it becomes ready and before every prompt.
//! A session id outside [`valid_session_id`] fails the session when it opens.
//!
//! Plan-exit consent: with `plan` requested, once the user allowed a plan-exit request in this
//! process (a dialect's plan approval answered `approved`, or a `session/request_permission`
//! for a `switch_mode` tool call answered with its `allow_once` option), a report of an
//! ask-level mode ([`ModeLevel::Ask`]) is accepted: it becomes the mode every report is
//! checked against, and the next `turn.ended` records `ext.<dialect>.mode_changed{from, to,
//! consent}` and `mode_note`. Anything wider than ask level, or leaving plan mode without that
//! consent, is handled like any other report twr does not accept. Prompts queue until all of that is done; then the session emits
//! `thread.started` and prompts run one at a time.
//!
//! | ACP (agent → twr)                                   | twr event                               |
//! |-----------------------------------------------------|-----------------------------------------|
//! | session ready                                       | `thread.started{session_id}` + `ext.twr.pid`, `ext.<dialect>.{version, model, mode, ...}` |
//! | `session/update` `agent_message_chunk` (text)        | `agent.text{delta}`; the run of chunks → `agent.text{final}` at the next boundary |
//! | `session/update` `agent_thought_chunk`               | one `agent.thinking` per run (only with `thinking`) |
//! | `session/update` `tool_call`                         | `agent.tool_call` (+ `agent.tool_result` if already completed/failed) |
//! | `session/update` `tool_call_update` completed/failed | `agent.tool_result` (once per call)     |
//! | `session/update` `current_mode_update` / `config_option_update` | the mode check above        |
//! | `session/request_permission` (agent → client request) | `attention.raised{permission}`, options `allow`/`deny` |
//! | a dialect's question / plan-approval request         | `attention.raised{question}` per question / `attention.raised{permission}` |
//! | `$/cancel_request` for a pending request            | `attention.resolved{by: timeout}` (withdrawal) |
//! | `session/prompt` response `stopReason`               | `turn.ended` (`end_turn` → completed, `cancelled` → interrupted, `max_tokens` / `max_turn_requests` / `refusal` / anything else → error, `ext.twr.error` saying which) |
//! | `session/prompt` error response                      | `turn.ended{error}` + `ext.<dialect>.error`, `ext.twr.error` its text |
//! | `initialize` / session open error (e.g. no auth)     | every waiting turn: `agent.text{final}` with the error + `turn.ended{error}` (`ext.twr.error` the same text); the driver stops |
//!
//! Usage ([`twr_core::usage`]): ACP has no rate-limit signal, so a dialect that has a read
//! of the plan's usage names it ([`Dialect::usage_request`], Grok Build's `_x.ai/billing`). It
//! is written once the session is ready and again when a prompt ends once the last read is
//! [`twr_core::usage::REFRESH_AFTER`] old, with ids of their own (from 2^50: the prompt ids
//! stay those of the recordings); its answer becomes `runtime.limits` (`source: read`,
//! [`Dialect::usage_windows`]) and an error stops the reads.
//!
//! Other `session/update` kinds (`plan`, `user_message_chunk`, `available_commands_update`,
//! `session_info_update`, vendor kinds) and notifications are ignored, except what the
//! dialect handles ([`Dialect::on_notification`]).
//!
//! Open items are immutable (see [`crate::attn`]): a request reusing the JSON-RPC id of a
//! pending one replaces it. The old items are withdrawn (`attention.resolved{by: timeout,
//! answer: null}`, `ext.<dialect>.superseded`), the new request is raised under a new id
//! (`perm-<id>~<n>`), and each id answers only with the content of the request it was raised
//! for; an answer to a withdrawn id is ignored.
//!
//! Answers (the relay rule: twr never answers on the user's behalf):
//! - permission: `allow` selects the first `allow_once` option, anything else the first
//!   `reject_once` option, falling back to the `cancelled` outcome. `allow_always` and
//!   `reject_always` are never chosen (they would persist a rule the user did not make). With
//!   no `allow_once` option the item offers only `deny`.
//! - question (dialect extension): once every question of the request has an answer (an
//!   option label or free text), `{outcome: accepted, answers: {<question>: <answer>}}`; a
//!   `deny`/`decline` answers `{outcome: cancelled}` and withdraws the request's other
//!   questions.
//! - plan approval (dialect extension): `allow` → `{outcome: approved}`, anything else →
//!   `{outcome: rejected}`.
//!
//! twr answers a pending request itself only when twr stops the session
//! ([`Translator::stop`]): permissions and questions get the `cancelled` outcome, a plan
//! approval an error. Requests from the agent twr does not know (`fs/*`, `terminal/*`, which
//! the client capabilities disclaim, and anything newer) get a JSON-RPC `-32601` error.
//! Responses to ids twr did not issue are ignored.

use std::collections::{HashMap, VecDeque};

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use twr_core::adapter::Answer;
use twr_core::event::{
    AgentText, AgentThinking, AgentToolCall, AgentToolResult, AttentionRaised, AttentionReason,
    AttentionResolved, EventBody, JsonMap, ResolvedBy, SessionPayload, TurnEnded, TurnResult,
    Usage,
};
use twr_core::usage::UsageWindow;

use crate::attn::IssuedIds;
use crate::claude_code::translate::{
    error_line, is_destructive, one_line, render_command, FULL_INPUT_KEY,
};
use crate::codex::translate::question_answer;
use crate::usage::{Limit, LimitKind};

/// ACP protocol version twr speaks.
pub const PROTOCOL_VERSION: u64 = 1;

/// Maximum length (chars) of a tool call's `input_summary`.
pub const INPUT_SUMMARY_MAX: usize = 160;

/// Maximum length (chars) of a tool result's `output_summary`.
pub const OUTPUT_SUMMARY_MAX: usize = 200;

/// Maximum length (chars) of a plan kept in `ext.<dialect>.plan`.
pub const PLAN_MAX: usize = 4000;

/// ACP tool kinds whose calls count as risky.
pub const RISKY_KINDS: &[&str] = &["execute", "edit", "delete", "move"];

/// `ext.twr.error` of a turn whose `session/prompt` response was too long to read.
pub const OVERSIZED_ERROR: &str = "oversized runtime message";

/// A request of the agent that a dialect maps onto one of twr's generic request kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtRequest {
    /// `{sessionId, toolCallId, questions: [{question, options: [{label}], multiSelect}]}`,
    /// answered `{outcome: accepted, answers: {question: answer}}` or `{outcome: cancelled}`.
    Question,
    /// `{sessionId, toolCallId, planContent}`, answered `{outcome: approved|rejected}`.
    PlanApproval,
}

/// What a dialect reads from a tool call besides the standard fields.
#[derive(Debug, Clone, Default)]
pub struct ToolMeta {
    /// ACP tool kind to use when the update has none.
    pub kind: Option<&'static str>,
    /// The agent's own tool name (`ext.<dialect>.tool`).
    pub name: String,
    /// The MCP tool the call runs through the agent's own tool, and its input, when the
    /// agent wraps MCP calls (grok's `use_tool`): the name as the agent gives it
    /// (`<server>__<tool>`, `ext.<dialect>.mcp_tool`). The call's summary is then that name
    /// and its input, not the wrapper's.
    pub mcp_call: Option<(String, Value)>,
}

/// What a dialect may look at when a notification arrives.
#[derive(Debug, Clone, Copy)]
pub struct SessionCtx<'a> {
    /// The open session, if any.
    pub session_id: Option<&'a str>,
    /// The requested twr mode (`default` when empty).
    pub requested_mode: &'a str,
}

/// What differs between ACP agents: the standard protocol ([`super::Standard`]) or a vendor's
/// extensions on top of it (Grok Build's).
pub trait Dialect: std::fmt::Debug + Send + 'static {
    /// Key of this runtime's extras (`data.ext.<key>`).
    fn ext_key(&self) -> &'static str;

    /// Prefix of log lines (the adapter's name).
    fn log_name(&self) -> &'static str;

    /// The agent's name in messages (`grok`, `gemini`).
    fn name(&self) -> &str;

    /// The agent's name at the start of a phrase or as an owner (`Grok`).
    fn title(&self) -> &str {
        self.name()
    }

    /// `_meta` of `session/new|load|resume`; None sends none.
    fn session_meta(&self) -> Option<Value> {
        None
    }

    /// Sees the `initialize` result.
    fn on_initialized(&mut self, _result: &Value) {}

    /// The agent's version from the `initialize` result (standard: `agentInfo.version`).
    fn agent_version(&self, result: &Value) -> Option<String> {
        result
            .pointer("/agentInfo/version")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The request setting `model` on session `sid`, given the session's open result:
    /// `(method, params)`, or why it cannot be set (the session then fails).
    fn model_request(
        &self,
        open: &Value,
        sid: &str,
        model: &str,
    ) -> Result<(&'static str, Value), String> {
        standard_model_request(self.name(), open, sid, model)
    }

    /// The request setting reasoning effort `effort` on session `sid`, given the latest
    /// `configOptions` the session reported (`{configOptions: [...]}`, `null` when none):
    /// `(method, params)`, or why it cannot be set (the session then fails).
    fn effort_request(
        &self,
        config: &Value,
        sid: &str,
        effort: &str,
    ) -> Result<(&'static str, Value), String> {
        standard_effort_request(self.name(), config, sid, effort)
    }

    /// Maps a request method of the agent onto a generic kind (None: not handled; a
    /// `session/request_permission` is always handled).
    fn extension_request(&self, _method: &str) -> Option<ExtRequest> {
        None
    }

    /// A notification other than `session/update` and `$/cancel_request`. Returns why the
    /// session must end (it allows more than requested), if it must.
    fn on_notification(
        &mut self,
        _method: &str,
        _params: &Value,
        _ctx: SessionCtx<'_>,
    ) -> Option<String> {
        None
    }

    /// Why the session must not run, from what the dialect has seen so far (e.g. a mode
    /// reported through a vendor notification before the session id was known). Checked before
    /// the session becomes ready and before every prompt; Some fails the session.
    fn veto(&self, _ctx: SessionCtx<'_>) -> Option<String> {
        None
    }

    /// Whether the agent's permission mode is fixed outside ACP (its command line), so a
    /// session reporting no mode is still in the requested one. When false, a session that
    /// reports no mode at all cannot be held to a [`ModeLevel::Restricted`] mode and fails;
    /// any other requested mode runs with `ext.<key>.mode_unenforced = true`.
    fn mode_pinned(&self) -> bool {
        false
    }

    /// Extra facts about a tool call update.
    fn tool_meta(&self, _update: &Value) -> ToolMeta {
        ToolMeta::default()
    }

    /// The text of a finished tool call's output, when the dialect wraps it in its own shape
    /// (grok's MCP results); `None` reads the standard `content`, else `rawOutput`.
    fn tool_output(&self, _update: &Value) -> Option<String> {
        None
    }

    /// Adds the dialect's facts to `thread.started` `ext.<key>`.
    fn ready_ext(&self, _ext: &mut Map<String, Value>) {}

    /// The request that reads the plan's usage without a turn, `(method, params)`; None (the
    /// default): the agent has none, nothing is read.
    fn usage_request(&self) -> Option<(&'static str, Value)> {
        None
    }

    /// The usage windows and the plan in the answer to [`Dialect::usage_request`].
    fn usage_windows(&self, _result: &Value) -> (Vec<UsageWindow>, Option<String>) {
        (Vec::new(), None)
    }

    /// Appended to an error that says authentication is required.
    fn auth_hint(&self) -> &str {
        " (sign in with the agent's own CLI first)"
    }

    /// The kind of limit a prompt error's text (`error_text`) says the account hit, if it says
    /// one: the turn then carries `ext.twr.limit` and is not transient. None (the default):
    /// the standard dialect knows no agent's wording.
    fn limit_kind(&self, _text: &str) -> Option<LimitKind> {
        None
    }

    /// The text of the first prompt of a new session, when the dialect changes it (the
    /// standard dialect prepends the system prompt: ACP has no field for one).
    fn first_prompt(&self, _text: &str) -> Option<String> {
        None
    }

    /// The prompt that compacts the session's context (`/compact`), when the agent has one
    /// twr knows ([`Translator::compact`]); None: twr cannot compact it.
    fn compact_prompt(&self) -> Option<&'static str> {
        None
    }

    /// What a vendor notification says of a compaction of the session's context, if it is one
    /// ([`Compaction`]).
    fn compaction(&self, _method: &str, _params: &Value) -> Option<Compaction> {
        None
    }

    /// The context window of `model` (the session's current one, when known), if the agent
    /// said.
    fn context_window(&self, _model: Option<&str>) -> Option<u64> {
        None
    }
}

/// A compaction of the session's context, as a vendor notification reports it
/// ([`Dialect::compaction`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Compaction {
    /// It began, with the context this full (and the window), when said.
    Started {
        tokens: Option<u64>,
        window: Option<u64>,
    },
    /// It ended, from `before` to `after` tokens, taking `elapsed_ms`, when said.
    Completed {
        before: Option<u64>,
        after: Option<u64>,
        elapsed_ms: Option<u64>,
    },
    /// It failed, or was cancelled: the context is as it was.
    Failed(String),
}

/// The standard model request: `session/set_config_option` for a `configOptions` entry of
/// category `model` (or id `model`), else `session/set_model` when the session reports
/// `models`, else an error naming `agent`.
pub fn standard_model_request(
    agent: &str,
    open: &Value,
    sid: &str,
    model: &str,
) -> Result<(&'static str, Value), String> {
    let option = open
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|a| {
            a.iter()
                .find(|o| str_at(o, "category") == "model")
                .or_else(|| a.iter().find(|o| str_at(o, "id") == "model"))
        });
    if let Some(o) = option.filter(|o| !str_at(o, "id").is_empty()) {
        return Ok((
            "session/set_config_option",
            json!({"sessionId": sid, "configId": str_at(o, "id"), "value": model}),
        ));
    }
    if open.get("models").is_some_and(Value::is_object) {
        return Ok((
            "session/set_model",
            json!({"sessionId": sid, "modelId": model}),
        ));
    }
    Err(format!(
        "{agent} reports no way to choose the model (no `configOptions` model option, no \
         `models`); leave the model unset or give it in the runtime's command line"
    ))
}

/// The `configOptions` entry of a session configuration `r` that holds the reasoning effort:
/// the one of ACP category `thought_level`, else the one with id `reasoning_effort`.
fn effort_option(r: &Value) -> Option<&Value> {
    let a = r.get("configOptions").and_then(Value::as_array)?;
    a.iter()
        .find(|o| str_at(o, "category") == "thought_level")
        .or_else(|| a.iter().find(|o| str_at(o, "id") == "reasoning_effort"))
        .filter(|o| !str_at(o, "id").is_empty())
}

/// The standard reasoning effort request: `session/set_config_option` for the session's effort
/// option ([`effort_option`]; grok 1.0.44 reports `reasoning_effort`, category
/// `thought_level`), else an error naming `agent` (ACP has no other way to set it).
pub fn standard_effort_request(
    agent: &str,
    config: &Value,
    sid: &str,
    effort: &str,
) -> Result<(&'static str, Value), String> {
    match effort_option(config) {
        Some(o) => Ok((
            "session/set_config_option",
            json!({"sessionId": sid, "configId": str_at(o, "id"), "value": effort}),
        )),
        None => Err(format!(
            "{agent} reports no way to choose the reasoning effort (no `configOptions` entry of \
             category thought_level); leave the effort unset"
        )),
    }
}

/// The reasoning effort a session configuration `r` reports: the `currentValue` of its effort
/// option ([`effort_option`]).
pub fn reported_effort(r: &Value) -> Option<String> {
    effort_option(r)?
        .get("currentValue")
        .and_then(Value::as_str)
        .filter(|e| !e.is_empty())
        .map(str::to_string)
}

/// Static facts about the session a translator serves.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Emit `agent.thinking`.
    pub thinking: bool,
    /// Runtime process id (`ext.twr.pid`).
    pub pid: Option<u32>,
    /// Program the process was started as (`ext.twr.exe`).
    pub exe: Option<String>,
    /// Working directory sent as the session `cwd` (absolute, per ACP).
    pub cwd: String,
    /// ACP `mcpServers` entries.
    pub mcp_servers: Vec<Value>,
    /// Session to resume instead of starting one.
    pub resume_session: Option<String>,
    /// The mode the session is checked against (empty = `default`).
    pub mode: String,
    /// Model set once the session is open ([`Dialect::model_request`]).
    pub model: Option<String>,
    /// Reasoning effort set once the session is open, after the model
    /// ([`Dialect::effort_request`]).
    pub effort: Option<String>,
}

/// One normalized event.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// Kind + payload.
    pub body: EventBody,
    /// Turn it belongs to (None for `thread.started` or out-of-turn output).
    pub turn: Option<u64>,
    /// The raw stdout line, kept for non-delta events.
    pub raw: Option<String>,
}

/// What one input produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translated {
    /// Events to emit, in order.
    pub events: Vec<Emitted>,
    /// Lines to write to the agent's stdin, in order.
    pub writes: Vec<String>,
    /// The session cannot continue (initialize or session open failed): the driver stops the
    /// process. The text explains why.
    pub fatal: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Before [`Translator::start`].
    Idle,
    /// `initialize` sent.
    Initializing,
    /// `session/new|load|resume` sent.
    Opening,
    /// The model or mode request sent.
    Configuring,
    /// Prompts may run.
    Ready,
    /// The session could not be opened.
    Failed,
}

#[derive(Debug, Clone)]
enum Req {
    Initialize,
    Open {
        method: &'static str,
    },
    /// The model request, while opening the session.
    SetModel {
        model: String,
        method: &'static str,
    },
    /// The reasoning effort request, while opening the session (after the model's).
    SetEffort {
        effort: String,
        method: &'static str,
    },
    /// A mode request through `source` (while opening the session, or to take back a mode
    /// twr does not accept that the session reported later).
    SetMode {
        source: ModeSource,
        mode: String,
    },
    Prompt {
        turn: u64,
    },
    /// A usage read ([`Dialect::usage_request`]).
    Usage,
}

/// A way the session reports (and sets) its mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeSource {
    /// `modes.currentModeId` / `current_mode_update`, set with `session/set_mode`.
    Modes,
    /// The `configOptions` entry of category `mode` with this id, set with
    /// `session/set_config_option`.
    Option(String),
}

impl std::fmt::Display for ModeSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ModeSource::Modes => f.write_str("modes"),
            ModeSource::Option(id) => write!(f, "config option {id}"),
        }
    }
}

/// The latest mode one source reported.
#[derive(Debug, Clone)]
struct ModeReport {
    source: ModeSource,
    /// None: the source was reported without a current mode (not accepted).
    current: Option<String>,
    /// The modes the source offers.
    available: Vec<String>,
}

/// What a pending agent → client request waits for.
#[derive(Debug, Clone)]
enum Waiting {
    /// `session/request_permission`: the option ids an answer may select.
    Permission {
        allow: Option<String>,
        reject: Option<String>,
        /// The tool call is a mode switch (ACP tool kind `switch_mode`, e.g. Claude's
        /// `ExitPlanMode` through claude-code-acp): the standard plan-exit request.
        plan_exit: bool,
    },
    /// A plan approval ([`ExtRequest::PlanApproval`]).
    PlanExit,
    /// Question `index` of a question request ([`ExtRequest::Question`]).
    Question { index: usize },
}

#[derive(Debug, Clone)]
struct Pending {
    /// JSON-RPC id of the agent's request.
    rpc_id: Value,
    waiting: Waiting,
    turn: Option<u64>,
}

/// The questions of one question request.
#[derive(Debug, Clone)]
struct QuestionSet {
    attn_ids: Vec<String>,
    questions: Vec<String>,
    labels: Vec<Vec<String>>,
    answers: Vec<Option<String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ToolState {
    done: bool,
}

/// Stateful ACP ↔ twr translator for one agent process.
#[derive(Debug)]
pub struct Translator<D: Dialect> {
    tasks: crate::background::Tasks,
    opts: Options,
    dialect: D,
    phase: Phase,
    next_id: u64,
    outstanding: HashMap<u64, Req>,
    session_id: Option<String>,
    open_method: Option<&'static str>,
    /// The `session/new|load|resume` result (the model request looks at it).
    open_result: Value,
    agent_version: Option<String>,
    can_resume: bool,
    can_load: bool,
    /// The latest mode the session reported through each way it reports one (`modes` first).
    mode_reports: Vec<ModeReport>,
    /// Sources a mode request was sent to since the reports were last all acceptable: a source
    /// still unacceptable after its request fails the session.
    mode_attempts: Vec<ModeSource>,
    mode_set: Option<Value>,
    /// The model was set.
    model_set: bool,
    /// The model the session reports ([`reported_model`]), or the one twr set when the answer
    /// reported none.
    model: Option<String>,
    /// The reasoning effort was set.
    effort_set: bool,
    /// The reasoning effort the session reports ([`reported_effort`]), or the one twr set when
    /// the answer reported none.
    effort: Option<String>,
    /// The latest `configOptions` the session reported (the effort request looks at it: a
    /// model change may change which efforts there are).
    config_options: Value,
    /// The model last recorded on `thread.started` / `turn.ended` (`ext.<dialect>.model`).
    model_logged: Option<String>,
    queued: VecDeque<(String, u64)>,
    /// The running prompt: (request id, turn).
    in_flight: Option<(u64, u64)>,
    /// A prompt was sent in this process.
    prompted: bool,
    /// A mode request is outstanding: no prompt starts until it is answered and every report
    /// is acceptable.
    mode_pending: bool,
    text: String,
    thought: String,
    pub(crate) tools: HashMap<String, ToolState>,
    /// Pending agent requests by attention id.
    pending: HashMap<String, Pending>,
    /// Questions of pending question requests, by the request's id key.
    question_sets: HashMap<String, QuestionSet>,
    /// Attention ids handed out (never reused while remembered).
    issued: IssuedIds,
    stopping: bool,
    /// The mode reports are checked against once the user consented to leaving plan mode
    /// ([`Translator::consented_plan_exit`]); None: the requested one.
    allowed_mode: Option<String>,
    /// The user allowed a plan-exit request in this process.
    plan_exit_approved: bool,
    /// A consented mode change not yet recorded on a `turn.ended`.
    mode_changed: Option<Value>,
    /// The mode reports (`current_mode_update` / `config_option_update`) that arrived while
    /// the session opened, before its id was known, with the session id each named: the
    /// open result names the session, and only its own are applied
    /// ([`Translator::on_opened`]). At most [`OPENING_UPDATES_MAX`], the latest.
    opening_updates: VecDeque<(String, Value)>,
    /// When to read the plan's usage again ([`Dialect::usage_request`]).
    usage: crate::usage::Reads,
    /// The id of the next usage read (their own space, from 2^50).
    next_usage_id: u64,
    /// The clock usage reads are timed by.
    clock: fn() -> DateTime<Utc>,
    /// Turns sent as the dialect's compaction prompt ([`Translator::compact`]) not ended yet.
    compact_turns: std::collections::HashSet<u64>,
    /// The compaction under way, as [`Dialect::compaction`] reported its start: its trigger and
    /// the context before.
    compacting: Option<(&'static str, Option<u64>)>,
    /// How full the context is (the prompt result's `_meta.totalTokens`, Grok Build's), and
    /// the window a compaction's start reported.
    context: Option<u64>,
    reported_window: Option<u64>,
}

/// `ext.<dialect>.mode_note` of the turn in which the session left plan mode with consent.
pub const PLAN_EXIT_NOTE: &str = "the user approved leaving plan mode, so the session continues \
    in the ask-level mode the agent switched to";

impl<D: Dialect> Translator<D> {
    /// A translator for a fresh process speaking `dialect`.
    pub fn with_dialect(opts: Options, dialect: D) -> Self {
        Translator {
            opts,
            dialect,
            phase: Phase::Idle,
            next_id: 1,
            outstanding: HashMap::new(),
            session_id: None,
            open_method: None,
            open_result: Value::Null,
            agent_version: None,
            can_resume: false,
            can_load: false,
            mode_reports: Vec::new(),
            mode_attempts: Vec::new(),
            mode_set: None,
            model_set: false,
            model: None,
            effort_set: false,
            effort: None,
            config_options: Value::Null,
            model_logged: None,
            queued: VecDeque::new(),
            in_flight: None,
            prompted: false,
            mode_pending: false,
            text: String::new(),
            thought: String::new(),
            tools: HashMap::new(),
            tasks: crate::background::Tasks::default(),
            pending: HashMap::new(),
            question_sets: HashMap::new(),
            issued: IssuedIds::default(),
            stopping: false,
            allowed_mode: None,
            plan_exit_approved: false,
            mode_changed: None,
            opening_updates: VecDeque::new(),
            usage: crate::usage::Reads::default(),
            next_usage_id: 1 << 50,
            clock: Utc::now,
            compact_turns: Default::default(),
            compacting: None,
            context: None,
            reported_window: None,
        }
    }

    /// Times usage reads by `clock` instead of the system clock (tests).
    pub fn set_clock(&mut self, clock: fn() -> DateTime<Utc>) {
        self.clock = clock;
    }

    /// Reads the plan's usage ([`Dialect::usage_request`]) when the session is ready and a
    /// read is due.
    fn read_usage(&mut self, out: &mut Translated) {
        let now = (self.clock)();
        if self.phase != Phase::Ready || self.stopping || !self.usage.due(now) {
            return;
        }
        let Some((method, params)) = self.dialect.usage_request() else {
            return;
        };
        self.usage.ask(now);
        let id = self.next_usage_id;
        self.next_usage_id += 1;
        self.outstanding.insert(id, Req::Usage);
        out.writes.push(
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
    }

    /// The dialect.
    pub fn dialect(&self) -> &D {
        &self.dialect
    }

    /// Session id, once the session is open.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// The session this process opens or has open: the open session's id, else the one being
    /// resumed (None while a new session opens).
    fn known_session(&self) -> Option<&str> {
        self.session_id
            .as_deref()
            .or(self.opts.resume_session.as_deref())
    }

    /// Why the dialect says the session must not run (yet or any more), if it does.
    fn dialect_veto(&self) -> Option<String> {
        let ctx = SessionCtx {
            session_id: self.known_session(),
            requested_mode: self.requested_mode(),
        };
        self.dialect.veto(ctx)
    }

    /// Whether prompts can run.
    pub fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    /// Attention ids waiting for an answer.
    pub fn pending_attention(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.pending.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// The session mode the agent reports (after any mode request): the first report twr does
    /// not accept for the requested mode, if any, else the first one (`modes` before a mode
    /// config option).
    pub fn current_mode(&self) -> Option<&str> {
        self.unacceptable_report()
            .and_then(|r| r.current.as_deref())
            .or_else(|| self.mode_reports.iter().find_map(|r| r.current.as_deref()))
    }

    /// The modes the agent offers (through `modes`, else its first mode config option).
    pub fn available_modes(&self) -> &[String] {
        self.mode_reports
            .first()
            .map_or(&[][..], |r| r.available.as_slice())
    }

    fn log(&self) -> &'static str {
        self.dialect.log_name()
    }

    fn ext_of(&self, m: Map<String, Value>) -> Option<JsonMap> {
        ext_of(self.dialect.ext_key(), m)
    }

    fn request(&mut self, method: &str, params: Value, req: Req) -> (u64, String) {
        let id = self.next_id;
        self.next_id += 1;
        self.outstanding.insert(id, req);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        (id, line.to_string())
    }

    /// The `initialize` request. Call once, first.
    pub fn start(&mut self) -> Translated {
        let mut out = Translated::default();
        if self.phase != Phase::Idle {
            return out;
        }
        self.phase = Phase::Initializing;
        let (_, line) = self.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {
                    "fs": {"readTextFile": false, "writeTextFile": false},
                    "terminal": false,
                },
                "clientInfo": {"name": "twr", "version": env!("CARGO_PKG_VERSION")},
            }),
            Req::Initialize,
        );
        out.writes.push(line);
        out
    }

    /// Starts turn `turn` with `text`: a `session/prompt` now, or queued until the session is
    /// ready and the running prompt ended.
    pub fn send(&mut self, text: &str, turn: u64) -> Translated {
        let mut out = Translated::default();
        if self.phase == Phase::Failed {
            let msg = format!("the {} session is not open", self.dialect.title());
            self.end_failed_turn(turn, &msg, None, &mut out);
            return out;
        }
        self.queued.push_back((text.to_string(), turn));
        self.pump(&mut out);
        out
    }

    /// Starts turn `turn` as a compaction of the session's context: the dialect's compaction
    /// prompt ([`Dialect::compact_prompt`], sent as any prompt), or, for an agent twr cannot
    /// compact, the turn ends `error` saying so.
    pub fn compact(&mut self, turn: u64) -> Translated {
        let Some(prompt) = self.dialect.compact_prompt() else {
            let mut out = Translated::default();
            let msg = format!(
                "{} cannot compact its context from twr",
                self.dialect.title()
            );
            self.end_failed_turn(turn, &msg, None, &mut out);
            return out;
        };
        self.compact_turns.insert(turn);
        self.send(prompt, turn)
    }

    /// A compaction a vendor notification reports ([`Dialect::compaction`]), in the running
    /// prompt's turn (`manual` when that prompt is twr's compaction).
    fn on_compaction(&mut self, c: Compaction, line: &str, out: &mut Translated) {
        let turn = self.turn();
        let manual = turn.is_some_and(|t| self.compact_turns.contains(&t));
        let trigger = if manual { "manual" } else { "auto" };
        let body = match c {
            Compaction::Started { tokens, window } => {
                if window.is_some() {
                    self.reported_window = window;
                }
                // Said already (the user's, when its prompt went out): what it began with.
                if let Some((_, pre)) = self.compacting.as_mut() {
                    *pre = tokens.or(*pre);
                    return;
                }
                self.compacting = Some((trigger, tokens.or(self.context)));
                EventBody::ContextCompacting(twr_core::event::ContextCompacting {
                    trigger: Some(trigger.into()),
                    ..Default::default()
                })
            }
            Compaction::Completed {
                before,
                after,
                elapsed_ms,
            } => {
                let (trigger, pre) = self.compacting.take().unwrap_or((trigger, None));
                // It replaced the context: what it left, unknown when it did not say (the
                // size before is no longer the context).
                self.context = after;
                EventBody::ContextCompacted(twr_core::event::ContextCompacted {
                    trigger: trigger.into(),
                    pre_tokens: before.or(pre),
                    post_tokens: after,
                    duration_ms: elapsed_ms,
                    ..Default::default()
                })
            }
            Compaction::Failed(why) => {
                let (trigger, _) = self.compacting.take().unwrap_or((trigger, None));
                EventBody::ContextCompacted(twr_core::event::ContextCompacted {
                    trigger: trigger.into(),
                    error: Some(why),
                    ..Default::default()
                })
            }
        };
        out.events.push(Emitted {
            body,
            turn,
            raw: Some(line.to_string()),
        });
    }

    fn pump(&mut self, out: &mut Translated) {
        if self.phase != Phase::Ready
            || self.in_flight.is_some()
            || self.stopping
            || self.mode_pending
        {
            return;
        }
        if self.queued.is_empty() {
            return;
        }
        if let Some(why) = self.dialect_veto() {
            self.refuse_mode(why, "", out);
            return;
        }
        // Checked again right before each prompt: no prompt runs while any report is a mode
        // twr does not accept (it is switched first, or the session fails).
        if self.unacceptable_report().is_some() {
            self.settle_modes("", out);
            return;
        }
        let Some((mut text, turn)) = self.queued.pop_front() else {
            return;
        };
        if !self.prompted && self.opts.resume_session.is_none() {
            if let Some(t) = self.dialect.first_prompt(&text) {
                text = t;
            }
        }
        self.prompted = true;
        let sid = self.session_id.clone().unwrap_or_default();
        let (id, line) = self.request(
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
            Req::Prompt { turn },
        );
        self.in_flight = Some((id, turn));
        out.writes.push(line);
        // The user's compaction starts now: Grok Build reports only its end (1.0.62), so its
        // start is said here, which holds the hub's stall checks while it runs.
        if self.compact_turns.contains(&turn) {
            self.on_compaction(
                Compaction::Started {
                    tokens: None,
                    window: None,
                },
                "",
                out,
            );
            if let Some(e) = out.events.last_mut() {
                e.raw = None;
            }
        }
    }

    /// Answers item `attn_id`. None if it is not pending.
    pub fn respond(&mut self, attn_id: &str, answer: &Answer) -> Option<Translated> {
        let p = self.pending.get(attn_id)?.clone();
        let mut out = Translated::default();
        let result = match &p.waiting {
            Waiting::Permission {
                allow,
                reject,
                plan_exit,
            } => {
                self.pending.remove(attn_id);
                if *plan_exit && answer.is_allow() && allow.is_some() {
                    self.plan_exit_approved = true;
                }
                let option = if answer.is_allow() {
                    if allow.is_none() {
                        tracing::warn!(
                            attn_id,
                            "{}: no allow_once option; answering allow with a reject",
                            self.log()
                        );
                    }
                    allow.clone().or_else(|| reject.clone())
                } else {
                    reject.clone()
                };
                let outcome = match option {
                    Some(id) => json!({"outcome": "selected", "optionId": id}),
                    None => json!({"outcome": "cancelled"}),
                };
                json!({"outcome": outcome})
            }
            Waiting::PlanExit => {
                self.pending.remove(attn_id);
                // Any outcome but `approved` / `abandoned` is "revise the plan" to grok 1.0.44.
                let outcome = if answer.is_allow() {
                    self.plan_exit_approved = true;
                    "approved"
                } else {
                    "rejected"
                };
                json!({"outcome": outcome})
            }
            Waiting::Question { index } => {
                let key = id_key(&p.rpc_id);
                let set = self.question_sets.get_mut(&key)?;
                let i = *index;
                self.pending.remove(attn_id);
                match question_answer(&set.questions[i], &set.labels[i], answer) {
                    None => {
                        // Declined: the request is answered, its other questions withdrawn.
                        let set = self.question_sets.remove(&key).expect("present above");
                        for (j, id) in set.attn_ids.iter().enumerate() {
                            if j != i && self.pending.remove(id).is_some() {
                                out.events.push(self.withdrawal(
                                    id.clone(),
                                    "sibling_declined",
                                    p.turn,
                                    "",
                                ));
                            }
                        }
                        json!({"outcome": "cancelled"})
                    }
                    Some(text) => {
                        set.answers[i] = Some(text);
                        if set.answers.iter().any(Option::is_none) {
                            // Waiting for the request's other questions.
                            return Some(out);
                        }
                        let set = self.question_sets.remove(&key).expect("present above");
                        let answers: Map<String, Value> = set
                            .questions
                            .iter()
                            .zip(&set.answers)
                            .map(|(q, a)| (q.clone(), json!(a.clone().unwrap_or_default())))
                            .collect();
                        json!({"outcome": "accepted", "answers": answers})
                    }
                }
            }
        };
        out.writes
            .push(json!({"jsonrpc": "2.0", "id": p.rpc_id, "result": result}).to_string());
        Some(out)
    }

    /// Stops the session: `session/cancel` for a running prompt, and an answer to every
    /// pending request (ACP requires one): `{outcome: cancelled}` for permissions and
    /// questions, an error for a plan approval. This is twr stopping, not an answer on the
    /// user's behalf, and emits no event: the hub resolves the items as `thread_stopped`.
    /// Queued prompts are dropped.
    pub fn stop(&mut self) -> Translated {
        let mut out = Translated::default();
        if self.stopping {
            return out;
        }
        self.stopping = true;
        self.queued.clear();
        if self.in_flight.is_some() {
            if let Some(sid) = &self.session_id {
                out.writes.push(cancel_line(sid));
            }
        }
        let mut pending: Vec<_> = self.pending.drain().collect();
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        self.question_sets.clear();
        let mut answered: Vec<Value> = Vec::new();
        for (_, p) in pending {
            if answered.contains(&p.rpc_id) {
                continue;
            }
            answered.push(p.rpc_id.clone());
            out.writes.push(stopped_reply(&p.rpc_id, &p.waiting));
        }
        out
    }

    /// Cancel the running prompt without closing or changing the session.
    pub fn interrupt(&mut self) -> Translated {
        let mut out = Translated::default();
        if self.in_flight.is_some() {
            if let Some(sid) = &self.session_id {
                out.writes.push(cancel_line(sid));
            }
        }
        out
    }

    /// A stdout line longer than the reader's cap was dropped; `head` is its start and `len`
    /// its length. The response to the running `session/prompt` still ends the turn
    /// (`turn.ended{error}` with `ext.twr.error = "oversized runtime message"`); anything else
    /// is only logged.
    pub fn oversized(&mut self, head: &str, len: usize) -> Translated {
        let mut out = Translated::default();
        let Some((id, turn)) = self.in_flight else {
            return out;
        };
        if crate::process::head_response_id(head) != Some(id) {
            return out;
        }
        self.outstanding.remove(&id);
        let e = json!({"code": -32603, "message": OVERSIZED_ERROR});
        self.on_prompt_done(&json!(id), turn, Some(&e), &Value::Null, "", &mut out);
        for ev in &mut out.events {
            if let EventBody::TurnEnded(t) = &mut ev.body {
                ev.raw = None;
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
        if line.is_empty() {
            return out;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v @ Value::Object(_)) => v,
            Ok(_) | Err(_) => {
                tracing::warn!(line = %one_line(line, 200), "{}: ignoring non-JSON stdout line", self.log());
                return out;
            }
        };
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id").filter(|x| !x.is_null());
        match (method, id) {
            (Some(m), Some(id)) => self.on_request(m, id.clone(), &v, line, &mut out),
            (Some(m), None) => self.on_notification(m, &v, line, &mut out),
            (None, Some(id)) => self.on_response(id, &v, line, &mut out),
            (None, None) => {}
        }
        out
    }

    // --- responses ---------------------------------------------------------------------------

    fn on_response(&mut self, id: &Value, v: &Value, line: &str, out: &mut Translated) {
        let Some(req) = id.as_u64().and_then(|n| self.outstanding.remove(&n)) else {
            tracing::debug!(%id, "{}: ignoring a response to an id twr did not issue", self.log());
            return;
        };
        let error = v.get("error").filter(|e| !e.is_null());
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        match req {
            Req::Usage => match error {
                Some(e) => {
                    tracing::debug!(error = %e, "{}: the usage read failed; no more usage reads", self.log());
                    self.usage.stop();
                }
                None => {
                    let (windows, plan) = self.dialect.usage_windows(&result);
                    self.usage.answered(&windows);
                    if !windows.is_empty() {
                        out.events.push(Emitted {
                            body: crate::usage::limits(windows, "read", plan),
                            turn: None,
                            raw: None,
                        });
                    }
                }
            },
            Req::Initialize => match error {
                Some(e) => self.fail("initialize", e, line, out),
                None => self.on_initialized(&result, out),
            },
            Req::Open { method } => match error {
                Some(e) => self.fail(method, e, line, out),
                None => self.on_opened(method, result, line, out),
            },
            Req::SetModel { model, method } => match error {
                Some(e) => {
                    let e =
                        json!({"message": format!("setting model {model:?}: {}", error_text(e))});
                    self.fail(method, &e, line, out);
                }
                None => {
                    self.model_set = true;
                    // The model is the one set, unless the result reports another.
                    self.model = Some(model);
                    // The result carries the session's whole configuration, and a model
                    // change may have changed the mode too: checked by `configure`.
                    self.apply_config(&result);
                    self.configure(line, out);
                }
            },
            Req::SetEffort { effort, method } => match error {
                Some(e) => {
                    let e = json!({"message": format!(
                        "setting reasoning effort {effort:?}: {}",
                        error_text(e)
                    )});
                    self.fail(method, &e, line, out);
                }
                None => {
                    self.effort_set = true;
                    // The effort is the one set, unless the result reports another.
                    self.effort = Some(effort);
                    self.apply_config(&result);
                    self.configure(line, out);
                }
            },
            Req::SetMode { source, mode } => {
                self.on_mode_set(source, mode, error, &result, line, out)
            }
            Req::Prompt { turn } => {
                if error.is_none() {
                    // Checked before the next prompt (`pump`).
                    self.apply_config(&result);
                }
                self.on_prompt_done(id, turn, error, &result, line, out)
            }
        }
    }

    fn on_initialized(&mut self, r: &Value, out: &mut Translated) {
        self.dialect.on_initialized(r);
        self.agent_version = self.dialect.agent_version(r);
        let caps = &r["agentCapabilities"];
        self.can_load = caps.get("loadSession").and_then(Value::as_bool) == Some(true);
        self.can_resume = caps
            .pointer("/sessionCapabilities/resume")
            .is_some_and(|x| !x.is_null());
        let mut params = json!({
            "cwd": self.opts.cwd,
            "mcpServers": self.opts.mcp_servers,
        });
        if let Some(meta) = self.dialect.session_meta() {
            params["_meta"] = meta;
        }
        let method = match self.opts.resume_session.clone() {
            None => "session/new",
            Some(sid) => {
                params["sessionId"] = json!(sid);
                if self.can_resume {
                    "session/resume"
                } else if self.can_load {
                    "session/load"
                } else {
                    let e = json!({"message": format!(
                        "{} advertises neither session/resume nor session/load",
                        self.dialect.name()
                    )});
                    self.fail("session/resume", &e, "", out);
                    return;
                }
            }
        };
        self.phase = Phase::Opening;
        self.open_method = Some(method);
        let (_, line) = self.request(method, params, Req::Open { method });
        out.writes.push(line);
    }

    fn on_opened(&mut self, method: &'static str, r: Value, line: &str, out: &mut Translated) {
        let sid = r
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| self.opts.resume_session.clone());
        let Some(sid) = sid else {
            let e = json!({"message": format!("{method} returned no sessionId")});
            self.fail(method, &e, line, out);
            return;
        };
        // The id goes, unquoted, into attach command lines and resume requests.
        if !valid_session_id(&sid) {
            let e = json!({"message": format!(
                "{method} returned the session id {:?}, which twr does not accept (1 to 256 of \
                 A-Z a-z 0-9 . _ : -, not starting with -)",
                one_line(&sid, 80)
            )});
            self.fail(method, &e, line, out);
            return;
        }
        // The mode reports that arrived while it opened, this session's only (another
        // session's mode says nothing of this one's), before the result's own.
        for (theirs, update) in std::mem::take(&mut self.opening_updates) {
            if theirs == sid {
                self.on_update(&update, line, out);
            } else {
                tracing::debug!(
                    session = theirs,
                    "{}: ignoring a mode report for another session",
                    self.log()
                );
            }
        }
        self.session_id = Some(sid);
        self.apply_config(&r);
        self.open_result = r;
        self.phase = Phase::Configuring;
        self.configure(line, out);
    }

    /// Applies the session configuration `r` reports: a `modes` object (`currentModeId`,
    /// `availableModes`) and every `configOptions` entry of category `mode` (or with the id of
    /// a mode option already known). Every report is applied, wherever it comes from: the
    /// session open result, a `session/set_config_option` / `set_mode` / `set_model` result
    /// (ACP agents answer them with their whole current configuration, and changing the model
    /// may change the mode too), a prompt result, a `config_option_update`. Each source keeps
    /// its latest value (an omitted entry leaves it unchanged); every one is checked on its own
    /// ([`Translator::settle_modes`]). Returns whether a mode source was reported.
    ///
    /// The model `r` reports, if any ([`reported_model`]), becomes the session's model.
    fn apply_config(&mut self, r: &Value) -> bool {
        if let Some(m) = reported_model(r) {
            self.model = Some(m);
        }
        if let Some(options) = r.get("configOptions").filter(|o| o.is_array()) {
            self.config_options = json!({ "configOptions": options });
        }
        if let Some(e) = reported_effort(r) {
            self.effort = Some(e);
        }
        let mut reported = false;
        if let Some(modes) = r.get("modes").filter(|m| m.is_object()) {
            reported = true;
            let rep = self.mode_report(ModeSource::Modes);
            if let Some(a) = modes.get("availableModes").and_then(Value::as_array) {
                rep.available = a
                    .iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect();
            }
            if let Some(c) = modes
                .get("currentModeId")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                rep.current = Some(c.to_string());
            }
        }
        for o in r
            .get("configOptions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = str_at(o, "id");
            if id.is_empty() {
                continue;
            }
            let source = ModeSource::Option(id.to_string());
            let known = self.mode_reports.iter().any(|r| r.source == source);
            if !known && str_at(o, "category") != "mode" {
                continue;
            }
            reported = true;
            let rep = self.mode_report(source);
            if o.get("options").is_some() {
                rep.available = option_values(o);
            }
            if let Some(c) = o
                .get("currentValue")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
            {
                rep.current = Some(c.to_string());
            }
        }
        reported
    }

    /// The report of `source`, added (with no current mode yet) if new; `modes` stays first.
    fn mode_report(&mut self, source: ModeSource) -> &mut ModeReport {
        let i = match self.mode_reports.iter().position(|r| r.source == source) {
            Some(i) => i,
            None => {
                let i = if source == ModeSource::Modes {
                    0
                } else {
                    self.mode_reports.len()
                };
                self.mode_reports.insert(
                    i,
                    ModeReport {
                        source,
                        current: None,
                        available: Vec::new(),
                    },
                );
                i
            }
        };
        &mut self.mode_reports[i]
    }

    /// The first report twr does not accept for the requested mode ([`mode_acceptable`]; a
    /// source with no current mode is not accepted).
    fn unacceptable_report(&self) -> Option<&ModeReport> {
        let requested = self.requested_mode();
        self.mode_reports.iter().find(|r| {
            !r.current
                .as_deref()
                .is_some_and(|c| mode_acceptable(c, requested))
        })
    }

    /// The mode request setting `mode` through `source`.
    fn mode_request(&mut self, source: ModeSource, mode: String) -> String {
        let sid = self.session_id.clone().unwrap_or_default();
        let (method, params) = match &source {
            ModeSource::Option(id) => (
                "session/set_config_option",
                json!({"sessionId": sid, "configId": id, "value": mode}),
            ),
            ModeSource::Modes => (
                "session/set_mode",
                json!({"sessionId": sid, "modeId": mode}),
            ),
        };
        self.mode_pending = true;
        self.mode_attempts.push(source.clone());
        self.request(method, params, Req::SetMode { source, mode })
            .1
    }

    /// The session is open: set the model, then the reasoning effort, then check the mode,
    /// then become ready.
    fn configure(&mut self, line: &str, out: &mut Translated) {
        let sid = self.session_id.clone().unwrap_or_default();
        if !self.model_set {
            if let Some(model) = self.opts.model.clone().filter(|m| !m.is_empty()) {
                match self.dialect.model_request(&self.open_result, &sid, &model) {
                    Ok((method, params)) => {
                        let (_, l) = self.request(method, params, Req::SetModel { model, method });
                        out.writes.push(l);
                    }
                    Err(why) => {
                        let e = json!({"message": format!("setting model {model:?}: {why}")});
                        self.fail("the model request", &e, line, out);
                    }
                }
                return;
            }
        }
        if !self.effort_set {
            if let Some(effort) = self.opts.effort.clone().filter(|e| !e.is_empty()) {
                match self
                    .dialect
                    .effort_request(&self.config_options, &sid, &effort)
                {
                    Ok((method, params)) => {
                        let (_, l) =
                            self.request(method, params, Req::SetEffort { effort, method });
                        out.writes.push(l);
                    }
                    Err(why) => {
                        let e = json!({"message": format!(
                            "setting reasoning effort {effort:?}: {why}"
                        )});
                        self.fail("the reasoning effort request", &e, line, out);
                    }
                }
                return;
            }
        }
        self.settle_modes(line, out);
    }

    /// Checks every mode report against the requested mode, and acts:
    /// - a report twr does not accept while a prompt runs fails the session at once (the
    ///   prompt is cancelled): the agent changed its own mode mid-prompt, and a request to
    ///   switch it back would leave the prompt running in that mode until it is answered;
    /// - a report twr does not accept ([`mode_acceptable`]) gets a mode request through its
    ///   own source (`session/set_mode` for `modes`, `session/set_config_option` for a mode
    ///   config option) to [`narrowing_target`] of what that source offers; with no target,
    ///   or when that source was already asked since the reports were last all acceptable,
    ///   the session fails (the running prompt is cancelled);
    /// - while the session opens, a requested mode the first source offers under that id and
    ///   does not report yet is set once (a failure is fine: the session stays in an accepted
    ///   mode);
    /// - with every report accepted, the session becomes ready (or the next prompt starts).
    ///
    /// Nothing happens while a mode request is outstanding: its answer comes back here.
    fn settle_modes(&mut self, line: &str, out: &mut Translated) {
        if self.mode_pending || !matches!(self.phase, Phase::Configuring | Phase::Ready) {
            return;
        }
        if let Some(why) = self.dialect_veto() {
            self.refuse_mode(why, line, out);
            return;
        }
        if let Some(c) = self.unacceptable_report().and_then(|r| r.current.clone()) {
            if self.consented_plan_exit(&c) {
                // Every source is checked again against the new mode.
                self.settle_modes(line, out);
                return;
            }
        }
        let requested = self.requested_mode().to_string();
        if let Some(r) = self.unacceptable_report().cloned() {
            let current = r.current.as_deref().unwrap_or("(none reported)");
            if self.phase == Phase::Ready && self.in_flight.is_some() {
                // The agent widened its own mode during a prompt, without the user's consent:
                // a request switching it back would leave the prompt running in the wider
                // mode until the agent answers, if it ever does.
                let why = format!(
                    "the session switched itself to mode {current:?} ({}) during a prompt; \
                     {current:?} allows more than {}, or twr does not know it, so the prompt \
                     is cancelled and the session ended",
                    r.source,
                    self.requested_text()
                );
                self.refuse_mode(why, line, out);
                return;
            }
            if self.mode_attempts.contains(&r.source) {
                let why = format!(
                    "the session reports mode {current:?} ({}) again after twr switched it; \
                     {current:?} allows more than {}, or twr does not know it",
                    r.source,
                    self.requested_text()
                );
                self.refuse_mode(why, line, out);
                return;
            }
            match narrowing_target(&requested, &r.available) {
                Some(mode) => {
                    tracing::warn!(
                        mode = current,
                        source = %r.source,
                        requested,
                        target = mode,
                        "{}: the session reports a mode twr does not accept; switching",
                        self.log()
                    );
                    let l = self.mode_request(r.source, mode);
                    out.writes.push(l);
                }
                None => {
                    let why = format!(
                        "the session is in mode {current:?} ({}), which allows more than {}, \
                         or twr does not know it, and {} offers no mode twr may switch to \
                         ({:?})",
                        r.source,
                        self.requested_text(),
                        self.dialect.name(),
                        r.available
                    );
                    self.refuse_mode(why, line, out);
                }
            }
            return;
        }
        if self.phase == Phase::Configuring && !requested.is_empty() {
            if let Some(r) = self.mode_reports.first().cloned() {
                if r.current.as_deref() != Some(requested.as_str())
                    && r.available.contains(&requested)
                    && !self.mode_attempts.contains(&r.source)
                {
                    let l = self.mode_request(r.source, requested);
                    out.writes.push(l);
                    return;
                }
            }
        }
        self.mode_attempts.clear();
        if self.phase == Phase::Configuring
            && self.mode_unenforced()
            && mode_level(&requested) == Some(ModeLevel::Restricted)
        {
            let why = format!(
                "{} reports no permission mode (no `modes`, no mode config option), so twr \
                 cannot hold the session to the restricted mode {requested:?}",
                self.dialect.name()
            );
            self.refuse_mode(why, line, out);
            return;
        }
        match self.phase {
            Phase::Configuring => self.become_ready(line, out),
            _ => self.pump(out),
        }
    }

    /// The session reports no mode at all and nothing else pins it ([`Dialect::mode_pinned`]):
    /// twr cannot see or set the mode the agent runs in.
    fn mode_unenforced(&self) -> bool {
        self.mode_reports.is_empty() && !self.dialect.mode_pinned()
    }

    /// The requested permission mode, trimmed (empty: none, the agent's default).
    fn requested_mode(&self) -> &str {
        self.allowed_mode
            .as_deref()
            .unwrap_or(self.opts.mode.trim())
    }

    /// The one widening the user consents to: the requested mode is the restricted `plan`, the
    /// user allowed a plan-exit request in this process ([`Translator::respond`]: a dialect's
    /// plan approval, or a `switch_mode` permission), and `report` is an ask-level mode
    /// ([`ModeLevel::Ask`]). Then `report` becomes the mode reports are checked against, and the
    /// change is recorded on the next `turn.ended` (`ext.<dialect>.mode_changed`, `mode_note`).
    /// Anything wider than ask level, or without that consent, is not accepted here.
    fn consented_plan_exit(&mut self, report: &str) -> bool {
        let from = self.requested_mode().to_string();
        let from_plan =
            mode_level(&from) == Some(ModeLevel::Restricted) && mode_key(&from) == "plan";
        if !(from_plan && self.plan_exit_approved && mode_level(report) == Some(ModeLevel::Ask)) {
            return false;
        }
        tracing::info!(
            from,
            to = report,
            "{}: the session left plan mode after the user approved the plan",
            self.log()
        );
        self.mode_changed = Some(json!({
            "from": from,
            "to": report,
            "consent": "plan exit approved by the user",
        }));
        self.allowed_mode = Some(report.trim().to_string());
        true
    }

    /// The requested mode for a message.
    fn requested_text(&self) -> String {
        match self.requested_mode() {
            "" => "the agent's default mode (no mode was requested)".to_string(),
            m => format!("the requested {m:?}"),
        }
    }

    /// The session allows more than requested and twr cannot take it back: the running prompt
    /// is cancelled and the session must not run.
    fn refuse_mode(&mut self, why: String, line: &str, out: &mut Translated) {
        tracing::error!(%why, "{}: refusing a session in a mode twr does not accept", self.log());
        self.mode_pending = false;
        if self.in_flight.is_some() {
            if let Some(sid) = &self.session_id {
                out.writes.push(cancel_line(sid));
            }
        }
        let e = json!({"message": why});
        self.fail("session/set_mode", &e, line, out);
    }

    /// The answer to a mode request through `source`. A failure is fine only while that
    /// source still reports an accepted mode; a success counts as `mode` (ACP answers
    /// `session/set_mode` with `{}`) unless the result reports otherwise, and a result still
    /// reporting a mode twr does not accept fails the session.
    fn on_mode_set(
        &mut self,
        source: ModeSource,
        mode: String,
        error: Option<&Value>,
        result: &Value,
        line: &str,
        out: &mut Translated,
    ) {
        if self.phase == Phase::Failed {
            return;
        }
        self.mode_pending = false;
        let requested = self.requested_mode().to_string();
        let accepted = |tr: &Self| {
            tr.mode_reports
                .iter()
                .find(|r| r.source == source)
                .and_then(|r| r.current.clone())
                .filter(|c| mode_acceptable(c, &requested))
        };
        match error {
            Some(e) => {
                tracing::warn!(error = %e, mode, %source, "{}: setting the session mode failed", self.log());
                self.mode_set = Some(json!({"mode": mode, "error": e}));
                if accepted(self).is_none() {
                    let current = self
                        .mode_reports
                        .iter()
                        .find(|r| r.source == source)
                        .and_then(|r| r.current.clone())
                        .unwrap_or_default();
                    let why = format!(
                        "setting mode {mode:?} ({source}) failed ({}): the session stays in \
                         mode {current:?}, which may allow more than {}",
                        error_text(e),
                        self.requested_text()
                    );
                    self.refuse_mode(why, line, out);
                    return;
                }
            }
            None => {
                self.mode_set = Some(json!({"mode": mode, "ok": true}));
                self.mode_report(source.clone()).current = Some(mode.clone());
                // What the result reports (its whole configuration, when it carries one) is
                // the mode now: one twr does not accept means the switch did not take.
                self.apply_config(result);
                if accepted(self).is_none() {
                    let current = self
                        .mode_reports
                        .iter()
                        .find(|r| r.source == source)
                        .and_then(|r| r.current.clone())
                        .unwrap_or_default();
                    let why = format!(
                        "the session reports mode {current:?} after twr switched it to \
                         {mode:?} ({source}); {current:?} allows more than {}, or twr does \
                         not know it",
                        self.requested_text()
                    );
                    self.refuse_mode(why, line, out);
                    return;
                }
            }
        }
        self.settle_modes(line, out);
    }

    fn become_ready(&mut self, line: &str, out: &mut Translated) {
        self.phase = Phase::Ready;
        let mut twr = Map::new();
        if let Some(pid) = self.opts.pid {
            twr.insert("pid".into(), json!(pid));
        }
        if let Some(exe) = &self.opts.exe {
            twr.insert("exe".into(), json!(exe));
        }
        let mut ours = Map::new();
        if let Some(v) = &self.agent_version {
            ours.insert("version".into(), json!(v));
        }
        ours.insert("resumed".into(), json!(self.opts.resume_session.is_some()));
        if let Some(m) = self.open_method {
            ours.insert("open_method".into(), json!(m));
        }
        if let Some(m) = &self.model {
            ours.insert("model".into(), json!(m));
            self.model_logged = Some(m.clone());
        }
        if let Some(e) = &self.effort {
            ours.insert("effort".into(), json!(e));
        }
        // Every report is accepted here (`settle_modes`): a bypass mode only when one was
        // requested.
        if let Some(m) = self.current_mode() {
            ours.insert("mode".into(), json!(m));
        }
        if self.mode_reports.len() > 1 {
            let all: Map<String, Value> = self
                .mode_reports
                .iter()
                .map(|r| (r.source.to_string(), json!(r.current)))
                .collect();
            ours.insert("mode_reports".into(), Value::Object(all));
        }
        if !self.available_modes().is_empty() {
            ours.insert("available_modes".into(), json!(self.available_modes()));
        }
        if let Some(s) = &self.mode_set {
            ours.insert("mode_set".into(), s.clone());
        }
        if self.mode_unenforced() {
            ours.insert("mode_unenforced".into(), json!(true));
        }
        self.dialect.ready_ext(&mut ours);
        let mut ext = JsonMap::new();
        if !twr.is_empty() {
            ext.insert("twr".into(), Value::Object(twr));
        }
        ext.insert(self.dialect.ext_key().into(), Value::Object(ours));
        out.events.push(Emitted {
            body: EventBody::ThreadStarted(SessionPayload {
                session_id: self.session_id.clone(),
                ext: Some(ext),
                ..Default::default()
            }),
            turn: None,
            raw: (!line.is_empty()).then(|| line.to_string()),
        });
        self.pump(out);
        self.read_usage(out);
    }

    /// The error text with the dialect's sign-in hint when it says authentication is needed.
    fn with_auth_hint(&self, text: &str) -> String {
        let mut hint = text.to_string();
        if text.contains("Authentication required") {
            hint.push_str(self.dialect.auth_hint());
        }
        hint
    }

    /// Why a prompt that ended with `stopReason` `stop` (not `end_turn` / `cancelled`) failed.
    fn stop_text(&self, stop: &str) -> String {
        let who = self.dialect.title();
        match stop {
            "" => format!("{who} ended the prompt without a stop reason"),
            "max_tokens" => format!("{who} hit its output token limit (stopReason max_tokens)"),
            "max_turn_requests" => {
                format!(
                    "{who} hit its limit of model requests per turn (stopReason max_turn_requests)"
                )
            }
            "refusal" => format!("{who} refused to continue (stopReason refusal)"),
            other => format!("{who} stopped the prompt (stopReason {other})"),
        }
    }

    /// The session cannot be used: every waiting turn ends with an error.
    fn fail(&mut self, stage: &str, e: &Value, line: &str, out: &mut Translated) {
        self.phase = Phase::Failed;
        // The turns end here; a late prompt response must not end them again.
        self.outstanding
            .retain(|_, r| !matches!(r, Req::Prompt { .. }));
        let msg = error_text(e);
        let text = format!("{} {stage} failed: {msg}", self.dialect.name());
        tracing::error!(stage, error = %e, "{}: the session could not be opened", self.log());
        let mut turns: Vec<u64> = self.in_flight.take().map(|(_, t)| t).into_iter().collect();
        turns.extend(self.queued.drain(..).map(|(_, t)| t));
        let raw = (!line.is_empty()).then_some(line);
        for turn in turns {
            self.end_failed_turn(turn, &text, raw, out);
        }
        out.fatal = Some(self.with_auth_hint(&text));
    }

    fn end_failed_turn(&self, turn: u64, text: &str, raw: Option<&str>, out: &mut Translated) {
        let hint = self.with_auth_hint(text);
        out.events.push(Emitted {
            body: EventBody::AgentText(AgentText {
                text: hint.clone(),
                is_final: true,
                ..Default::default()
            }),
            turn: Some(turn),
            raw: None,
        });
        let mut ours = Map::new();
        ours.insert("error".into(), json!({"message": hint}));
        ours.insert("transient".into(), json!(false));
        let mut ext = self.ext_of(ours);
        ext.get_or_insert_with(JsonMap::new)
            .insert("twr".into(), json!({ "error": error_line(&hint) }));
        out.events.push(Emitted {
            body: EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Error,
                session_id: self.session_id.clone(),
                ext,
                ..Default::default()
            }),
            turn: Some(turn),
            raw: raw.map(str::to_string),
        });
    }

    fn on_prompt_done(
        &mut self,
        id: &Value,
        turn: u64,
        error: Option<&Value>,
        r: &Value,
        line: &str,
        out: &mut Translated,
    ) {
        if self.in_flight.map(|(i, _)| i) == id.as_u64() {
            self.in_flight = None;
        }
        self.flush(Some(turn), out);
        // The agent is no longer blocked on requests of a finished prompt; the hub resolves
        // their items when the turn ends.
        self.pending.retain(|_, p| p.turn != Some(turn));
        let pending = &self.pending;
        self.question_sets
            .retain(|_, s| s.attn_ids.iter().any(|a| pending.contains_key(a)));
        // Completed tools are per prompt, but a still-running tool can finish after its
        // prompt response. Keep its identity so the terminal update closes the same task.
        self.tools.retain(|id, tool| {
            !tool.done && self.tasks.running.contains_key(&format!("tool:{id}"))
        });
        let mut ours = Map::new();
        let stop = r.get("stopReason").and_then(Value::as_str).unwrap_or("");
        // Every failed turn says why (`ext.twr.error`, what the hub quotes).
        let mut why = None;
        let mut limit = None;
        let result = match error {
            Some(e) => {
                let text = error_text(e);
                why = Some(self.with_auth_hint(&text));
                ours.insert("error".into(), e.clone());
                if let Some(kind) = self.dialect.limit_kind(&text) {
                    // Retrying before the reset cannot help.
                    ours.insert("transient".into(), json!(false));
                    limit = Some(Limit {
                        kind,
                        window: None,
                        resets_at: None,
                        message: text,
                    });
                } else {
                    let reason = transient_reason(e);
                    ours.insert("transient".into(), json!(reason.is_some()));
                    if let Some(r) = reason {
                        ours.insert("transient_reason".into(), json!(r));
                    }
                }
                TurnResult::Error
            }
            None => {
                ours.insert("stop_reason".into(), json!(stop));
                match stop {
                    "end_turn" => TurnResult::Completed,
                    "cancelled" => TurnResult::Interrupted,
                    _ => {
                        why = Some(self.stop_text(stop));
                        ours.insert("transient".into(), json!(false));
                        TurnResult::Error
                    }
                }
            }
        };
        let u = r
            .pointer("/_meta/usage")
            .or_else(|| r.get("usage"))
            .filter(|u| u.is_object());
        let usage = u.map(|u| {
            let tok = |keys: &[&str]| {
                keys.iter()
                    .find_map(|k| u.get(*k).and_then(Value::as_u64))
                    .unwrap_or(0)
            };
            Usage {
                input: tok(&["inputTokens", "input_tokens"]),
                output: tok(&["outputTokens", "output_tokens"]),
                cost: None,
            }
        });
        if let Some(u) = u {
            ours.insert("usage".into(), u.clone());
        }
        // The model the prompt ran on, else a model change the session reported since the
        // model was last recorded (a `config_option_update`, a result's configuration).
        let ran_on = r
            .pointer("/_meta/modelId")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        let changed = self
            .model
            .clone()
            .filter(|m| Some(m) != self.model_logged.as_ref());
        if let Some(m) = ran_on.or(changed) {
            ours.insert("model".into(), json!(m));
            self.model_logged = Some(m);
        }
        if let Some(change) = self.mode_changed.take() {
            ours.insert("mode_changed".into(), change);
            ours.insert("mode_note".into(), json!(PLAN_EXIT_NOTE));
        }
        let mut ext = self.ext_of(ours);
        if let Some(why) = why {
            ext.get_or_insert_with(JsonMap::new)
                .insert("twr".into(), json!({ "error": error_line(&why) }));
        }
        if let Some(l) = limit {
            twr_core::event::set_twr_ext(&mut ext, crate::usage::LIMIT_KEY, l.to_value());
        }
        self.compact_turns.remove(&turn);
        self.compacting = None;
        // How full the context is, when the agent says (Grok Build's `_meta.totalTokens`: the
        // last call's input and output; 0 for a turn that called no model, `/compact`'s).
        if let Some(n) = r
            .pointer("/_meta/totalTokens")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
        {
            self.context = Some(n);
        }
        let window = self
            .dialect
            .context_window(self.model.as_deref())
            .or(self.reported_window);
        let context = self
            .context
            .map(|tokens| twr_core::event::ContextSize { tokens, window });
        out.events.push(Emitted {
            body: EventBody::TurnEnded(TurnEnded {
                result,
                session_id: self.session_id.clone(),
                usage,
                context,
                ext,
                ..Default::default()
            }),
            turn: Some(turn),
            raw: Some(line.to_string()),
        });
        self.pump(out);
        self.read_usage(out);
    }

    // --- notifications -----------------------------------------------------------------------

    fn turn(&self) -> Option<u64> {
        self.in_flight.map(|(_, t)| t)
    }

    fn on_notification(&mut self, method: &str, v: &Value, line: &str, out: &mut Translated) {
        let params = &v["params"];
        match method {
            "session/update" => {
                // Every update names its session (ACP requires it): one that names none, or
                // another, says nothing about this session's mode.
                let Some(theirs) = params.get("sessionId").and_then(Value::as_str) else {
                    tracing::debug!("{}: ignoring an update that names no session", self.log());
                    return;
                };
                let update = &params["update"];
                let mode_report = matches!(
                    str_at(update, "sessionUpdate"),
                    "current_mode_update" | "config_option_update"
                );
                let Some(ours) = self.session_id.as_deref() else {
                    // Not open yet. While it opens, history replayed by session/load was
                    // recorded when it happened: only the mode the session reports counts. It
                    // is kept with the session it names until the open result says which
                    // session this is (`session/new` names it only there), then applied and
                    // checked like the open result's own report; nothing counts before.
                    if self.phase == Phase::Opening && mode_report {
                        if self.opening_updates.len() == OPENING_UPDATES_MAX {
                            self.opening_updates.pop_front();
                        }
                        self.opening_updates
                            .push_back((theirs.to_string(), update.clone()));
                    }
                    return;
                };
                if ours != theirs {
                    tracing::debug!(
                        session = theirs,
                        "{}: ignoring an update for another session",
                        self.log()
                    );
                    return;
                }
                self.on_update(update, line, out);
            }
            "$/cancel_request" | "$/cancelRequest" => {
                let rid = params.get("requestId").or_else(|| params.get("id"));
                let Some(rid) = rid.cloned() else { return };
                let mut ids: Vec<String> = self
                    .pending
                    .iter()
                    .filter(|(_, p)| p.rpc_id == rid)
                    .map(|(k, _)| k.clone())
                    .collect();
                ids.sort();
                self.question_sets.remove(&id_key(&rid));
                for attn_id in ids {
                    let p = self.pending.remove(&attn_id);
                    tracing::info!(%attn_id, "{}: runtime cancelled a pending request", self.log());
                    let w = self.withdrawal(attn_id, "runtime", p.and_then(|p| p.turn), line);
                    out.events.push(w);
                }
            }
            // Vendor extensions and anything newer: the dialect's.
            _ => {
                // A compaction of this session (another session's says nothing of this one).
                let ours = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_none_or(|s| self.session_id.as_deref().is_some_and(|own| own == s));
                if let Some(c) = self.dialect.compaction(method, params).filter(|_| ours) {
                    self.on_compaction(c, line, out);
                }
                let requested = self.requested_mode().to_string();
                let sid = self.known_session().map(str::to_string);
                let ctx = SessionCtx {
                    session_id: sid.as_deref(),
                    requested_mode: &requested,
                };
                let why = self.dialect.on_notification(method, params, ctx);
                if let Some(why) = why {
                    if self.phase != Phase::Failed {
                        self.refuse_mode(why, line, out);
                    }
                }
            }
        }
    }

    fn on_update(&mut self, u: &Value, line: &str, out: &mut Translated) {
        let turn = self.turn();
        match str_at(u, "sessionUpdate") {
            "agent_message_chunk" => {
                if !self.thought.is_empty() {
                    self.flush_thought(turn, out);
                }
                let text = content_text(&u["content"]);
                if text.is_empty() {
                    return;
                }
                self.text.push_str(&text);
                out.events.push(Emitted {
                    body: EventBody::AgentText(AgentText {
                        text,
                        delta: true,
                        ..Default::default()
                    }),
                    turn,
                    raw: None,
                });
            }
            "agent_thought_chunk" => {
                if self.opts.thinking {
                    if !self.text.is_empty() {
                        self.flush_text(turn, out);
                    }
                    self.thought.push_str(&content_text(&u["content"]));
                }
            }
            "tool_call" | "tool_call_update" => {
                self.flush(turn, out);
                self.on_tool(u, line, turn, out);
            }
            "current_mode_update" => {
                let m = str_at(u, "currentModeId");
                if !m.is_empty() {
                    self.mode_report(ModeSource::Modes).current = Some(m.to_string());
                    self.on_mode_report(line, out);
                }
            }
            // Every mode option's value, and the model ([`Translator::apply_config`]: a changed
            // model is recorded on the next `turn.ended`); other options are the agent's
            // business.
            "config_option_update" if self.apply_config(u) => self.on_mode_report(line, out),
            // plan, user_message_chunk, available_commands_update, session_info_update, ...:
            // nothing to show.
            _ => {}
        }
    }

    /// The session reported a mode on its own. Once the session is ready, a report twr does not
    /// accept is taken back (no new prompt starts meanwhile), or the session is ended
    /// ([`Translator::settle_modes`]); before that, opening the session checks it.
    fn on_mode_report(&mut self, line: &str, out: &mut Translated) {
        if self.phase == Phase::Ready && self.unacceptable_report().is_some() {
            self.settle_modes(line, out);
        }
    }

    fn on_tool(&mut self, u: &Value, line: &str, turn: Option<u64>, out: &mut Translated) {
        let id = str_at(u, "toolCallId").to_string();
        if id.is_empty() {
            return;
        }
        if !self.tools.contains_key(&id) {
            // Only a new `tool_call` reuses a finished id: a late update of a tool that
            // completed (its state gone with its prompt) never makes it run again.
            if str_at(u, "sessionUpdate") == "tool_call" {
                self.tasks.reopen(&format!("tool:{id}"));
            }
            self.tools.insert(id.clone(), ToolState::default());
            let meta = self.dialect.tool_meta(u);
            let kind = match str_at(u, "kind") {
                "" => meta.kind.unwrap_or("other"),
                k => k,
            }
            .to_string();
            let title = str_at(u, "title");
            let input = u.get("rawInput").cloned().unwrap_or(Value::Null);
            let destructive = is_destructive(&input) || is_destructive(&json!(title));
            let mut ours = Map::new();
            if !title.is_empty() {
                ours.insert("title".into(), json!(title));
            }
            if !meta.name.is_empty() {
                ours.insert("tool".into(), json!(meta.name));
            }
            if let Some((mcp_tool, _)) = &meta.mcp_call {
                ours.insert("mcp_tool".into(), json!(mcp_tool));
            }
            if let Some(locs) = u.get("locations").and_then(Value::as_array) {
                let paths: Vec<&str> = locs
                    .iter()
                    .filter_map(|l| l.get("path").and_then(Value::as_str))
                    .collect();
                if !paths.is_empty() {
                    ours.insert("paths".into(), json!(paths));
                }
            }
            if destructive {
                ours.insert("destructive".into(), json!(true));
            }
            let text_of = |input: &Value| match input {
                Value::Null => String::new(),
                Value::Object(m) if m.is_empty() => String::new(),
                other => other.to_string(),
            };
            let input_text = text_of(&input);
            let summary = if let Some((mcp_tool, mcp_input)) = &meta.mcp_call {
                // The MCP tool, then its own input: the wrapper's input names the tool after
                // its input (keys sort), so a long input would cut the name off the summary.
                match text_of(mcp_input) {
                    t if t.is_empty() => mcp_tool.clone(),
                    t => format!("{mcp_tool} {t}"),
                }
            } else if title.is_empty() {
                input_text
            } else if title == meta.name && !input_text.is_empty() {
                format!("{title} {input_text}")
            } else {
                title.to_string()
            };
            out.events.push(Emitted {
                body: EventBody::AgentToolCall(AgentToolCall {
                    call_id: id.clone(),
                    input_summary: one_line(&summary, INPUT_SUMMARY_MAX),
                    input_ref: None,
                    risky: RISKY_KINDS.contains(&kind.as_str()) || destructive,
                    tool: kind,
                    ext: self.ext_of(ours),
                    ..Default::default()
                }),
                turn,
                raw: Some(line.to_string()),
            });
        }
        let status = str_at(u, "status");
        let task_id = format!("tool:{id}");
        let output_progress = (status == "in_progress"
            || status.is_empty() && self.tasks.running.contains_key(&task_id))
            && (u.get("rawOutput").is_some_and(|v| !v.is_null())
                || u.get("content")
                    .and_then(Value::as_array)
                    .is_some_and(|v| !v.is_empty()));
        if output_progress || matches!(status, "in_progress" | "completed" | "failed") {
            let update = if output_progress {
                self.tasks.progress(&task_id, str_at(u, "title"))
            } else {
                self.tasks
                    .update(&task_id, status == "in_progress", str_at(u, "title"))
            };
            if let Some(task) = update {
                out.events.push(Emitted {
                    body: EventBody::TaskUpdated(task),
                    turn: None,
                    raw: Some(line.to_string()),
                });
            }
        }
        let state = self.tools.get_mut(&id).expect("inserted above");
        if matches!(status, "completed" | "failed") && !state.done {
            state.done = true;
            out.events.push(Emitted {
                body: EventBody::AgentToolResult(AgentToolResult {
                    call_id: id,
                    ok: status == "completed",
                    output_summary: match self.dialect.tool_output(u) {
                        Some(text) => one_line(&text, OUTPUT_SUMMARY_MAX),
                        None => tool_output_summary(u),
                    },
                    output_ref: None,
                    ..Default::default()
                }),
                turn,
                raw: Some(line.to_string()),
            });
        }
    }

    // --- requests from the agent -------------------------------------------------------------

    fn on_request(&mut self, method: &str, id: Value, v: &Value, line: &str, out: &mut Translated) {
        let waiting = match (method, self.dialect.extension_request(method)) {
            ("session/request_permission", _) => Waiting::Permission {
                allow: None,
                reject: None,
                plan_exit: false,
            },
            (_, Some(ExtRequest::PlanApproval)) => Waiting::PlanExit,
            (_, Some(ExtRequest::Question)) => Waiting::Question { index: 0 },
            (_, None) => {
                tracing::warn!(
                    method,
                    "{}: answering an unsupported agent request with an error",
                    self.log()
                );
                out.writes.push(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": format!("twr does not handle {method}")},
                    })
                    .to_string(),
                );
                return;
            }
        };
        if self.stopping {
            out.writes.push(stopped_reply(&id, &waiting));
            return;
        }
        let params = &v["params"];
        let turn = self.turn();
        self.flush(turn, out);
        // Open items never change: a request reusing a pending request's id replaced it (an
        // answer names only the id, so it would answer the replacement).
        let mut replaced: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| p.rpc_id == id)
            .map(|(k, _)| k.clone())
            .collect();
        replaced.sort();
        self.question_sets.remove(&id_key(&id));
        for attn_id in replaced {
            let p = self.pending.remove(&attn_id);
            tracing::info!(%attn_id, "{}: runtime replaced a pending request; withdrawing it", self.log());
            let w = self.withdrawal(attn_id, "superseded", p.and_then(|p| p.turn), line);
            out.events.push(w);
        }
        match waiting {
            Waiting::Permission { .. } => self.on_permission(id, params, line, turn, out),
            Waiting::PlanExit => self.on_plan_exit(id, method, params, line, turn, out),
            Waiting::Question { .. } => self.on_questions(id, method, params, line, turn, out),
        }
    }

    fn on_permission(
        &mut self,
        id: Value,
        params: &Value,
        line: &str,
        turn: Option<u64>,
        out: &mut Translated,
    ) {
        let call = &params["toolCall"];
        let call_id = str_at(call, "toolCallId").to_string();
        if !call_id.is_empty() && !self.tools.contains_key(&call_id) {
            self.on_tool(call, line, turn, out);
        }
        let options: Vec<&Value> = params["options"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let first = |kind: &str| {
            options
                .iter()
                .find(|o| str_at(o, "kind") == kind)
                .and_then(|o| o.get("optionId").and_then(Value::as_str))
                .map(str::to_string)
        };
        let mut allow = first("allow_once");
        // Never `reject_always`: it would persist a "never allow" rule the user did not ask
        // for. Without `reject_once` a deny answers `{outcome: cancelled}`.
        let reject = first("reject_once");
        let title = str_at(call, "title");
        let kind = str_at(call, "kind");
        let mut ours = Map::new();
        // What the user judges the request by: its title (often the command), else its input
        // as JSON. Shown as it runs, all of it in the item's context when the summary shows
        // only its start ([`render_command`]): an untitled input is no less what runs.
        let subject = match call.get("rawInput") {
            _ if !title.is_empty() => title.to_string(),
            Some(i) if !i.is_null() => i.to_string(),
            _ => String::new(),
        };
        let r = render_command(&subject, INPUT_SUMMARY_MAX);
        if let Some(full) = &r.full {
            ours.insert(FULL_INPUT_KEY.into(), json!(full));
        }
        let mut too_long = r.too_long;
        let what = r.line;
        // The agent's kind for the call, marked like the title (an invisible character in it
        // shows). One past `FULL_INPUT_MAX` marked is cut, and the request too long too.
        let label = if kind.is_empty() {
            "tool call".to_string()
        } else {
            let k = render_command(kind, usize::MAX);
            too_long |= k.too_long;
            k.full.unwrap_or(k.line)
        };
        let mut summary = if what.is_empty() {
            format!("Allow {label}?")
        } else {
            format!("Allow {label}: {what}")
        };
        if too_long {
            // The user cannot see all of what an allow would run.
            allow = None;
            summary = format!("[deny only: the request is too long to show in full] {summary}");
        }
        ours.insert("rpc_id".into(), id.clone());
        if !kind.is_empty() {
            ours.insert("tool_kind".into(), json!(kind));
        }
        if !title.is_empty() {
            ours.insert("title".into(), json!(title));
        }
        ours.insert(
            "options".into(),
            Value::Array(options.iter().map(|o| (*o).clone()).collect()),
        );
        if allow.is_none() {
            ours.insert("no_allow_once".into(), json!(true));
        }
        let offered = if allow.is_some() {
            vec!["allow".to_string(), "deny".to_string()]
        } else {
            vec!["deny".to_string()]
        };
        let attn_id = self.fresh_id(&format!("perm-{}", id_key(&id)));
        self.pending.insert(
            attn_id.clone(),
            Pending {
                rpc_id: id,
                waiting: Waiting::Permission {
                    allow,
                    reject,
                    plan_exit: kind == "switch_mode",
                },
                turn,
            },
        );
        out.events.push(Emitted {
            body: EventBody::AttentionRaised(AttentionRaised {
                attn_id,
                reason: AttentionReason::Permission,
                summary,
                options: offered,
                call_id: (!call_id.is_empty()).then_some(call_id),
                ext: self.ext_of(ours),
                ..Default::default()
            }),
            turn,
            raw: Some(line.to_string()),
        });
    }

    /// A plan approval (`{sessionId, toolCallId, planContent}`): the agent wants the plan
    /// approved to leave plan mode.
    fn on_plan_exit(
        &mut self,
        id: Value,
        method: &str,
        params: &Value,
        line: &str,
        turn: Option<u64>,
        out: &mut Translated,
    ) {
        let plan = str_at(params, "planContent");
        let call_id = str_at(params, "toolCallId").to_string();
        let first = plan
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or_default();
        let title = self.dialect.title();
        let summary = if first.is_empty() {
            format!("Approve {title}'s plan and leave plan mode?")
        } else {
            format!(
                "Approve {title}'s plan and leave plan mode: {}",
                one_line(first, INPUT_SUMMARY_MAX)
            )
        };
        let mut ours = Map::new();
        ours.insert("rpc_id".into(), id.clone());
        ours.insert("method".into(), json!(method));
        ours.insert("plan".into(), json!(truncate(plan, PLAN_MAX)));
        let attn_id = self.fresh_id(&format!("plan-{}", id_key(&id)));
        self.pending.insert(
            attn_id.clone(),
            Pending {
                rpc_id: id,
                waiting: Waiting::PlanExit,
                turn,
            },
        );
        out.events.push(Emitted {
            body: EventBody::AttentionRaised(AttentionRaised {
                attn_id,
                reason: AttentionReason::Permission,
                summary,
                options: vec!["allow".into(), "deny".into()],
                call_id: (!call_id.is_empty()).then_some(call_id),
                ext: self.ext_of(ours),
                ..Default::default()
            }),
            turn,
            raw: Some(line.to_string()),
        });
    }

    /// A question request (`{sessionId, toolCallId, questions: [{question, options: [{label,
    /// description}], multiSelect}], mode}`): one question item per question.
    fn on_questions(
        &mut self,
        id: Value,
        method: &str,
        params: &Value,
        line: &str,
        turn: Option<u64>,
        out: &mut Translated,
    ) {
        let mut qs: Vec<Value> = params
            .get("questions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if qs.is_empty() {
            // Still the user's to answer (or decline): one item with no text.
            qs.push(json!({}));
        }
        let call_id = str_at(params, "toolCallId").to_string();
        let n = qs.len();
        let key = format!("ask-{}", id_key(&id));
        let pending = &self.pending;
        let item = |b: &str, i: usize| question_item(b, if n == 1 { usize::MAX } else { i });
        let base = self
            .issued
            .fresh(&key, n, item, |x| pending.contains_key(x));
        let mut set = QuestionSet {
            attn_ids: Vec::new(),
            questions: Vec::new(),
            labels: Vec::new(),
            answers: vec![None; n],
        };
        for (i, q) in qs.iter().enumerate() {
            let attn_id = item(&base, i);
            let question = str_at(q, "question").to_string();
            let labels: Vec<String> = q["options"]
                .as_array()
                .map(|o| {
                    o.iter()
                        .filter_map(|o| o.get("label").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let text = if question.is_empty() {
                format!("{} asks a question", self.dialect.title())
            } else {
                question.clone()
            };
            let summary = if n > 1 {
                format!("({}/{n}) {text}", i + 1)
            } else {
                text
            };
            let mut ours = Map::new();
            ours.insert("rpc_id".into(), id.clone());
            ours.insert("method".into(), json!(method));
            ours.insert("question".into(), json!(question));
            if let Some(opts) = q.get("options").filter(|o| o.is_array()) {
                ours.insert("options".into(), opts.clone());
            }
            if q.get("multiSelect").and_then(Value::as_bool) == Some(true) {
                ours.insert("multi_select".into(), json!(true));
            }
            self.pending.insert(
                attn_id.clone(),
                Pending {
                    rpc_id: id.clone(),
                    waiting: Waiting::Question { index: i },
                    turn,
                },
            );
            set.attn_ids.push(attn_id.clone());
            set.questions.push(question);
            set.labels.push(labels.clone());
            out.events.push(Emitted {
                body: EventBody::AttentionRaised(AttentionRaised {
                    attn_id,
                    reason: AttentionReason::Question,
                    summary: one_line(&summary, 400),
                    options: labels,
                    call_id: (!call_id.is_empty()).then(|| call_id.clone()),
                    ext: self.ext_of(ours),
                    ..Default::default()
                }),
                turn,
                raw: Some(line.to_string()),
            });
        }
        self.question_sets.insert(id_key(&id), set);
    }

    /// A fresh attention id for a one-item request keyed `key`.
    fn fresh_id(&mut self, key: &str) -> String {
        let pending = &self.pending;
        self.issued.fresh_one(key, |x| pending.contains_key(x))
    }

    /// `attention.resolved{by: timeout, answer: null}` for an item this adapter withdraws
    /// (`cause`: `runtime` cancelled it, it was `superseded`, or a `sibling_declined`).
    fn withdrawal(&self, attn_id: String, cause: &str, turn: Option<u64>, line: &str) -> Emitted {
        let mut ours = Map::new();
        ours.insert("cancelled_by".into(), json!(cause));
        if cause == "superseded" {
            ours.insert("superseded".into(), json!(true));
        }
        Emitted {
            body: EventBody::AttentionResolved(AttentionResolved {
                attn_id,
                by: ResolvedBy::Timeout,
                answer: None,
                ext: self.ext_of(ours),
                ..Default::default()
            }),
            turn,
            raw: (!line.is_empty()).then(|| line.to_string()),
        }
    }

    // --- text runs ---------------------------------------------------------------------------

    fn flush(&mut self, turn: Option<u64>, out: &mut Translated) {
        self.flush_thought(turn, out);
        self.flush_text(turn, out);
    }

    fn flush_text(&mut self, turn: Option<u64>, out: &mut Translated) {
        if self.text.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.text);
        out.events.push(Emitted {
            body: EventBody::AgentText(AgentText {
                text,
                is_final: true,
                ..Default::default()
            }),
            turn,
            raw: None,
        });
    }

    fn flush_thought(&mut self, turn: Option<u64>, out: &mut Translated) {
        let text = std::mem::take(&mut self.thought);
        if text.trim().is_empty() {
            return;
        }
        out.events.push(Emitted {
            body: EventBody::AgentThinking(AgentThinking {
                text,
                ..Default::default()
            }),
            turn,
            raw: None,
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The `session/cancel` notification for session `sid`.
fn cancel_line(sid: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": sid}}).to_string()
}

/// The values a select config option offers (options may be grouped).
fn option_values(o: &Value) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        for e in v.as_array().into_iter().flatten() {
            match e.get("value").and_then(Value::as_str) {
                Some(s) => out.push(s.to_string()),
                None => walk(&e["options"], out),
            }
        }
    }
    let mut out = Vec::new();
    walk(&o["options"], &mut out);
    out
}

/// Item id `i` of a question request with id base `base` (`usize::MAX`: the only question).
fn question_item(base: &str, i: usize) -> String {
    if i == usize::MAX {
        base.to_string()
    } else {
        format!("{base}#q{i}")
    }
}

/// twr's answer to a pending request when twr stops the session.
fn stopped_reply(id: &Value, waiting: &Waiting) -> String {
    match waiting {
        Waiting::Permission { .. } => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": {"outcome": "cancelled"}}})
        }
        Waiting::Question { .. } => {
            json!({"jsonrpc": "2.0", "id": id, "result": {"outcome": "cancelled"}})
        }
        Waiting::PlanExit => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32000, "message": "twr stopped the session"},
        }),
    }
    .to_string()
}

/// The most mode reports kept while a session opens ([`Translator::opening_updates`]): the
/// latest, which are the ones that count.
const OPENING_UPDATES_MAX: usize = 256;

/// How much a known permission mode lets run without asking, narrowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ModeLevel {
    /// Plans or reads only, or denies what is not allowed ([`KNOWN_RESTRICTED`]).
    Restricted,
    /// Asks before edits and commands ([`KNOWN_ASK`]).
    Ask,
    /// Accepts file edits, asks for commands ([`KNOWN_ACCEPT_EDITS`]).
    AcceptEdits,
    /// Decides on its own what to ask ([`KNOWN_AUTO`]).
    Auto,
    /// Asks nothing ([`KNOWN_BYPASS`]).
    Bypass,
}

/// Mode ids that only plan, read, or deny what is not allowed: Claude Code / grok `plan` and
/// `dontAsk`, Gemini CLI / qwen-code / opencode `plan`, codex-acp `read-only`.
pub const KNOWN_RESTRICTED: &[&str] = &["plan", "dontAsk", "read-only"];

/// Mode ids that ask before edits and commands: `default` (Claude Code, Gemini CLI, grok),
/// `ask`, and twr's `manual`.
pub const KNOWN_ASK: &[&str] = &["default", "ask", "manual"];

/// Mode ids that accept file edits without asking: Claude Code / grok `acceptEdits`, Gemini
/// CLI / qwen-code `autoEdit`.
pub const KNOWN_ACCEPT_EDITS: &[&str] = &["acceptEdits", "autoEdit"];

/// Mode ids that decide on their own what to ask: Claude Code / grok / codex-acp `auto`.
pub const KNOWN_AUTO: &[&str] = &["auto"];

/// Mode ids that ask nothing: Claude Code / grok `bypassPermissions`, Gemini CLI / qwen-code
/// `yolo`, grok `always-approve`, codex-acp `full-access`, codex `danger-full-access`, and the
/// usual CLI flag names.
pub const KNOWN_BYPASS: &[&str] = &[
    "bypassPermissions",
    "yolo",
    "always-approve",
    "always-allow",
    "full-access",
    "danger-full-access",
    "dangerously-skip-permissions",
    "dangerously-allow-all",
    "allow-all",
];

/// A mode id compared with the tables: lowercase, without `-`, `_` and spaces (`read-only`,
/// `readOnly` and `read_only` are the same id). The whole id must match a table entry.
fn mode_key(mode: &str) -> String {
    mode.trim()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// The level of a known mode id (twr's or an agent's), or None for an id in no table.
pub fn mode_level(mode: &str) -> Option<ModeLevel> {
    let key = mode_key(mode);
    [
        (KNOWN_RESTRICTED, ModeLevel::Restricted),
        (KNOWN_ASK, ModeLevel::Ask),
        (KNOWN_ACCEPT_EDITS, ModeLevel::AcceptEdits),
        (KNOWN_AUTO, ModeLevel::Auto),
        (KNOWN_BYPASS, ModeLevel::Bypass),
    ]
    .into_iter()
    .find(|(ids, _)| ids.iter().any(|id| mode_key(id) == key))
    .map(|(_, level)| level)
}

/// Whether a mode id (twr's or an agent's) is a known bypass mode ([`KNOWN_BYPASS`]).
pub fn is_bypass(mode: &str) -> bool {
    mode_level(mode) == Some(ModeLevel::Bypass)
}

/// The runtime's own spelling of twr mode `mode` (trimmed, not empty), for a runtime whose
/// documented mode ids are `native`:
/// - exactly a native id: that id;
/// - a native id under another spelling ([`mode_key`]: case, `-`, `_` and spaces ignored, e.g.
///   `dont-ask` for `dontAsk`): that native id;
/// - else a known mode ([`mode_level`]): `by_level` of its level (never wider than it);
/// - else an error: twr does not know what the mode allows, so the runtime must not start.
pub fn native_mode(
    mode: &str,
    native: &[&'static str],
    by_level: impl Fn(ModeLevel) -> &'static str,
) -> Result<&'static str, String> {
    let mode = mode.trim();
    if let Some(n) = native.iter().find(|n| **n == mode) {
        return Ok(n);
    }
    let key = mode_key(mode);
    if let Some(n) = native.iter().find(|n| mode_key(n) == key) {
        return Ok(n);
    }
    match mode_level(mode) {
        Some(level) => Ok(by_level(level)),
        None => Err(format!(
            "unknown permission mode {mode:?}: twr does not know what it allows (use one of \
             {native:?}, or a mode twr knows such as plan, manual, acceptEdits, auto, \
             bypassPermissions)"
        )),
    }
}

/// Whether `id` may be used as an ACP session id: 1 to 256 characters from
/// `[A-Za-z0-9._:-]`, not starting with `-` (it is put, unquoted, into an attach command line,
/// where it must not read as an option).
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && !id.starts_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// Whether twr lets a session run in mode `report` when mode `requested` was asked for
/// (trimmed; empty: none, the agent's default). Fail closed:
/// - exactly the requested id: yes;
/// - nothing requested: any mode but a known bypass one (twr does not know the agent's
///   vocabulary; its default is what it reports);
/// - a known bypass mode requested: anything (nothing allows more);
/// - a known mode requested: a known mode of the same level or narrower (ids within a level
///   count as the same mode, e.g. `default` for `manual`);
/// - a mode twr does not know requested: only a [`KNOWN_RESTRICTED`] one;
/// - anything else, including every id twr does not know: no.
pub fn mode_acceptable(report: &str, requested: &str) -> bool {
    let (report, requested) = (report.trim(), requested.trim());
    if report.is_empty() {
        return false;
    }
    if report == requested {
        return true;
    }
    if requested.is_empty() {
        return !is_bypass(report);
    }
    match (mode_level(report), mode_level(requested)) {
        (_, Some(ModeLevel::Bypass)) => true,
        (Some(c), Some(r)) => c <= r,
        (Some(ModeLevel::Restricted), None) => true,
        _ => false,
    }
}

/// The mode to switch a source to when it reports a mode twr does not accept, from the modes
/// it offers (`available`), if any:
/// - the requested mode (`requested`, trimmed) when offered under exactly that id;
/// - else the widest offered known mode [`mode_acceptable`] for `requested`, no wider than
///   [`ModeLevel::Ask`] when nothing was requested (`default` before `ask` on a tie, then the
///   offered order). Ids twr does not know are never chosen.
///
/// None: nothing fits, and the caller fails the session.
pub fn narrowing_target(requested: &str, available: &[String]) -> Option<String> {
    let requested = requested.trim();
    if !requested.is_empty() {
        if let Some(m) = available.iter().find(|m| m.trim() == requested) {
            return Some(m.clone());
        }
    }
    let preference = |m: &str| match m {
        "default" => 0,
        "ask" => 1,
        _ => 2,
    };
    available
        .iter()
        .filter_map(|m| mode_level(m).map(|l| (l, m)))
        .filter(|(l, m)| {
            mode_acceptable(m, requested) && (!requested.is_empty() || *l <= ModeLevel::Ask)
        })
        .fold(None::<(ModeLevel, &String)>, |best, (l, m)| match best {
            Some((bl, bm)) if bl > l || (bl == l && preference(bm) <= preference(m)) => {
                Some((bl, bm))
            }
            _ => Some((l, m)),
        })
        .map(|(_, m)| m.clone())
}

/// Text of an ACP content block (`{type: text, text}`); other block types give "".
fn content_text(c: &Value) -> String {
    match c {
        Value::Object(_) if str_at(c, "type") == "text" => str_at(c, "text").to_string(),
        Value::Array(a) => a.iter().map(content_text).collect(),
        _ => String::new(),
    }
}

/// First ~[`OUTPUT_SUMMARY_MAX`] chars of a tool call's `content` (text blocks, diffs,
/// terminals) or, without content, its `rawOutput`.
fn tool_output_summary(u: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(items) = u.get("content").and_then(Value::as_array) {
        for it in items {
            match str_at(it, "type") {
                "content" => parts.push(content_text(&it["content"])),
                "diff" => parts.push(format!("[diff {}]", str_at(it, "path"))),
                "terminal" => parts.push("[terminal]".to_string()),
                _ => {}
            }
        }
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        text = match u.get("rawOutput") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string(),
        };
    }
    one_line(&text, OUTPUT_SUMMARY_MAX)
}

/// The first `max` chars of `s`.
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The model a session configuration `r` reports: the `currentValue` of its `configOptions`
/// entry of category `model` (or id `model`), else `models.currentModelId`.
pub fn reported_model(r: &Value) -> Option<String> {
    let options = r.get("configOptions").and_then(Value::as_array);
    let option = options.and_then(|a| {
        a.iter()
            .find(|o| str_at(o, "category") == "model")
            .or_else(|| a.iter().find(|o| str_at(o, "id") == "model"))
    });
    [
        option.and_then(|o| o.get("currentValue")),
        r.pointer("/models/currentModelId"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .map(str::trim)
    .find(|m| !m.is_empty())
    .map(str::to_string)
}

/// `message (data)` of a JSON-RPC error.
pub fn error_text(e: &Value) -> String {
    let msg = e
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("unknown error");
    match e.get("data") {
        Some(Value::String(d)) if !d.is_empty() => format!("{msg} ({d})"),
        Some(d) if !d.is_null() => format!("{msg} ({d})"),
        _ => msg.to_string(),
    }
}

/// Why a prompt error looks transient (rate limits, overload, timeouts, 5xx), if it does.
pub fn transient_reason(e: &Value) -> Option<String> {
    let text = error_text(e).to_lowercase();
    [
        "rate limit",
        "rate_limit",
        "overloaded",
        "timeout",
        "timed out",
        "temporarily",
        "503",
        "502",
        "529",
    ]
    .iter()
    .find(|k| text.contains(*k))
    .map(|k| format!("error mentions {k:?}"))
}

/// A JSON-RPC id as an attention id suffix.
fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The string at `key` of `v` ("" when absent).
pub fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn ext_of(key: &str, m: Map<String, Value>) -> Option<JsonMap> {
    if m.is_empty() {
        return None;
    }
    let mut out = JsonMap::new();
    out.insert(key.into(), Value::Object(m));
    Some(out)
}

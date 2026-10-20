//! Pure translation between Codex's app-server JSON-RPC protocol and twr events.
//!
//! [`Translator`] has no IO: the driver feeds it every stdout line ([`Translator::translate`])
//! and writes the stdin lines it returns ([`Translator::start`], [`Translator::send`],
//! [`Translator::respond`], [`Translator::interrupt`], and `replies` of each translation). The
//! fixture tests replay recorded sessions through it.
//!
//! Wire facts it relies on (Codex 0.155.1, see the recorded fixtures):
//! - JSON-RPC 2.0 over NDJSON. Server messages omit `"jsonrpc"`, and server-to-client requests
//!   use their own id space (starting at 0), which collides with ours. So a message is routed on
//!   the presence of `method`: with `id` it is a server request, without one a notification;
//!   a message with `id` and no `method` is a response to one of our requests.
//! - Handshake: `initialize` → response → `initialized` notification → `thread/start` (or
//!   `thread/resume`) → response carrying `thread.id` (the session id). Nothing is sent before
//!   the previous step's response.
//! - MCP allowlist (Codex 0.160.0): the `config.mcp_servers` of `thread/start` / `thread/resume`
//!   is merged into the servers of the user's and the project's Codex config, not put in their
//!   place, and a `-c` override merges the same way; a server is turned off only by name
//!   (`mcp_servers.<name>.enabled = false`). So when the open params name MCP servers (a
//!   strict set, such as the orchestrator's), the translator first asks `config/read{cwd}`
//!   (the effective config as seen from the thread's directory, project layers included) and
//!   turns off every server it lists but those, before opening the thread. A server the config
//!   defines under one of those names, or a `config/read` that fails, ends the session: twr
//!   cannot tell what it would run. Every `mcpServer/startupStatus/updated` naming a server
//!   outside the set ends it too ([`Translator::translate`]: `fatal`), in case a server came
//!   from somewhere `config/read` does not list. Plugins and apps, which bring servers of their
//!   own, are turned off by `codex::thread_config`.
//! - A turn is `turn/start{threadId, input}`; its response carries `turn.id`, and
//!   `turn/completed{turn{status, error, durationMs}}` ends it. Every notification of the turn
//!   carries `turnId`. Sends are queued here and started one at a time: `turn/start` during a
//!   running turn would steer that turn instead of starting a new one. A `turn/started` that
//!   comes before the `turn/start` answer (both orders occur) is held until the answer names
//!   twr's turn: another id is a turn Codex started on its own just before
//!   (`Translator::settle_unbound`). Everything such a turn reports (text, tool calls,
//!   questions, requests) and its token updates wait for the answer too (`Translator::hold`);
//!   a withdrawal follows the request it withdraws (`Translator::withdraw`).
//! - Approvals are server requests: `item/commandExecution/requestApproval`,
//!   `item/fileChange/requestApproval` (the diff is only on the preceding `item/started`, looked
//!   up by `itemId`), and `mcpServer/elicitation/request` with
//!   `_meta.codex_approval_kind: "mcp_tool_call"` for MCP tool calls. Answers:
//!   `{decision: "accept" | "decline"}` / `{action: "accept" | "decline", content, _meta}`.
//!   `item/permissions/requestApproval{itemId, cwd, reason, permissions}` asks for extra
//!   sandbox permissions; it is answered `{permissions: <the recognized part of the requested
//!   profile>, scope: "turn"}` to allow ([`permission_grant`]: a request with anything twr does
//!   not recognize, or too long to show in full, offers only `deny`) and `{permissions: {},
//!   scope: "turn"}` (nothing granted) to deny (schema `PermissionsRequestApprovalResponse`;
//!   not seen live). A file-change approval's `grantRoot` (approving also grants writes under
//!   it for the session) leads its summary.
//!   `serverRequest/resolved{requestId}` follows every answer; for a request twr did not answer,
//!   the runtime withdrew it.
//! - Questions: `request_user_input_async` shows up as an `agentMessage` with `questions`
//!   (`delivery: "async"`) while the turn keeps running; the answer goes back as
//!   `turn/steer{threadId, input, expectedTurnId}` (verified live). The synchronous
//!   `item/tool/requestUserInput` server request (EXPERIMENTAL in the schema; not seen live) is
//!   answered with `{answers: {<question id>: {answers: [text]}}}`.
//! - Open items are immutable (see [`crate::attn`]): a server request reusing the id of a
//!   pending one (or an `agentMessage` item asking again) replaces it; its open items are
//!   withdrawn (`attention.resolved{by: timeout, answer: null}`, `ext.codex.superseded`) and the
//!   new request gets new attention ids. A reply names only the JSON-RPC id, so an answer to a
//!   withdrawn item is never sent: it would answer the replacement.
//! - Not signed in (Codex 0.159.2 with an empty `CODEX_HOME`, an invalid API key or expired
//!   ChatGPT tokens): `turn/start` succeeds, `error` notifications (`willRetry`) follow, and
//!   `turn/completed` has `status: failed` with `error.codexErrorInfo`
//!   `{httpConnectionFailed: {httpStatusCode: 401}}` (or `"other"` with "workspace routing
//!   discovery unauthorized (401)"). Such a turn ([`auth_failure`]) is never transient and its
//!   `ext.twr.error` says how to sign in ([`AUTH_HINT`]).
//! - Every failed turn (`turn/completed` `status: failed`, or a refused `turn/start`) gets
//!   `ext.twr.error` for the hub's error item ([`error_text`]): `turn.error.message` (an API
//!   error body quoted as JSON is reduced to its message), else a readable `codexErrorInfo`,
//!   else the turn's last `error` notification (`{error, willRetry}`).
//! - Usage ([`twr_core::usage`], Codex 0.160.0): `account/rateLimits/read` (no turn) answers a
//!   `rateLimits` snapshot whose `primary` and `secondary` windows carry `usedPercent`,
//!   `windowDurationMins` and `resetsAt` (epoch seconds); which is the weekly one depends on
//!   the plan, so windows are named by their length ([`UsageWindow::of_minutes`]). It is asked
//!   right after `initialized` and again at a `turn/completed` once the last report is
//!   [`twr_core::usage::REFRESH_AFTER`] old, with ids of the monitor space (the turn RPC ids
//!   stay those of the recordings); an error answer stops the reads. The
//!   `account/rateLimits/updated` notification (sparse: a window it leaves null is unknown,
//!   not gone) is the passive report. Both become `runtime.limits`.
//! - Tokens: `thread/tokenUsage/updated` after every model call; `total` is cumulative for the
//!   thread (a resumed thread first replays its old total), so a turn's usage is the growth of
//!   `total` over the turn. There is no USD cost.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use twr_core::adapter::Answer;
use twr_core::usage::UsageWindow;

use crate::claude_code::translate::{error_line, mark_visible, render_command, FULL_INPUT_KEY};
use twr_core::event::{
    set_twr_ext, AgentText, AgentThinking, AgentToolCall, AgentToolResult, AttentionRaised,
    AttentionReason, AttentionResolved, EventBody, JsonMap, ResolvedBy, SessionPayload, TurnEnded,
    TurnFrom, TurnResult, TurnStarted, Usage, RUNTIME_TURN_KEY, SESSION_SCOPE_KEY,
};

/// Key of this runtime's extras (`data.ext.codex`).
pub const EXT: &str = "codex";

/// Maximum length (chars) of a tool call's `input_summary`.
pub const INPUT_SUMMARY_MAX: usize = 160;

/// Maximum length (chars) of a tool result's `output_summary`.
pub const OUTPUT_SUMMARY_MAX: usize = 200;

/// Files named in a file-change summary before it says "and N more".
pub const SUMMARY_FILES_MAX: usize = 4;

/// Steer text sent when the user declines an async question.
pub const DEFAULT_DECLINE_MESSAGE: &str = "The user declined to answer via twr";

/// Substrings (matched lowercase) that make a command risky whatever its parsed actions say.
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

/// `ext.twr.error` of a turn whose `turn/completed` line was too long to read.
pub const OVERSIZED_ERROR: &str = "oversized runtime message";

/// Codex turn ids remembered (turn stamping of late notifications).
pub const TURN_IDS_KEPT: usize = 64;

/// Attention ids remembered for uniqueness.
pub const ATTN_IDS_KEPT: usize = 4096;

/// JSON-RPC error code for server requests twr does not handle.
const METHOD_NOT_FOUND: i64 = -32601;

/// `codexErrorInfo` values of a failed turn that a retry may fix.
const TRANSIENT_ERRORS: &[&str] = &[
    "rateLimitExceeded",
    "serverOverloaded",
    "internalServerError",
    "httpConnectionFailed",
    "responseStreamConnectionFailed",
    "responseStreamDisconnected",
    "responseTooManyFailedAttempts",
];

/// Static facts about the process a translator serves.
#[derive(Debug, Clone, Default)]
pub struct TranslatorOptions {
    /// Emit `agent.thinking` for reasoning items (and ask for reasoning summaries).
    pub thinking: bool,
    /// Runtime process id (`ext.twr.pid` on `thread.started`).
    pub pid: Option<u32>,
    /// Program the process was started as (`ext.twr.exe`), when it is not a script.
    pub exe: Option<String>,
    /// `clientInfo.version` sent in `initialize`.
    pub client_version: String,
}

/// How the thread is opened once the handshake is done.
#[derive(Debug, Clone, PartialEq)]
pub struct Open {
    /// `thread/start` or `thread/resume`.
    pub method: &'static str,
    /// Its params (see `codex::thread_params`).
    pub params: Value,
    /// Why the Codex policy is not an exact match of the twr mode (`ext.codex.mode_note` on
    /// `thread.started`), see `codex::mode_note`.
    pub mode_note: Option<String>,
}

impl Open {
    /// Whether this resumes an existing thread.
    pub fn resumed(&self) -> bool {
        self.method == "thread/resume"
    }
}

/// One normalized event produced from a stdout line.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// Kind + payload.
    pub body: EventBody,
    /// Turn it belongs to (None for `thread.started`, or outside any turn).
    pub turn: Option<u64>,
    /// The raw stdout line, kept for non-delta events.
    pub raw: Option<String>,
}

/// Result of translating one stdout line (or handling a command).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Translated {
    /// Events to emit, in order.
    pub events: Vec<Emitted>,
    /// Lines to write to stdin right away, in order.
    pub replies: Vec<String>,
    /// The session cannot go on (the handshake or opening the thread failed): stop the
    /// process. The text says why.
    pub fatal: Option<String>,
}

/// Our requests waiting for a response.
#[derive(Debug, Clone)]
enum Req {
    Initialize,
    /// `config/read`, before opening a thread with an MCP allowlist.
    ConfigRead,
    Open,
    TurnStart {
        turn: u64,
    },
    Interrupt,
    Steer,
    Terminals,
    ChildRead {
        thread: String,
    },
    ChildTerminals {
        thread: String,
    },
    RateLimits,
}

/// Cumulative token counters (`TokenUsageBreakdown`).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tokens {
    input: u64,
    cached: u64,
    output: u64,
    reasoning: u64,
    total: u64,
}

impl Tokens {
    fn of(v: &Value) -> Tokens {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        Tokens {
            input: n("inputTokens"),
            cached: n("cachedInputTokens"),
            output: n("outputTokens"),
            reasoning: n("reasoningOutputTokens"),
            total: n("totalTokens"),
        }
    }

    fn minus(self, base: Tokens) -> Tokens {
        Tokens {
            input: self.input.saturating_sub(base.input),
            cached: self.cached.saturating_sub(base.cached),
            output: self.output.saturating_sub(base.output),
            reasoning: self.reasoning.saturating_sub(base.reasoning),
            total: self.total.saturating_sub(base.total),
        }
    }
}

/// The turn Codex is running (or about to run) for twr.
#[derive(Debug, Clone)]
struct Current {
    /// The twr turn; `None` for a turn Codex started on its own (`from: runtime`), which the
    /// hub numbers.
    turn: Option<u64>,
    /// Codex's turn id, once the `turn/start` answer (for Codex's own turn, `turn/started`)
    /// named it.
    codex_id: Option<String>,
    /// Thread token total when the turn started.
    base: Tokens,
    /// A token update of this turn arrived (earlier ones replay the thread's history).
    usage_seen: bool,
    /// Stop asked for an interrupt before the turn id was known.
    interrupt_wanted: bool,
    interrupt_sent: bool,
    /// The last `error` notification of the turn (`params.error`) and its `willRetry`: the
    /// cause of a failed turn whose `turn/completed` names none.
    last_error: Option<(Value, bool)>,
}

/// A turn Codex started while twr's `turn/start` had no answer yet: whose it is is known once
/// the answer names twr's turn ([`Translator::settle_unbound`]).
#[derive(Debug)]
struct Unbound {
    id: String,
    /// Thread token total when it started.
    base: Tokens,
    /// A token update of this turn arrived.
    usage_seen: bool,
    /// The events of its requests (raised, withdrawn), given a turn once it is known whose
    /// turn it is.
    events: Vec<Emitted>,
    /// Its last `error` notification and its `willRetry` (see [`Current::last_error`]).
    last_error: Option<(Value, bool)>,
    /// Its `turn/completed` (params and line) and the thread token total then, if that came
    /// too.
    completed: Option<(Value, String, Tokens)>,
}

/// A turn Codex started on its own that runs beside twr's (`Translator::settle_unbound`): what
/// its end needs, kept apart from the running turn's.
#[derive(Debug)]
struct OwnTurn {
    /// Thread token total when it started.
    base: Tokens,
    /// Its last `error` notification and its `willRetry` (see [`Current::last_error`]).
    last_error: Option<(Value, bool)>,
}

/// What an open attention item answers.
#[derive(Debug, Clone)]
enum Waiting {
    /// `item/commandExecution/requestApproval` / `item/fileChange/requestApproval`; `allow`:
    /// the item offers `allow` (else only `deny`).
    Decision { allow: bool },
    /// `mcpServer/elicitation/request` asking to run an MCP tool.
    Elicitation,
    /// Any other `mcpServer/elicitation/request` (a form to fill or a URL to open): twr has
    /// no form UI, so the user may only decline or cancel it.
    FormElicitation,
    /// `item/permissions/requestApproval`: the recognized part of the requested profile,
    /// granted on `allow`; None when the request may only be denied.
    Permissions { granted: Option<Value> },
    /// `item/tool/requestUserInput`: question ids, in order.
    UserInput { ids: Vec<String> },
    /// `agentMessage.questions`: answered with `turn/steer` on this Codex turn.
    Steer {
        turn_id: String,
        /// The `agentMessage` item that asked.
        item_id: String,
    },
}

/// A request (or async question set) with one attention item per question.
#[derive(Debug, Clone)]
struct Pending {
    /// Server request id (None for async questions).
    request_id: Option<Value>,
    /// Codex turn it belongs to.
    turn_id: Option<String>,
    waiting: Waiting,
    /// Attention id of each item.
    attn_ids: Vec<String>,
    /// Question texts (for multi-question steer answers).
    questions: Vec<String>,
    /// Option labels per item (a label is a valid answer).
    labels: Vec<Vec<String>>,
    answers: Vec<Option<String>>,
}

/// Stateful app-server ↔ twr translator for one Codex process.
#[derive(Debug)]
pub struct Translator {
    tasks: crate::background::Tasks,
    terminal_api: bool,
    check_after_turn: bool,
    terminal_pages: HashMap<String, String>,
    terminal_cursors: HashSet<String>,

    opts: TranslatorOptions,
    open: Open,
    /// The MCP servers the thread may have, when its open params name a set
    /// (`config.mcp_servers`): every other server of the user's Codex config is turned off
    /// before the thread opens (module docs, "MCP allowlist").
    mcp_allow: Option<Vec<String>>,
    next_id: u64,
    next_monitor_id: u64,
    requests: HashMap<u64, Req>,
    thread_id: Option<String>,
    ready: bool,
    queued: VecDeque<(String, u64)>,
    current: Option<Current>,
    /// Codex turn id → twr turn (`None`: Codex's own, see [`Current::turn`]), for the last
    /// [`TURN_IDS_KEPT`] turns this process ran.
    turn_ids: Recent<Option<u64>>,
    /// Turns Codex started while twr's `turn/start` had no answer yet.
    unbound: Vec<Unbound>,
    /// Codex's own turns that run beside twr's, by id.
    own_turns: HashMap<String, OwnTurn>,
    /// Withdrawals routed by the turn of the request they withdraw rather than by the line
    /// they come in ([`Translator::withdraw`]); they go before the line's own events.
    routed: Vec<Emitted>,
    /// Latest thread token total seen.
    tokens: Tokens,
    /// Item id → summary of its `item/started` (tool calls already reported).
    items: HashMap<String, String>,
    pending: Vec<Pending>,
    /// The last [`ATTN_IDS_KEPT`] attention ids raised (ids still pending are never reused
    /// either, see [`Translator::attn_id`]).
    used_attn_ids: Recent<()>,
    /// The thread's working directory (file-change summaries name files relative to it).
    cwd: Option<String>,
    /// When to read the plan's usage again (`account/rateLimits/read`).
    usage: crate::usage::Reads,
    /// The clock usage reads are timed by.
    clock: fn() -> DateTime<Utc>,
}

impl Translator {
    /// A translator for a fresh process that will open the thread with `open`.
    pub fn new(opts: TranslatorOptions, open: Open) -> Self {
        let cwd = opt_str(&open.params, "cwd");
        let mcp_allow = open
            .params
            .pointer("/config/mcp_servers")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect());
        Translator {
            mcp_allow,
            tasks: crate::background::Tasks::default(),
            terminal_api: true,
            check_after_turn: false,
            terminal_pages: HashMap::new(),
            terminal_cursors: HashSet::new(),
            cwd,
            opts,
            open,
            next_id: 1,
            next_monitor_id: 1 << 50,
            requests: HashMap::new(),
            thread_id: None,
            ready: false,
            queued: VecDeque::new(),
            current: None,
            turn_ids: Recent::new(TURN_IDS_KEPT),
            unbound: Vec::new(),
            own_turns: HashMap::new(),
            routed: Vec::new(),
            tokens: Tokens::default(),
            items: HashMap::new(),
            pending: Vec::new(),
            used_attn_ids: Recent::new(ATTN_IDS_KEPT),
            usage: crate::usage::Reads::default(),
            clock: Utc::now,
        }
    }

    /// Times usage reads by `clock` instead of the system clock (tests).
    pub fn set_clock(&mut self, clock: fn() -> DateTime<Utc>) {
        self.clock = clock;
    }

    /// Asks for the plan's usage (`account/rateLimits/read`) when a read is due.
    fn read_usage(&mut self, out: &mut Translated) {
        let now = (self.clock)();
        if !self.usage.due(now) {
            return;
        }
        self.usage.ask(now);
        let line = self.request(Req::RateLimits, "account/rateLimits/read", Value::Null);
        out.replies.push(line);
    }

    /// `runtime.limits` for a `rateLimits` snapshot, if it names a window: a read's answer
    /// (`read`) or the runtime's own report (`event`), which refreshes the data only when it
    /// covers every window the last read had.
    fn usage_report(&mut self, snapshot: &Value, source: &str, out: &mut Translated) {
        let windows = rate_limit_windows(snapshot);
        if source == "read" {
            self.usage.answered(&windows);
        } else {
            self.usage.heard((self.clock)(), &windows);
        }
        if windows.is_empty() {
            return;
        }
        out.events.push(Emitted {
            body: crate::usage::limits(windows, source, opt_str(snapshot, "planType")),
            turn: None,
            raw: None,
        });
    }

    /// The Codex thread id (the session id), once the thread is open.
    pub fn thread_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }

    /// Whether a turn is running or queued.
    pub fn busy(&self) -> bool {
        self.current.is_some() || !self.queued.is_empty()
    }

    /// Attention ids waiting for an answer.
    pub fn pending_attention(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .pending
            .iter()
            .flat_map(|p| {
                p.attn_ids
                    .iter()
                    .zip(&p.answers)
                    .filter(|(_, a)| a.is_none())
                    .map(|(id, _)| id.clone())
            })
            .collect();
        ids.sort();
        ids
    }

    fn request(&mut self, req: Req, method: &str, params: Value) -> String {
        let id = if matches!(
            &req,
            Req::Terminals
                | Req::ChildRead { .. }
                | Req::ChildTerminals { .. }
                | Req::RateLimits
                | Req::ConfigRead
        ) {
            let id = self.next_monitor_id;
            self.next_monitor_id += 1;
            id
        } else {
            let id = self.next_id;
            self.next_id += 1;
            id
        };
        self.requests.insert(id, req);
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
    }

    /// The first stdin line: `initialize`.
    pub fn start(&mut self) -> String {
        let params = json!({
            "clientInfo": {"name": "twr", "title": null, "version": self.opts.client_version},
            // Background terminal inventory is an experimental method. Unknown-method
            // replies still fall back to item lifecycle tracking on older app servers.
            "capabilities": {"experimentalApi": true, "requestAttestation": false},
        });
        self.request(Req::Initialize, "initialize", params)
    }

    /// Queues turn `turn` with `text`. Returns the `turn/start` line when the thread is open
    /// and no other turn runs; otherwise the turn starts when the running one ends.
    pub fn send(&mut self, text: &str, turn: u64) -> Vec<String> {
        self.queued.push_back((text.to_string(), turn));
        self.dispatch().into_iter().collect()
    }

    fn dispatch(&mut self) -> Option<String> {
        if !self.ready || self.current.is_some() {
            return None;
        }
        let thread_id = self.thread_id.clone()?;
        let (text, turn) = self.queued.pop_front()?;
        self.current = Some(Current {
            turn: Some(turn),
            codex_id: None,
            base: self.tokens,
            usage_seen: false,
            interrupt_wanted: false,
            interrupt_sent: false,
            last_error: None,
        });
        let mut params = json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": text, "text_elements": []}],
        });
        if self.opts.thinking {
            params["summary"] = json!("auto");
        }
        Some(self.request(Req::TurnStart { turn }, "turn/start", params))
    }

    /// The line interrupting the running turn (`turn/interrupt`), if one runs. Queued turns
    /// are dropped (the caller is stopping). When the turn id is not known yet, the interrupt
    /// goes out as soon as it is.
    pub fn interrupt(&mut self) -> Option<String> {
        self.queued.clear();
        self.interrupt_current()
    }

    /// Interrupt one turn while retaining the session. A prompt waiting for startup
    /// is cancelled explicitly, so the hub can dispatch its next turn without hanging. Each
    /// call asks again, even for a turn asked before: the hub interrupts a turn again when the
    /// runtime goes on with it (a turn whose message the user took back).
    pub fn interrupt_turn(&mut self) -> Translated {
        let mut out = Translated::default();
        if let Some(cur) = self.current.as_mut() {
            cur.interrupt_sent = false;
            out.replies.extend(self.interrupt_current());
        } else if let Some((_, turn)) = self.queued.pop_front() {
            self.push(
                &mut out,
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Interrupted,
                    ..Default::default()
                }),
                Some(turn),
                None,
            );
        }
        out
    }

    fn interrupt_current(&mut self) -> Option<String> {
        let thread_id = self.thread_id.clone()?;
        let cur = self.current.as_mut()?;
        if cur.interrupt_sent {
            return None;
        }
        let Some(turn_id) = cur.codex_id.clone() else {
            cur.interrupt_wanted = true;
            return None;
        };
        cur.interrupt_sent = true;
        Some(self.request(
            Req::Interrupt,
            "turn/interrupt",
            json!({"threadId": thread_id, "turnId": turn_id}),
        ))
    }

    /// Handles the user's answer to attention `attn_id`. Returns None if it is not pending
    /// (unknown or already answered); otherwise `replies` holds the stdin line to write, if
    /// any, and `events` the withdrawn sibling questions.
    ///
    /// Approvals: `allow` accepts; `cancel` cancels (Codex aborts the turn); anything else
    /// declines. Permission requests: `allow` grants the requested profile for the turn,
    /// anything else grants nothing. Questions: `answer.extra.answers[<question>]`, else the decision (an option
    /// label or free text), else `answer.message` for a bare `allow`; `deny` / `decline`
    /// (when not an option label) declines. Nothing is written until every question of the
    /// request has an answer; a decline answers the whole request at once.
    pub fn respond(&mut self, attn_id: &str, answer: &Answer) -> Option<Translated> {
        let pi = self.pending.iter().position(|p| {
            p.attn_ids
                .iter()
                .zip(&p.answers)
                .any(|(id, a)| id == attn_id && a.is_none())
        })?;
        let i = self.pending[pi]
            .attn_ids
            .iter()
            .position(|id| id == attn_id)?;
        let mut out = Translated::default();
        let p = &mut self.pending[pi];
        let reply = match p.waiting.clone() {
            Waiting::Decision { allow } => {
                let decision = match answer.decision.as_str() {
                    // A deny-only item (a grant twr could not show) is never accepted.
                    "allow" if allow => "accept",
                    // Not among the offered options (allow / deny); only a client sending the
                    // decision "cancel" verbatim gets here. Kept because it is the user's
                    // explicit choice and the recorded `cancel` fixture covers it.
                    "cancel" => "cancel",
                    _ => "decline",
                };
                Some(Reply::Result(json!({"decision": decision})))
            }
            Waiting::Permissions { granted } => Some(Reply::Result(match granted {
                Some(profile) if answer.is_allow() => {
                    json!({"permissions": profile, "scope": "turn"})
                }
                // The response has no decline field: granting nothing is the decline.
                _ => json!({"permissions": {}, "scope": "turn"}),
            })),
            Waiting::Elicitation => Some(Reply::Result(if answer.is_allow() {
                json!({"action": "accept", "content": {}, "_meta": null})
            } else {
                json!({"action": "decline", "content": null, "_meta": null})
            })),
            // Never `accept`: twr has no form content to send. `cancel` cancels, any other
            // answer declines.
            Waiting::FormElicitation => Some(Reply::Result(json!({
                "action": if answer.decision.trim() == "cancel" { "cancel" } else { "decline" },
                "content": null,
                "_meta": null,
            }))),
            waiting @ (Waiting::UserInput { .. } | Waiting::Steer { .. }) => {
                match question_answer(&p.questions[i], &p.labels[i], answer) {
                    None => {
                        let open: Vec<String> = p
                            .attn_ids
                            .iter()
                            .zip(&p.answers)
                            .enumerate()
                            .filter(|(j, (_, a))| *j != i && a.is_none())
                            .map(|(_, (id, _))| id.clone())
                            .collect();
                        for id in open {
                            out.events.push(Emitted {
                                body: withdrawal(id, "sibling_declined"),
                                turn: None,
                                raw: None,
                            });
                        }
                        let message = answer
                            .message
                            .clone()
                            .filter(|m| !m.trim().is_empty())
                            .unwrap_or_else(|| DEFAULT_DECLINE_MESSAGE.into());
                        Some(match waiting {
                            Waiting::UserInput { .. } => Reply::Error(message),
                            _ => Reply::Steer(message),
                        })
                    }
                    Some(text) => {
                        p.answers[i] = Some(text);
                        if p.answers.iter().all(Option::is_some) {
                            let answers: Vec<String> = p
                                .answers
                                .iter()
                                .map(|a| a.clone().unwrap_or_default())
                                .collect();
                            Some(match waiting {
                                Waiting::UserInput { ids } => {
                                    let map: Map<String, Value> = ids
                                        .iter()
                                        .zip(&answers)
                                        .map(|(id, a)| (id.clone(), json!({"answers": [a]})))
                                        .collect();
                                    Reply::Result(json!({"answers": map}))
                                }
                                _ => Reply::Steer(steer_text(&p.questions, &answers)),
                            })
                        } else {
                            None
                        }
                    }
                }
            }
        };
        let Some(reply) = reply else {
            // Waiting for the other questions of the request.
            return Some(out);
        };
        let p = self.pending.remove(pi);
        // Events of the running turn (withdrawn siblings) carry its number.
        let turn = self.current.as_ref().and_then(|c| c.turn);
        for e in &mut out.events {
            e.turn = turn;
        }
        let line = match reply {
            Reply::Result(result) => {
                json!({"jsonrpc": "2.0", "id": p.request_id, "result": result}).to_string()
            }
            Reply::Error(message) => json!({
                "jsonrpc": "2.0",
                "id": p.request_id,
                "error": {"code": -32000, "message": message},
            })
            .to_string(),
            Reply::Steer(text) => {
                let Waiting::Steer { turn_id, .. } = &p.waiting else {
                    unreachable!("only async questions steer")
                };
                let params = json!({
                    "threadId": self.thread_id.clone().unwrap_or_default(),
                    "input": [{"type": "text", "text": text, "text_elements": []}],
                    "expectedTurnId": turn_id,
                });
                self.request(Req::Steer, "turn/steer", params)
            }
        };
        out.replies.push(line);
        Some(out)
    }

    /// A stdout line longer than the reader's cap was dropped; `head` is its start and `len`
    /// its length. A `turn/completed` still ends the running turn (`turn.ended{error}` with
    /// `ext.twr.error = "oversized runtime message"`); anything else is only logged.
    pub fn oversized(&mut self, head: &str, len: usize) -> Translated {
        let mut out = Translated::default();
        if !crate::process::head_has(head, "method", "turn/completed") || self.current.is_none() {
            return out;
        }
        let codex_id = self.current.as_ref().and_then(|c| c.codex_id.clone());
        let p = json!({"turn": {
            "id": codex_id,
            "status": "failed",
            "error": {"message": OVERSIZED_ERROR},
        }});
        self.on_turn_completed(&p, "", &mut out);
        for e in &mut out.events {
            e.raw = None;
            if let EventBody::TurnEnded(t) = &mut e.body {
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
                tracing::warn!(line = %one_line(line, 200), "codex: ignoring non-JSON stdout line");
                return out;
            }
        };
        match (v.get("method").and_then(Value::as_str), v.get("id")) {
            (Some(method), Some(id)) => {
                self.on_server_request(method, id, &v, line, &mut out);
                self.hold(&v["params"], &mut out);
            }
            (Some(method), None) => {
                self.on_notification(method, &v["params"], line, &mut out);
                self.hold(&v["params"], &mut out);
            }
            (None, Some(_)) => self.on_response(&v, line, &mut out),
            (None, None) => {}
        }
        if !self.routed.is_empty() {
            let line_events = std::mem::replace(&mut out.events, std::mem::take(&mut self.routed));
            out.events.extend(line_events);
        }
        out
    }

    /// The events of a line about a turn whose owner is not known yet (`turnId` held,
    /// [`Translator::settle_unbound`]) wait to be given their turn: text, tool calls, requests
    /// and questions alike. Task updates do not: they belong to the session.
    fn hold(&mut self, params: &Value, out: &mut Translated) {
        let Some(u) =
            opt_str(params, "turnId").and_then(|t| self.unbound.iter_mut().find(|u| u.id == t))
        else {
            return;
        };
        let (session, held): (Vec<Emitted>, Vec<Emitted>) = std::mem::take(&mut out.events)
            .into_iter()
            .partition(|e| matches!(e.body, EventBody::TaskUpdated(_)));
        out.events = session;
        u.events.extend(held);
    }

    /// Withdraws item `attn_id` of a request of Codex turn `turn_id` (`why`: the withdrawal's
    /// `ext.codex.reason`), whatever line it comes in. While that turn's owner is not known yet,
    /// the withdrawal waits with its other events, and an item still held is never shown.
    fn withdraw(&mut self, turn_id: Option<&str>, attn_id: String, why: &str, line: &str) {
        if let Some(u) = turn_id.and_then(|t| self.unbound.iter_mut().find(|u| u.id == t)) {
            let raised = u.events.iter().position(
                |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.attn_id == attn_id),
            );
            match raised {
                Some(i) => {
                    u.events.remove(i);
                }
                None => u.events.push(Emitted {
                    body: withdrawal(attn_id, why),
                    turn: None,
                    raw: Some(line.to_string()),
                }),
            }
            return;
        }
        let turn = self.turn_of(turn_id);
        self.routed.push(Emitted {
            body: withdrawal(attn_id, why),
            turn,
            raw: Some(line.to_string()),
        });
    }

    /// Reconcile background terminals and the known running child agents. One request per
    /// inventory may be outstanding; older servers reject the terminal API once, then use
    /// item lifecycle events alone. Failed reads never mean the tasks finished.
    pub fn poll_background(&mut self) -> Vec<String> {
        if !self.ready {
            return Vec::new();
        }
        let mut requests = Vec::new();
        if self.terminal_api && !self.requests.values().any(|r| matches!(r, Req::Terminals)) {
            self.terminal_pages.clear();
            self.terminal_cursors.clear();
            requests.push(self.request(
                Req::Terminals,
                "thread/backgroundTerminals/list",
                json!({"threadId": self.thread_id, "limit": 100}),
            ));
        }
        let children: Vec<String> = self
            .tasks
            .running
            .keys()
            .filter_map(|id| id.strip_prefix("agent:").map(str::to_string))
            .collect();
        for child in children {
            if !self
                .requests
                .values()
                .any(|r| matches!(r, Req::ChildRead { thread } if thread == &child))
            {
                requests.push(self.request(
                    Req::ChildRead {
                        thread: child.clone(),
                    },
                    "thread/read",
                    json!({"threadId": child, "includeTurns": false}),
                ));
            }
        }
        requests
    }

    fn task(
        &mut self,
        id: &str,
        running: bool,
        description: &str,
        line: &str,
        out: &mut Translated,
    ) {
        if let Some(task) = self.tasks.update(id, running, description) {
            self.push(out, EventBody::TaskUpdated(task), None, Some(line));
        }
    }

    fn task_progress(&mut self, id: &str, line: &str, out: &mut Translated) {
        if self.tasks.running.contains_key(id) {
            if let Some(task) = self.tasks.progress(id, "") {
                self.push(out, EventBody::TaskUpdated(task), None, Some(line));
            }
        }
    }

    fn check_child(&mut self, thread: &str, line: &str, out: &mut Translated) {
        if self.terminal_api {
            if !self
                .requests
                .values()
                .any(|r| matches!(r, Req::ChildTerminals { thread: id } if id == thread))
            {
                out.replies.push(self.request(
                    Req::ChildTerminals {
                        thread: thread.to_string(),
                    },
                    "thread/backgroundTerminals/list",
                    json!({"threadId": thread, "limit": 1}),
                ));
            }
        } else {
            self.task(&format!("agent:{thread}"), false, "", line, out);
        }
    }

    fn activate_child(
        &mut self,
        thread: &str,
        description: &str,
        line: &str,
        out: &mut Translated,
    ) {
        // An idle snapshot requested before this activity cannot retire the new child turn.
        self.requests.retain(|_, req| {
            !matches!(req,
            Req::ChildRead { thread: id } | Req::ChildTerminals { thread: id } if id.as_str() == thread)
        });
        let id = format!("agent:{thread}");
        self.tasks.reopen(&id);
        self.task(&id, true, description, line, out);
    }

    fn item_tasks(&mut self, item: &Value, completed: bool, line: &str, out: &mut Translated) {
        match str_at(item, "type") {
            "commandExecution" if !str_at(item, "id").is_empty() => {
                let running = !completed
                    || (item.get("processId").is_some_and(|p| !p.is_null())
                        && item.get("exitCode").is_none_or(Value::is_null));
                self.task(
                    &format!("shell:{}", str_at(item, "id")),
                    running,
                    &command_summary(item),
                    line,
                    out,
                );
            }
            "collabAgentToolCall" => {
                if let Some(states) = item.get("agentsStates").and_then(Value::as_object) {
                    for (id, state) in states {
                        let status = str_at(state, "status");
                        let task_id = format!("agent:{id}");
                        let running = matches!(status, "running" | "pendingInit");
                        if running
                            && matches!(
                                str_at(item, "tool"),
                                "spawnAgent" | "resumeAgent" | "sendInput"
                            )
                        {
                            self.activate_child(id, "subagent", line, out);
                            continue;
                        }
                        if running
                            || matches!(
                                status,
                                "interrupted" | "completed" | "errored" | "shutdown" | "notFound"
                            )
                        {
                            if !running
                                && !matches!(status, "shutdown" | "notFound")
                                && self.tasks.running.contains_key(&task_id)
                            {
                                self.check_child(id, line, out);
                            } else {
                                self.task(&task_id, running, "subagent", line, out);
                            }
                        }
                    }
                }
            }
            "subAgentActivity" => {
                let id = str_at(item, "agentThreadId");
                if !id.is_empty() {
                    let kind = str_at(item, "kind");
                    let task_id = format!("agent:{id}");
                    let running = matches!(kind, "started" | "interacted");
                    if running {
                        self.activate_child(id, str_at(item, "agentPath"), line, out);
                        return;
                    }
                    if running || matches!(kind, "interrupted" | "completed") {
                        if !running && self.tasks.running.contains_key(&task_id) {
                            self.check_child(id, line, out);
                        } else {
                            self.task(&task_id, running, str_at(item, "agentPath"), line, out);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // --- stamping ----------------------------------------------------------------------------

    /// The twr turn a notification about Codex turn `turn_id` belongs to: the turn that id was
    /// bound to, else the running one.
    fn turn_of(&self, turn_id: Option<&str>) -> Option<u64> {
        if let Some(t) = turn_id.and_then(|id| self.turn_ids.get(id)) {
            return *t;
        }
        self.current.as_ref().and_then(|c| c.turn)
    }

    /// `turn/started` named the id of the running turn, Codex's own (twr's is named by the
    /// `turn/start` answer, `settle_unbound`).
    fn bind(&mut self, turn_id: &str) {
        if self.turn_ids.contains_key(turn_id) {
            return;
        }
        if let Some(cur) = self.current.as_mut().filter(|c| c.codex_id.is_none()) {
            cur.codex_id = Some(turn_id.to_string());
            self.turn_ids.insert_new(turn_id.to_string(), cur.turn);
        }
    }

    fn push(&self, out: &mut Translated, body: EventBody, turn: Option<u64>, raw: Option<&str>) {
        out.events.push(Emitted {
            body,
            turn,
            raw: raw.map(str::to_string),
        });
    }

    /// A fresh attention id based on `base`: not among the last [`ATTN_IDS_KEPT`] ids raised
    /// nor any still pending.
    fn attn_id(&mut self, base: String) -> String {
        let mut id = base.clone();
        let mut n = 2;
        while self.pending.iter().any(|p| p.attn_ids.contains(&id))
            || !self.used_attn_ids.insert_new(id.clone(), ())
        {
            id = format!("{base}#{n}");
            n += 1;
        }
        id
    }

    /// Withdraws every pending request `replaced` matches (the runtime asked again under what
    /// identifies it on the wire): its open items resolve `by: timeout` with
    /// `ext.codex.superseded`, so no answer to them is ever sent (see [`crate::attn`]). The new
    /// request is raised under ids of its own ([`Translator::attn_id`]).
    fn supersede(&mut self, replaced: impl Fn(&Pending) -> bool, line: &str) {
        let (old, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|p| replaced(p));
        self.pending = keep;
        for p in old {
            tracing::info!(request = ?p.request_id, "codex: runtime replaced a pending request; withdrawing its items");
            for (id, a) in p.attn_ids.into_iter().zip(p.answers) {
                if a.is_none() {
                    self.withdraw(p.turn_id.as_deref(), id, "superseded", line);
                }
            }
        }
    }

    // --- responses ---------------------------------------------------------------------------

    fn on_response(&mut self, v: &Value, line: &str, out: &mut Translated) {
        let Some(req) = v["id"].as_u64().and_then(|id| self.requests.remove(&id)) else {
            tracing::debug!(id = %v["id"], "codex: response to an unknown request");
            return;
        };
        let error = v.get("error").filter(|e| !e.is_null());
        let error_text = error.map(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| e.to_string())
        });
        match req {
            Req::Terminals => {
                if error.is_some() {
                    if error.and_then(|e| e.get("code")).and_then(Value::as_i64)
                        == Some(METHOD_NOT_FOUND)
                    {
                        self.terminal_api = false;
                    }
                    self.terminal_pages.clear();
                    self.terminal_cursors.clear();
                    return;
                }
                let Some(data) = v.pointer("/result/data").and_then(Value::as_array) else {
                    return;
                };
                for terminal in data {
                    if let Some(id) = opt_str(terminal, "itemId") {
                        self.terminal_pages.insert(
                            format!("shell:{id}"),
                            one_line(str_at(terminal, "command"), INPUT_SUMMARY_MAX),
                        );
                    }
                }
                if let Some(cursor) = v.pointer("/result/nextCursor").and_then(Value::as_str) {
                    if self.terminal_pages.len() < 4096
                        && self.terminal_cursors.insert(cursor.to_string())
                    {
                        let request = self.request(
                            Req::Terminals,
                            "thread/backgroundTerminals/list",
                            json!({"threadId": self.thread_id, "cursor": cursor, "limit": 100}),
                        );
                        out.replies.push(request);
                    }
                    return;
                }
                let live = std::mem::take(&mut self.terminal_pages);
                self.terminal_cursors.clear();
                if std::mem::take(&mut self.check_after_turn) {
                    // This snapshot was requested before the turn ended. It may omit a
                    // command that yielded meanwhile; only the fresh read may retire it.
                    out.replies.push(self.request(
                        Req::Terminals,
                        "thread/backgroundTerminals/list",
                        json!({"threadId": self.thread_id, "limit": 100}),
                    ));
                    return;
                }
                let finished: Vec<String> = self
                    .tasks
                    .running
                    .keys()
                    .filter(|id| {
                        self.current.is_none()
                            && id.starts_with("shell:")
                            && !live.contains_key(*id)
                    })
                    .cloned()
                    .collect();
                for id in finished {
                    self.task(&id, false, "", line, out);
                }
                for (id, description) in live {
                    self.task(&id, true, &description, line, out);
                }
            }
            Req::ChildTerminals { thread } => {
                if let Some(data) = v
                    .pointer("/result/data")
                    .and_then(Value::as_array)
                    .filter(|_| error.is_none())
                {
                    self.task(
                        &format!("agent:{thread}"),
                        !data.is_empty(),
                        "subagent background commands",
                        line,
                        out,
                    );
                } else if !self.terminal_api
                    || error.and_then(|e| e.get("code")).and_then(Value::as_i64)
                        == Some(METHOD_NOT_FOUND)
                {
                    self.task(&format!("agent:{thread}"), false, "", line, out);
                }
            }
            Req::ChildRead { thread } => {
                let status = v
                    .pointer("/result/thread/status/type")
                    .and_then(Value::as_str);
                if error.is_none()
                    && matches!(
                        status,
                        Some("active" | "idle" | "notLoaded" | "systemError")
                    )
                {
                    if status == Some("active") {
                        // Newer than any completion check still out: that one cannot retire
                        // the child.
                        self.requests.retain(|_, req| {
                            !matches!(req, Req::ChildTerminals { thread: id } if *id == thread)
                        });
                        self.task(&format!("agent:{thread}"), true, "subagent", line, out);
                    } else {
                        self.check_child(&thread, line, out);
                    }
                }
            }
            Req::Initialize => match error_text {
                Some(e) => {
                    out.fatal = Some(with_auth_hint(format!(
                        "codex app-server initialize failed: {e}"
                    )))
                }
                None => {
                    out.replies
                        .push(json!({"jsonrpc": "2.0", "method": "initialized"}).to_string());
                    if self.mcp_allow.is_some() {
                        let params = json!({"cwd": self.cwd});
                        let l = self.request(Req::ConfigRead, "config/read", params);
                        out.replies.push(l);
                    } else {
                        self.open_thread(out);
                    }
                    self.read_usage(out);
                }
            },
            Req::ConfigRead => match error_text {
                Some(e) => {
                    out.fatal = Some(format!(
                        "codex config/read failed, so the MCP servers of the Codex config \
                         cannot be turned off: {e}"
                    ))
                }
                None => match self.disable_inherited_mcp(&v["result"]) {
                    Ok(()) => self.open_thread(out),
                    Err(why) => out.fatal = Some(why),
                },
            },
            Req::RateLimits => match error_text {
                Some(e) => {
                    tracing::debug!(error = %e, "codex: account/rateLimits/read failed; no more usage reads");
                    self.usage.stop();
                }
                None => self.usage_report(&v["result"]["rateLimits"], "read", out),
            },
            Req::Open => match error_text {
                Some(e) => {
                    out.fatal = Some(with_auth_hint(format!(
                        "codex {} failed: {e}",
                        self.open.method
                    )))
                }
                None => self.on_open(&v["result"], line, out),
            },
            Req::TurnStart { turn } => match error_text {
                Some(e) => {
                    tracing::warn!(turn, error = %e, "codex: turn/start failed");
                    let mut codex = Map::new();
                    codex.insert("error".into(), json!({"message": e}));
                    codex.insert("transient".into(), json!(false));
                    let auth = auth_failure(&json!({"message": e}));
                    if auth {
                        codex.insert("auth_failed".into(), json!(true));
                    }
                    let mut ext = ext_of(codex);
                    let why = if auth {
                        format!("{}{AUTH_HINT}", error_line(&e))
                    } else {
                        error_line(&e)
                    };
                    ext.get_or_insert_with(JsonMap::new)
                        .insert("twr".into(), json!({ "error": why }));
                    let body = EventBody::TurnEnded(TurnEnded {
                        result: TurnResult::Error,
                        session_id: self.thread_id.clone(),
                        usage: None,
                        ext,
                        ..Default::default()
                    });
                    if self.current.as_ref().is_some_and(|c| c.turn == Some(turn)) {
                        self.current = None;
                    }
                    self.push(out, body, Some(turn), Some(line));
                    self.settle_unbound(None, line, out);
                    out.replies.extend(self.dispatch());
                }
                None => {
                    if let Some(id) = v.pointer("/result/turn/id").and_then(Value::as_str) {
                        self.turn_ids.insert_new(id.to_string(), Some(turn));
                        let mut late_interrupt = false;
                        if let Some(cur) = self.current.as_mut().filter(|c| c.turn == Some(turn)) {
                            if cur.codex_id.is_none() {
                                cur.codex_id = Some(id.to_string());
                            }
                            late_interrupt = cur.interrupt_wanted && !cur.interrupt_sent;
                        }
                        if late_interrupt {
                            out.replies.extend(self.interrupt_current());
                        }
                        self.settle_unbound(Some(id), line, out);
                    }
                }
            },
            Req::Interrupt => {
                if let Some(e) = error_text {
                    tracing::warn!(error = %e, "codex: turn/interrupt failed");
                    // Not interrupted: a later interrupt is sent again.
                    if let Some(cur) = self.current.as_mut() {
                        cur.interrupt_sent = false;
                    }
                }
            }
            Req::Steer => {
                if let Some(e) = error_text {
                    tracing::warn!(error = %e, "codex: turn/steer (question answer) failed");
                }
            }
        }
    }

    fn on_open(&mut self, r: &Value, line: &str, out: &mut Translated) {
        let thread_id = r
            .pointer("/thread/id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let Some(thread_id) = thread_id else {
            out.fatal = Some(format!("codex {} returned no thread id", self.open.method));
            return;
        };
        // What Codex says the thread runs with, not what twr asked for, decides (a user or
        // managed config can override a request): nothing runs in a wider policy.
        if let Err(why) = check_policy(&self.open.params, r) {
            tracing::error!(%why, "codex: refusing a thread that runs with more than twr asked for");
            out.fatal = Some(format!("codex {}: {why}", self.open.method));
            return;
        }
        self.thread_id = Some(thread_id.clone());
        self.ready = true;
        let mut twr = Map::new();
        if let Some(pid) = self.opts.pid {
            twr.insert("pid".into(), json!(pid));
        }
        if let Some(exe) = &self.opts.exe {
            twr.insert("exe".into(), json!(exe));
        }
        let mut codex = Map::new();
        for (from, to) in [
            ("model", "model"),
            ("modelProvider", "model_provider"),
            ("approvalPolicy", "approval_policy"),
            ("approvalsReviewer", "approvals_reviewer"),
            ("reasoningEffort", "reasoning_effort"),
        ] {
            if let Some(x) = r.get(from).filter(|x| !x.is_null()) {
                codex.insert(to.into(), x.clone());
            }
        }
        if let Some(cwd) = opt_str(r, "cwd") {
            self.cwd = Some(cwd);
        }
        if let Some(s) = r.pointer("/sandbox/type") {
            codex.insert("sandbox".into(), s.clone());
        }
        if let Some(n) = r.pointer("/sandbox/networkAccess").filter(|n| !n.is_null()) {
            codex.insert("sandbox_network".into(), n.clone());
        }
        if let Some(roots) = r
            .pointer("/sandbox/writableRoots")
            .filter(|w| w.as_array().is_some_and(|a| !a.is_empty()))
        {
            codex.insert("sandbox_writable_roots".into(), roots.clone());
        }
        if let Some(s) = r.pointer("/thread/cliVersion") {
            codex.insert("version".into(), s.clone());
        }
        codex.insert("resumed".into(), json!(self.open.resumed()));
        if let Some(note) = &self.open.mode_note {
            codex.insert("mode_note".into(), json!(note));
        }
        let mut ext = JsonMap::new();
        if !twr.is_empty() {
            ext.insert("twr".into(), Value::Object(twr));
        }
        ext.insert(EXT.into(), Value::Object(codex));
        let body = EventBody::ThreadStarted(SessionPayload {
            session_id: Some(thread_id),
            ext: Some(ext),
            ..Default::default()
        });
        self.push(out, body, None, Some(line));
        out.replies.extend(self.dispatch());
    }

    // --- server requests ---------------------------------------------------------------------

    fn on_server_request(
        &mut self,
        method: &str,
        id: &Value,
        v: &Value,
        line: &str,
        out: &mut Translated,
    ) {
        let p = &v["params"];
        let turn_id = opt_str(p, "turnId");
        let first = out.events.len();
        // Open items never change: a server request reusing a pending request's id replaced
        // it (a reply names only the id, so it would answer the replacement).
        self.supersede(|x| x.request_id.as_ref() == Some(id), line);
        self.raise_request(method, id, p, turn_id.clone(), line, out);
        // A child agent's request belongs to the session, not to the parent's turn: Codex keeps
        // waiting on it after that turn ends, so its item must outlive the turn.
        if opt_str(p, "threadId").is_some_and(|t| Some(&t) != self.thread_id.as_ref()) {
            for e in &mut out.events[first..] {
                if let EventBody::AttentionRaised(a) = &mut e.body {
                    set_twr_ext(&mut a.ext, SESSION_SCOPE_KEY, json!(true));
                    e.turn = None;
                }
            }
        }
    }

    /// The attention item(s) of server request `id` (`on_server_request`).
    fn raise_request(
        &mut self,
        method: &str,
        id: &Value,
        p: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        match method {
            "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
                let item_id = str_at(p, "itemId").to_string();
                let mut base = if item_id.is_empty() {
                    format!("codex-{}", uuid::Uuid::new_v4())
                } else {
                    item_id.clone()
                };
                if let Some(a) = opt_str(p, "approvalId") {
                    base = format!("{base}#{a}");
                }
                let attn_id = self.attn_id(base);
                let mut codex = Map::new();
                codex.insert("method".into(), json!(method));
                codex.insert("request_id".into(), id.clone());
                for (from, to) in [
                    ("reason", "reason"),
                    ("cwd", "cwd"),
                    ("kind", "kind"),
                    ("grantRoot", "grant_root"),
                ] {
                    if let Some(x) = p.get(from).filter(|x| !x.is_null()) {
                        codex.insert(to.into(), x.clone());
                    }
                }
                // `grantRoot`: approving also lets Codex write anywhere under that directory
                // for the rest of the session. Shown first, in full and marked like a command
                // (an invisible character in it shows), or the item offers only `deny`.
                let mut deny_only = None;
                let grant = p.get("grantRoot").filter(|x| !x.is_null()).map(|root| {
                    let root = root
                        .as_str()
                        .map_or_else(|| root.to_string(), str::to_string);
                    let grant = format!(
                        "Approving also lets Codex write anywhere under {} for the rest of \
                         the session. ",
                        mark_visible(&root)
                    );
                    if grant.chars().count() > GRANT_SUMMARY_MAX - 100 {
                        deny_only =
                            Some("the directory it would grant writes under cannot be shown");
                        String::new()
                    } else {
                        grant
                    }
                });
                // The command, as it runs: the user judges it from the card (newlines and
                // whitespace visible, [`render_command`]), all of it in the item's context
                // when the summary shows only its start.
                let room = if grant.is_some() {
                    400
                } else {
                    GRANT_SUMMARY_MAX - 100
                };
                let what = if method.starts_with("item/commandExecution") {
                    let cmd = render_command(&command_text(p), room);
                    if let Some(full) = &cmd.full {
                        codex.insert(FULL_INPUT_KEY.into(), json!(full));
                    }
                    if cmd.too_long {
                        deny_only = Some("the command is too long to show in full");
                    }
                    if cmd.line.is_empty() {
                        "Allow a shell command?".to_string()
                    } else {
                        format!("Allow shell: {}", cmd.line)
                    }
                } else {
                    // Already one line (`file_change_summary`): cut only.
                    truncate(
                        &match self.items.get(&item_id) {
                            Some(s) => format!("Allow {s}"),
                            None => "Allow a file change?".to_string(),
                        },
                        room,
                    )
                };
                let summary = format!("{}{what}", grant.unwrap_or_default());
                let (summary, options): (String, Vec<String>) = match deny_only {
                    None => (summary, vec!["allow".into(), "deny".into()]),
                    Some(why) => (format!("[deny only: {why}] {summary}"), vec!["deny".into()]),
                };
                let allow = options.iter().any(|o| o == "allow");
                let body = AttentionRaised {
                    attn_id: attn_id.clone(),
                    reason: AttentionReason::Permission,
                    summary,
                    options,
                    call_id: (!item_id.is_empty()).then_some(item_id),
                    ext: ext_of(codex),
                    ..Default::default()
                };
                self.pending.push(Pending {
                    request_id: Some(id.clone()),
                    turn_id: turn_id.clone(),
                    waiting: Waiting::Decision { allow },
                    attn_ids: vec![attn_id],
                    questions: vec![String::new()],
                    labels: vec![Vec::new()],
                    answers: vec![None],
                });
                let turn = self.turn_of(turn_id.as_deref());
                self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
            }
            "mcpServer/elicitation/request" if is_tool_approval(p) => {
                let server = str_at(p, "serverName").to_string();
                let base = match &turn_id {
                    Some(t) => format!("{t}#mcp{}", id_text(id)),
                    None => format!("codex-{}", uuid::Uuid::new_v4()),
                };
                let attn_id = self.attn_id(base);
                let mut codex = Map::new();
                codex.insert("method".into(), json!(method));
                codex.insert("request_id".into(), id.clone());
                codex.insert("server".into(), json!(server));
                if let Some(k) = p.pointer("/_meta/codex_approval_kind") {
                    codex.insert("approval_kind".into(), k.clone());
                }
                let message = str_at(p, "message");
                let summary = if message.is_empty() {
                    format!("Allow the {server} MCP server to run a tool?")
                } else {
                    message.to_string()
                };
                let body = AttentionRaised {
                    attn_id: attn_id.clone(),
                    reason: AttentionReason::Permission,
                    summary: one_line(&summary, 400),
                    options: vec!["allow".into(), "deny".into()],
                    ext: ext_of(codex),
                    ..Default::default()
                };
                self.pending.push(Pending {
                    request_id: Some(id.clone()),
                    turn_id: turn_id.clone(),
                    waiting: Waiting::Elicitation,
                    attn_ids: vec![attn_id],
                    questions: vec![String::new()],
                    labels: vec![Vec::new()],
                    answers: vec![None],
                });
                let turn = self.turn_of(turn_id.as_deref());
                self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
            }
            "mcpServer/elicitation/request" => {
                self.on_form_elicitation(id, p, turn_id, line, out);
            }
            "item/tool/requestUserInput" => {
                self.on_user_input(id, p, turn_id, line, out);
            }
            "item/permissions/requestApproval" => {
                self.on_permissions(id, p, turn_id, line, out);
            }
            _ => {
                tracing::warn!(
                    method,
                    "codex: answering an unsupported server request with an error"
                );
                // Recorded, so the error reply is never silent: an item raised and closed at
                // once.
                let attn_id = self.attn_id(format!("unsupported-{}", id_text(id)));
                let mut codex = Map::new();
                codex.insert("method".into(), json!(method));
                codex.insert("request_id".into(), id.clone());
                codex.insert("unsupported".into(), json!(true));
                let turn = self.turn_of(turn_id.as_deref());
                let body = AttentionRaised {
                    attn_id: attn_id.clone(),
                    reason: AttentionReason::Error,
                    summary: one_line(
                        &format!(
                            "Codex sent a request twr does not handle ({method}); twr \
                             answered it with an error"
                        ),
                        400,
                    ),
                    ext: ext_of(codex),
                    ..Default::default()
                };
                self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
                self.push(out, withdrawal(attn_id, "unsupported_request"), turn, None);
                out.replies.push(
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": METHOD_NOT_FOUND,
                            "message": format!("twr does not handle {method}"),
                        },
                    })
                    .to_string(),
                );
            }
        }
    }

    /// An MCP elicitation that is not a tool approval: a form to fill (`mode: form`) or a URL
    /// to open (`mode: url`). twr cannot show forms, so it becomes a question whose only
    /// answers are `decline` and `cancel`; nothing is sent until the user picks one.
    fn on_form_elicitation(
        &mut self,
        id: &Value,
        p: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        let server = str_at(p, "serverName").to_string();
        let base = match &turn_id {
            Some(t) => format!("{t}#mcp{}", id_text(id)),
            None => format!("codex-{}", uuid::Uuid::new_v4()),
        };
        let attn_id = self.attn_id(base);
        let mut codex = Map::new();
        codex.insert("method".into(), json!("mcpServer/elicitation/request"));
        codex.insert("request_id".into(), id.clone());
        codex.insert("server".into(), json!(server));
        codex.insert("elicitation".into(), json!(true));
        for (from, to) in [
            ("mode", "mode"),
            ("url", "url"),
            ("elicitationId", "elicitation_id"),
            ("requestedSchema", "requested_schema"),
        ] {
            if let Some(x) = p.get(from).filter(|x| !x.is_null()) {
                codex.insert(to.into(), x.clone());
            }
        }
        let who = if server.is_empty() {
            "An MCP server".to_string()
        } else {
            format!("MCP server {server}")
        };
        let message = str_at(p, "message");
        let mut summary = if message.is_empty() {
            format!("{who} asks for input twr cannot show")
        } else {
            format!("{who}: {message}")
        };
        if let Some(u) = opt_str(p, "url") {
            summary.push_str(&format!(" ({u})"));
        }
        summary.push_str(" [twr cannot fill forms: decline or cancel]");
        tracing::info!(%server, "codex: relaying an MCP elicitation twr cannot fill");
        let body = AttentionRaised {
            attn_id: attn_id.clone(),
            reason: AttentionReason::Question,
            summary: one_line(&summary, 400),
            options: vec!["decline".into(), "cancel".into()],
            ext: ext_of(codex),
            ..Default::default()
        };
        self.pending.push(Pending {
            request_id: Some(id.clone()),
            turn_id: turn_id.clone(),
            waiting: Waiting::FormElicitation,
            attn_ids: vec![attn_id],
            questions: vec![String::new()],
            labels: vec![Vec::new()],
            answers: vec![None],
        });
        let turn = self.turn_of(turn_id.as_deref());
        self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
    }

    /// `item/permissions/requestApproval`: the agent asks for more sandbox permissions
    /// (file-system paths, network) for the rest of the turn.
    fn on_permissions(
        &mut self,
        id: &Value,
        p: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        let item_id = str_at(p, "itemId").to_string();
        let base = if item_id.is_empty() {
            format!("codex-{}", uuid::Uuid::new_v4())
        } else {
            format!("{item_id}#perm")
        };
        let attn_id = self.attn_id(base);
        let requested = p.get("permissions").cloned().unwrap_or(Value::Null);
        let grant = permission_grant(&requested, self.cwd.as_deref());
        let mut codex = Map::new();
        codex.insert("method".into(), json!("item/permissions/requestApproval"));
        codex.insert("request_id".into(), id.clone());
        codex.insert("permissions".into(), requested.clone());
        codex.insert("scope".into(), json!("turn"));
        for (from, to) in [("reason", "reason"), ("cwd", "cwd")] {
            if let Some(x) = p.get(from).filter(|x| !x.is_null()) {
                codex.insert(to.into(), x.clone());
            }
        }
        let what = if grant.parts.is_empty() {
            "more sandbox permissions".to_string()
        } else {
            grant.parts.join(", ")
        };
        let mut summary = match opt_str(p, "reason") {
            Some(r) => format!("Allow {what} for this turn? {r}"),
            None => format!("Allow {what} for this turn?"),
        };
        // An allow grants only what the user can see in full: anything twr does not
        // recognize, or a request too long to show, can only be denied.
        let mut deny_only = Vec::new();
        if !grant.unrecognized.is_empty() {
            codex.insert("unrecognized".into(), json!(grant.unrecognized));
            deny_only.push(format!(
                "it also asks for what twr does not recognize ({})",
                grant.unrecognized.join("; ")
            ));
        }
        if summary.chars().count() > GRANT_SUMMARY_MAX {
            deny_only.push("the request is too long to show in full".to_string());
        }
        let options = if deny_only.is_empty() {
            codex.insert("grant".into(), grant.profile.clone());
            vec!["allow".into(), "deny".into()]
        } else {
            summary = format!("[deny only: {}] {summary}", deny_only.join("; "));
            vec!["deny".into()]
        };
        let body = AttentionRaised {
            attn_id: attn_id.clone(),
            reason: AttentionReason::Permission,
            summary: one_line(&summary, GRANT_SUMMARY_MAX),
            options,
            call_id: (!item_id.is_empty()).then_some(item_id),
            ext: ext_of(codex),
            ..Default::default()
        };
        let granted = deny_only.is_empty().then_some(grant.profile);
        self.pending.push(Pending {
            request_id: Some(id.clone()),
            turn_id: turn_id.clone(),
            waiting: Waiting::Permissions { granted },
            attn_ids: vec![attn_id],
            questions: vec![String::new()],
            labels: vec![Vec::new()],
            answers: vec![None],
        });
        let turn = self.turn_of(turn_id.as_deref());
        self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
    }

    fn on_user_input(
        &mut self,
        id: &Value,
        p: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        let item_id = str_at(p, "itemId").to_string();
        let qs: Vec<&Value> = p["questions"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        if qs.is_empty() {
            out.replies
                .push(json!({"jsonrpc": "2.0", "id": id, "result": {"answers": {}}}).to_string());
            return;
        }
        let n = qs.len();
        let base = if item_id.is_empty() {
            format!("codex-{}", uuid::Uuid::new_v4())
        } else {
            item_id.clone()
        };
        let mut pending = Pending {
            request_id: Some(id.clone()),
            turn_id: turn_id.clone(),
            waiting: Waiting::UserInput {
                ids: qs.iter().map(|q| str_at(q, "id").to_string()).collect(),
            },
            attn_ids: Vec::new(),
            questions: Vec::new(),
            labels: Vec::new(),
            answers: vec![None; n],
        };
        let turn = self.turn_of(turn_id.as_deref());
        for (i, q) in qs.iter().enumerate() {
            let attn_id = self.attn_id(if n == 1 {
                base.clone()
            } else {
                format!("{base}#q{i}")
            });
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
            let mut codex = Map::new();
            codex.insert("method".into(), json!("item/tool/requestUserInput"));
            codex.insert("request_id".into(), id.clone());
            codex.insert("question_id".into(), json!(str_at(q, "id")));
            if let Some(h) = opt_str(q, "header") {
                codex.insert("header".into(), json!(h));
            }
            for (from, to) in [("isOther", "is_other"), ("isSecret", "is_secret")] {
                if q.get(from).and_then(Value::as_bool) == Some(true) {
                    codex.insert(to.into(), json!(true));
                }
            }
            let text = if question.is_empty() {
                "Codex asks a question".to_string()
            } else {
                question.clone()
            };
            let summary = if n > 1 {
                format!("({}/{n}) {text}", i + 1)
            } else {
                text
            };
            let body = AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Question,
                summary: one_line(&summary, 400),
                options: labels.clone(),
                call_id: (!item_id.is_empty()).then(|| item_id.clone()),
                ext: ext_of(codex),
                ..Default::default()
            };
            pending.attn_ids.push(attn_id);
            pending.questions.push(question);
            pending.labels.push(labels);
            self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
        }
        self.pending.push(pending);
    }

    /// `agentMessage.questions` (`request_user_input_async`): one question item per question,
    /// answered with `turn/steer` on the running turn.
    fn on_async_questions(
        &mut self,
        item: &Value,
        qs: &[Value],
        turn_id: Option<String>,
        turn: Option<u64>,
        line: &str,
        out: &mut Translated,
    ) {
        let Some(codex_turn) = turn_id.clone() else {
            tracing::warn!("codex: async questions without a turn id; shown as text only");
            return;
        };
        let item_id = str_at(item, "id").to_string();
        // Open items never change: the same message item asking again replaces the set.
        self.supersede(
            |p| matches!(&p.waiting, Waiting::Steer { item_id: i, .. } if *i == item_id),
            line,
        );
        let n = qs.len();
        let mut pending = Pending {
            request_id: None,
            turn_id,
            waiting: Waiting::Steer {
                turn_id: codex_turn,
                item_id: item_id.clone(),
            },
            attn_ids: Vec::new(),
            questions: Vec::new(),
            labels: Vec::new(),
            answers: vec![None; n],
        };
        for (i, q) in qs.iter().enumerate() {
            let attn_id = self.attn_id(if n == 1 {
                item_id.clone()
            } else {
                format!("{item_id}#q{i}")
            });
            let title = str_at(q, "title").to_string();
            let labels: Vec<String> = q["options"]
                .as_array()
                .map(|o| {
                    o.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let mut codex = Map::new();
            codex.insert("delivery".into(), json!("async"));
            let text = if title.is_empty() {
                "Codex asks a question".to_string()
            } else {
                title.clone()
            };
            let summary = if n > 1 {
                format!("({}/{n}) {text}", i + 1)
            } else {
                text
            };
            let body = AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Question,
                summary: one_line(&summary, 400),
                options: labels.clone(),
                call_id: (!item_id.is_empty()).then(|| item_id.clone()),
                ext: ext_of(codex),
                ..Default::default()
            };
            pending.attn_ids.push(attn_id);
            pending.questions.push(title);
            pending.labels.push(labels);
            self.push(out, EventBody::AttentionRaised(body), turn, Some(line));
        }
        self.pending.push(pending);
    }

    /// Sends `thread/start` (or `thread/resume`).
    fn open_thread(&mut self, out: &mut Translated) {
        let (method, params) = (self.open.method, self.open.params.clone());
        let l = self.request(Req::Open, method, params);
        out.replies.push(l);
    }

    /// Turns off, in the open params, every MCP server of the effective Codex config `result`
    /// (a `config/read` answer) that is not in the thread's set (module docs, "MCP
    /// allowlist"). An error (why) when the config defines a server under one of the set's
    /// names: Codex would merge the two.
    fn disable_inherited_mcp(&mut self, result: &Value) -> Result<(), String> {
        let Some(allow) = &self.mcp_allow else {
            return Ok(());
        };
        let configured: Vec<String> = result
            .pointer("/config/mcp_servers")
            .and_then(Value::as_object)
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(name) = configured.iter().find(|n| allow.contains(n)) {
            return Err(format!(
                "the Codex config defines an MCP server named {name:?}, which Codex would merge \
                 with twr's own: rename or remove it"
            ));
        }
        let Some(servers) = self
            .open
            .params
            .pointer_mut("/config/mcp_servers")
            .and_then(Value::as_object_mut)
        else {
            return Ok(());
        };
        for name in configured {
            servers.insert(name, json!({"enabled": false}));
        }
        Ok(())
    }

    /// Ends the session when `mcpServer/startupStatus/updated` `p` names an MCP server outside
    /// the thread's set (module docs, "MCP allowlist").
    fn check_mcp_server(&self, p: &Value, out: &mut Translated) {
        let Some(allow) = &self.mcp_allow else {
            return;
        };
        let name = str_at(p, "name");
        if !allow.iter().any(|a| a == name) {
            out.fatal = Some(format!(
                "codex started the MCP server {name:?}, which is not one twr gave this thread"
            ));
        }
    }

    // --- notifications -----------------------------------------------------------------------

    fn on_notification(&mut self, method: &str, p: &Value, line: &str, out: &mut Translated) {
        if method == "mcpServer/startupStatus/updated" {
            self.check_mcp_server(p, out);
            return;
        }
        // A child thread's notifications are its background activity, except the withdrawal of
        // a request it raised: that request is pending here like the parent's.
        if let Some(thread) = opt_str(p, "threadId")
            .filter(|id| Some(id) != self.thread_id.as_ref())
            .filter(|_| method != "serverRequest/resolved")
        {
            let id = format!("agent:{thread}");
            if method == "thread/status/changed" && self.tasks.knows(&id) {
                match p.pointer("/status/type").and_then(Value::as_str) {
                    Some("active") => self.activate_child(&thread, "", line, out),
                    Some("idle" | "systemError" | "notLoaded") => {
                        self.check_child(&thread, line, out)
                    }
                    _ => {}
                }
            } else if matches!(
                method,
                "item/agentMessage/delta"
                    | "item/commandExecution/outputDelta"
                    | "item/reasoning/summaryTextDelta"
                    | "item/reasoning/textDelta"
            ) && !str_at(p, "delta").is_empty()
            {
                self.task_progress(&id, line, out);
            }
            return;
        }
        let turn_id = opt_str(p, "turnId");
        match method {
            "turn/started" => {
                if let Some(id) = p.pointer("/turn/id").and_then(Value::as_str) {
                    if self.ready
                        && self.current.is_none()
                        && self.queued.is_empty()
                        && !self.turn_ids.contains_key(id)
                    {
                        self.current = Some(Current {
                            turn: None,
                            codex_id: None,
                            base: self.tokens,
                            usage_seen: false,
                            interrupt_wanted: false,
                            interrupt_sent: false,
                            last_error: None,
                        });
                        self.push_runtime_start(id, line, out);
                    }
                    if self
                        .current
                        .as_ref()
                        .is_some_and(|c| c.turn.is_some() && c.codex_id.is_none())
                        && !self.turn_ids.contains_key(id)
                    {
                        // twr's `turn/start` has no answer yet: this may be Codex's own turn,
                        // begun before twr's. The answer tells (`settle_unbound`).
                        if !self.unbound.iter().any(|u| u.id == id) {
                            self.unbound.push(Unbound {
                                id: id.to_string(),
                                base: self.tokens,
                                usage_seen: false,
                                events: Vec::new(),
                                last_error: None,
                                completed: None,
                            });
                        }
                    } else {
                        self.bind(id);
                    }
                }
            }
            "item/agentMessage/delta" => {
                let delta = str_at(p, "delta");
                if !delta.is_empty() {
                    let turn = self.turn_of(turn_id.as_deref());
                    let body = EventBody::AgentText(AgentText {
                        text: delta.into(),
                        delta: true,
                        ..Default::default()
                    });
                    self.push(out, body, turn, None);
                }
            }
            "item/commandExecution/outputDelta" if !str_at(p, "delta").is_empty() => {
                self.task_progress(&format!("shell:{}", str_at(p, "itemId")), line, out);
            }
            "item/started" => self.on_item_started(&p["item"], turn_id, line, out),
            "item/completed" => self.on_item_completed(&p["item"], turn_id, line, out),
            "item/autoApprovalReview/completed" => self.on_auto_review(p, turn_id, line, out),
            "serverRequest/resolved" => {
                let rid = &p["requestId"];
                let Some(pi) = self
                    .pending
                    .iter()
                    .position(|x| x.request_id.as_ref() == Some(rid))
                else {
                    return;
                };
                let pending = self.pending.remove(pi);
                tracing::info!(request = %rid, "codex: runtime withdrew a pending request");
                for (id, a) in pending.attn_ids.into_iter().zip(pending.answers) {
                    if a.is_none() {
                        self.withdraw(pending.turn_id.as_deref(), id, "runtime", line);
                    }
                }
            }
            "thread/tokenUsage/updated" => {
                let Some(total) = p.pointer("/tokenUsage/total") else {
                    return;
                };
                let total = Tokens::of(total);
                self.tokens = total;
                // A turn whose owner is not known yet: its usage is its own, not history.
                if let Some(u) = turn_id
                    .as_ref()
                    .and_then(|t| self.unbound.iter_mut().find(|u| &u.id == t))
                {
                    u.usage_seen = true;
                } else if let Some(cur) = self.current.as_mut() {
                    let ours = turn_id.is_some() && turn_id == cur.codex_id;
                    if ours {
                        cur.usage_seen = true;
                    } else if !cur.usage_seen {
                        // History replayed by a resumed thread, before this turn's first call.
                        cur.base = total;
                    }
                }
            }
            "turn/completed" => {
                self.on_turn_completed(p, line, out);
                self.read_usage(out);
            }
            "account/rateLimits/updated" => self.usage_report(&p["rateLimits"], "event", out),
            // A failed request (`willRetry`: Codex tries again). Kept as the cause of the turn,
            // should it fail without naming one; one Codex gives up on wins over a retried one.
            "error" => {
                let retry = p.get("willRetry").and_then(Value::as_bool).unwrap_or(false);
                let Some(err) = p.get("error").filter(|e| e.is_object()) else {
                    return;
                };
                // The error of a turn whose owner is not known yet, or of Codex's own beside
                // twr's, stays that turn's; the running turn takes one of no other known turn.
                let known = turn_id
                    .as_ref()
                    .filter(|t| self.turn_ids.contains_key(t.as_str()));
                let noted = if let Some(u) = turn_id
                    .as_ref()
                    .and_then(|t| self.unbound.iter_mut().find(|u| &u.id == t))
                {
                    Some(&mut u.last_error)
                } else if let Some(own) = turn_id.as_ref().and_then(|t| self.own_turns.get_mut(t)) {
                    Some(&mut own.last_error)
                } else {
                    self.current
                        .as_mut()
                        .filter(|c| {
                            turn_id.is_none()
                                || turn_id == c.codex_id
                                || (c.codex_id.is_none() && known.is_none())
                        })
                        .map(|c| &mut c.last_error)
                };
                if let Some(noted) = noted {
                    if !retry || noted.as_ref().is_none_or(|(_, r)| *r) {
                        *noted = Some((err.clone(), retry));
                    }
                }
            }
            _ => {}
        }
    }

    fn on_item_started(
        &mut self,
        item: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        self.item_tasks(item, false, line, out);
        let Some(call) = tool_call(item, self.cwd.as_deref()) else {
            return;
        };
        let id = call.call_id.clone();
        if id.is_empty() || self.items.contains_key(&id) {
            return;
        }
        self.items.insert(id, call.input_summary.clone());
        let turn = self.turn_of(turn_id.as_deref());
        self.push(out, EventBody::AgentToolCall(call), turn, Some(line));
    }

    /// A decision of Codex's reviewer agent (`[codex] auto = "auto_review"`), which answers
    /// what the sandbox does not allow instead of the user: an `auto_review` tool call (the
    /// action it judged) and its result (`ok` when approved; the status, risk, authorization
    /// and rationale), so the transcript shows what ran outside the sandbox unasked and what
    /// was refused. The whole review is kept as `ext.codex.review`.
    fn on_auto_review(
        &mut self,
        p: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        let review = &p["review"];
        let action = &p["action"];
        let id = format!("auto_review:{}", str_at(p, "reviewId"));
        let status = str_at(review, "status");
        let approved = status == "approved";
        let what = match str_at(action, "command") {
            "" => compact(action),
            command => command.to_string(),
        };
        let turn = self.turn_of(turn_id.as_deref());
        let mut codex = Map::new();
        codex.insert("item_type".into(), json!("autoApprovalReview"));
        for (from, to) in [
            ("targetItemId", "target_item_id"),
            ("decisionSource", "decision_source"),
        ] {
            if let Some(x) = p.get(from).filter(|x| !x.is_null()) {
                codex.insert(to.into(), x.clone());
            }
        }
        codex.insert("action".into(), action.clone());
        let call = AgentToolCall {
            call_id: id.clone(),
            tool: "auto_review".into(),
            input_summary: one_line(&what, INPUT_SUMMARY_MAX),
            input_ref: None,
            risky: approved,
            ext: ext_of(codex),
            ..Default::default()
        };
        self.push(out, EventBody::AgentToolCall(call), turn, Some(line));
        let mut said = if status.is_empty() {
            "no decision"
        } else {
            status
        }
        .to_string();
        let grades: Vec<String> = [
            ("riskLevel", "risk"),
            ("userAuthorization", "authorization"),
        ]
        .into_iter()
        .filter_map(|(k, name)| opt_str(review, k).map(|v| format!("{name} {v}")))
        .collect();
        if !grades.is_empty() {
            said = format!("{said} ({})", grades.join(", "));
        }
        if let Some(why) = opt_str(review, "rationale") {
            said = format!("{said}: {why}");
        }
        let mut codex = Map::new();
        codex.insert("status".into(), json!(status));
        codex.insert("review".into(), review.clone());
        let result = AgentToolResult {
            call_id: id,
            ok: approved,
            output_summary: one_line(&said, OUTPUT_SUMMARY_MAX),
            output_ref: None,
            ext: ext_of(codex),
            ..Default::default()
        };
        self.push(out, EventBody::AgentToolResult(result), turn, None);
    }

    fn on_item_completed(
        &mut self,
        item: &Value,
        turn_id: Option<String>,
        line: &str,
        out: &mut Translated,
    ) {
        self.item_tasks(item, true, line, out);
        let turn = self.turn_of(turn_id.as_deref());
        match str_at(item, "type") {
            "agentMessage" => {
                let text = str_at(item, "text");
                if !text.is_empty() {
                    let mut codex = Map::new();
                    if let Some(ph) = item.get("phase").filter(|x| !x.is_null()) {
                        codex.insert("phase".into(), ph.clone());
                    }
                    let body = EventBody::AgentText(AgentText {
                        text: text.into(),
                        is_final: true,
                        ext: ext_of(codex),
                        ..Default::default()
                    });
                    self.push(out, body, turn, Some(line));
                }
                if let Some(qs) = item["questions"].as_array().filter(|q| !q.is_empty()) {
                    let qs = qs.clone();
                    self.on_async_questions(item, &qs, turn_id, turn, line, out);
                }
            }
            "reasoning" if self.opts.thinking => {
                let join = |k: &str| {
                    item[k]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                        .unwrap_or_default()
                };
                let mut text = join("summary");
                if text.trim().is_empty() {
                    text = join("content");
                }
                if !text.trim().is_empty() {
                    let body = EventBody::AgentThinking(AgentThinking {
                        text,
                        ..Default::default()
                    });
                    self.push(out, body, turn, Some(line));
                }
            }
            _ => {
                let Some(call) = tool_call(item, self.cwd.as_deref()) else {
                    return;
                };
                let id = call.call_id.clone();
                if id.is_empty() {
                    return;
                }
                if !self.items.contains_key(&id) {
                    self.items.insert(id.clone(), call.input_summary.clone());
                    self.push(out, EventBody::AgentToolCall(call), turn, Some(line));
                }
                let body = EventBody::AgentToolResult(tool_result(item));
                self.push(out, body, turn, Some(line));
            }
        }
    }

    fn on_turn_completed(&mut self, p: &Value, line: &str, out: &mut Translated) {
        self.on_turn_completed_at(p, line, self.tokens, out);
    }

    /// [`Translator::on_turn_completed`], the thread token total being `tokens` when the turn
    /// completed (a held completion is handled later, `settle_unbound`).
    fn on_turn_completed_at(
        &mut self,
        p: &Value,
        line: &str,
        tokens: Tokens,
        out: &mut Translated,
    ) {
        let t = &p["turn"];
        let codex_id = opt_str(t, "id");
        // Whose turn this is waits for the answer to twr's `turn/start` (`settle_unbound`).
        if let Some(held) = codex_id
            .as_ref()
            .and_then(|id| self.unbound.iter_mut().find(|u| &u.id == id))
        {
            held.completed = Some((p.clone(), line.to_string(), tokens));
            return;
        }
        let mapped = codex_id
            .as_ref()
            .and_then(|id| self.turn_ids.get(id).copied());
        // A running turn not named yet takes a completion of a turn this process does not know
        // (one it knows, Codex's own included, is never twr's unnamed one).
        let is_current = match (&self.current, &codex_id) {
            (Some(cur), Some(id)) => {
                cur.codex_id.as_deref() == Some(id)
                    || (cur.codex_id.is_none() && !self.turn_ids.contains_key(id))
            }
            (Some(_), None) => true,
            _ => false,
        };
        if !is_current && mapped.is_none() {
            tracing::debug!(turn = ?codex_id, "codex: turn/completed for a turn this process did not start");
            return;
        }
        let (turn, cur) = if is_current {
            let cur = self.current.take();
            (cur.as_ref().and_then(|c| c.turn), cur)
        } else {
            (mapped.flatten(), None)
        };
        // Reconcile promptly at turn end. Follow an older in-flight snapshot with a fresh
        // read; running command items themselves keep the thread active meanwhile.
        if is_current && self.terminal_api {
            if self.requests.values().any(|r| matches!(r, Req::Terminals)) {
                self.check_after_turn = true;
            } else {
                out.replies.push(self.request(
                    Req::Terminals,
                    "thread/backgroundTerminals/list",
                    json!({"threadId": self.thread_id, "limit": 100}),
                ));
            }
        }
        let status = str_at(t, "status");
        let result = match status {
            "completed" => TurnResult::Completed,
            "interrupted" => TurnResult::Interrupted,
            _ => TurnResult::Error,
        };
        let mut codex = Map::new();
        codex.insert("status".into(), json!(status));
        if let Some(id) = &codex_id {
            codex.insert("turn_id".into(), json!(id));
        }
        // Codex's own turn beside twr's counts from its own start, and twr's turn, which runs
        // after it, from its end.
        let own_turn = codex_id
            .as_ref()
            .filter(|_| !is_current)
            .and_then(|id| self.own_turns.remove(id));
        let own_base = own_turn.as_ref().map(|o| o.base);
        if own_base.is_some() {
            if let Some(c) = self
                .current
                .as_mut()
                .filter(|c| c.turn.is_some() && !c.usage_seen)
            {
                c.base = tokens;
            }
        }
        let usage = cur.as_ref().map(|c| c.base).or(own_base).map(|base| {
            let d = tokens.minus(base);
            codex.insert("cached_input_tokens".into(), json!(d.cached));
            codex.insert("reasoning_output_tokens".into(), json!(d.reasoning));
            codex.insert("thread_total_tokens".into(), json!(tokens.total));
            Usage {
                input: d.input,
                output: d.output,
                cost: None,
            }
        });
        if let Some(err) = t.get("error").filter(|e| !e.is_null()) {
            codex.insert("error".into(), err.clone());
        }
        // Every failed turn says why (`ext.twr.error`, what the hub quotes): the turn's error,
        // else its last `error` notification. Not signed in (or the login expired): said with
        // what to do, and never transient (Codex reports it as a failed connection, which
        // otherwise is).
        let auth = result == TurnResult::Error && auth_failure(&t["error"]);
        let twr_error = (result == TurnResult::Error).then(|| {
            let noted = cur
                .as_ref()
                .and_then(|c| c.last_error.as_ref())
                .or_else(|| own_turn.as_ref().and_then(|o| o.last_error.as_ref()));
            let text = error_text(&t["error"])
                .or_else(|| noted.and_then(|(e, _)| error_text(e)))
                .unwrap_or_else(|| match status {
                    "" => "Codex ended the turn without a status".to_string(),
                    s => format!("Codex ended the turn with status {s}"),
                });
            if auth {
                format!("{}{AUTH_HINT}", error_line(&text))
            } else {
                error_line(&text)
            }
        });
        if auth {
            codex.insert("auth_failed".into(), json!(true));
            codex.insert("transient".into(), json!(false));
        } else if result == TurnResult::Error {
            let reason = transient_reason(&t["error"]);
            codex.insert("transient".into(), json!(reason.is_some()));
            if let Some(r) = reason {
                codex.insert("transient_reason".into(), json!(r));
            }
        }
        // Requests of a finished turn can no longer be answered. The hub resolves the items of a
        // turn it ends; Codex's own turn may have gone into twr's (its end then closes nothing),
        // so its items are withdrawn here.
        let (done, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|x| match &x.turn_id {
                Some(id) => codex_id.as_ref() == Some(id),
                // Anything else still pending belonged to this turn.
                None => is_current,
            });
        self.pending = keep;
        if turn.is_none() {
            for x in done {
                for (id, a) in x.attn_ids.into_iter().zip(x.answers) {
                    if a.is_none() {
                        self.push(out, withdrawal(id, "turn_completed"), None, Some(line));
                    }
                }
            }
        }
        if is_current {
            // Item ids are per turn: nothing refers to them once the turn is over.
            self.items.clear();
        }
        let mut ext = ext_of(codex);
        if let Some(e) = twr_error {
            ext.get_or_insert_with(JsonMap::new)
                .insert("twr".into(), json!({ "error": e }));
        }
        // Codex's own turn: its end carries its id, so it closes only the turn its start opened.
        if turn.is_none() {
            if let Some(id) = codex_id.or_else(|| cur.as_ref().and_then(|c| c.codex_id.clone())) {
                set_twr_ext(&mut ext, RUNTIME_TURN_KEY, json!(id));
            }
        }
        let body = EventBody::TurnEnded(TurnEnded {
            result,
            session_id: self.thread_id.clone(),
            usage,
            duration_ms: t.get("durationMs").and_then(Value::as_u64),
            ext,
            ..Default::default()
        });
        self.push(out, body, turn, Some(line));
        out.replies.extend(self.dispatch());
    }

    /// The `turn.started{from: runtime}` of Codex's own turn `id`, carrying that id.
    fn push_runtime_start(&self, id: &str, line: &str, out: &mut Translated) {
        let mut ext = None;
        set_twr_ext(&mut ext, RUNTIME_TURN_KEY, json!(id));
        self.push(
            out,
            EventBody::TurnStarted(TurnStarted {
                from: TurnFrom::Runtime,
                prompt: "[runtime resumed after background activity]".into(),
                ext,
                ..Default::default()
            }),
            None,
            Some(line),
        );
    }

    /// The answer to twr's `turn/start` named twr's turn `ours` (none: it failed): every other
    /// turn Codex started meanwhile is its own, begun before twr's. The hub drops such a start
    /// while twr's turn is open, and that turn's events go to twr's. A completion held for one
    /// of them is handled now. With twr's turn failed, one still running is the running turn:
    /// no prompt starts before it completes.
    fn settle_unbound(&mut self, ours: Option<&str>, line: &str, out: &mut Translated) {
        for u in std::mem::take(&mut self.unbound) {
            let own = Some(u.id.as_str()) != ours;
            if own {
                self.turn_ids.insert_new(u.id.clone(), None);
                self.push_runtime_start(&u.id, line, out);
                if u.completed.is_none() && ours.is_none() && self.current.is_none() {
                    self.current = Some(Current {
                        turn: None,
                        codex_id: Some(u.id.clone()),
                        base: u.base,
                        usage_seen: u.usage_seen,
                        interrupt_wanted: false,
                        interrupt_sent: false,
                        last_error: u.last_error,
                    });
                } else {
                    self.own_turns.insert(
                        u.id.clone(),
                        OwnTurn {
                            base: u.base,
                            last_error: u.last_error,
                        },
                    );
                }
            } else if let Some(cur) = self.current.as_mut() {
                cur.usage_seen |= u.usage_seen;
                if u.last_error.is_some() {
                    cur.last_error = u.last_error;
                }
            }
            // A turn that completed meanwhile: its requests can no longer be answered, so they
            // are never shown.
            let mut events = u.events;
            if u.completed.is_some() {
                let (done, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending)
                    .into_iter()
                    .partition(|x| x.turn_id.as_deref() == Some(u.id.as_str()));
                self.pending = keep;
                let gone: HashSet<String> = done.into_iter().flat_map(|x| x.attn_ids).collect();
                events.retain(|e| match &e.body {
                    EventBody::AttentionRaised(a) => !gone.contains(&a.attn_id),
                    EventBody::AttentionResolved(r) => !gone.contains(&r.attn_id),
                    _ => true,
                });
            }
            // Its requests: twr's turn's, or with no turn for Codex's own, so they go to the
            // turn the hub has open, which outlives them (`Adapter`).
            let turn = if own { None } else { self.turn_of(Some(&u.id)) };
            for mut e in events {
                e.turn = turn;
                out.events.push(e);
            }
            if let Some((p, held_line, tokens)) = u.completed {
                self.on_turn_completed_at(&p, &held_line, tokens, out);
            }
        }
    }
}

/// A map of the last `cap` keys inserted (the oldest is evicted first).
#[derive(Debug)]
struct Recent<V> {
    map: HashMap<String, V>,
    order: VecDeque<String>,
    cap: usize,
}

impl<V> Recent<V> {
    fn new(cap: usize) -> Self {
        Recent {
            map: HashMap::new(),
            order: VecDeque::new(),
            cap: cap.max(1),
        }
    }

    fn get(&self, k: &str) -> Option<&V> {
        self.map.get(k)
    }

    fn contains_key(&self, k: &str) -> bool {
        self.map.contains_key(k)
    }

    /// Inserts `k` unless present (the value is kept). Returns whether it was new.
    fn insert_new(&mut self, k: String, v: V) -> bool {
        if self.map.contains_key(&k) {
            return false;
        }
        self.order.push_back(k.clone());
        self.map.insert(k, v);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        true
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }
}

/// How a pending request is answered.
enum Reply {
    Result(Value),
    Error(String),
    Steer(String),
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// The windows of a Codex `rateLimits` snapshot: `primary` and `secondary`, named by their
/// length (a null one is unknown and left out).
pub fn rate_limit_windows(snapshot: &Value) -> Vec<UsageWindow> {
    ["primary", "secondary"]
        .into_iter()
        .filter_map(|key| {
            let w = &snapshot[key];
            let used = crate::usage::pct(&w["usedPercent"])?;
            let resets = crate::usage::epoch(&w["resetsAt"]);
            Some(match w["windowDurationMins"].as_u64() {
                Some(m) => UsageWindow::of_minutes(m, used, resets),
                None => UsageWindow {
                    window: key.into(),
                    used_pct: used,
                    resets_at: resets,
                    minutes: None,
                    observed_at: None,
                },
            })
        })
        .collect()
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

fn id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn ext_of(codex: Map<String, Value>) -> Option<JsonMap> {
    if codex.is_empty() {
        return None;
    }
    let mut m = JsonMap::new();
    m.insert(EXT.into(), Value::Object(codex));
    Some(m)
}

fn withdrawal(attn_id: String, cause: &str) -> EventBody {
    let mut codex = Map::new();
    codex.insert("cancelled_by".into(), json!(cause));
    if cause == "superseded" {
        codex.insert("superseded".into(), json!(true));
    }
    EventBody::AttentionResolved(AttentionResolved {
        attn_id,
        by: ResolvedBy::Timeout,
        answer: None,
        ext: ext_of(codex),
        ..Default::default()
    })
}

fn truncate(s: &str, max_chars: usize) -> String {
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

/// Whether an elicitation asks to run an MCP tool (a plain allow/deny, no form to fill).
fn is_tool_approval(p: &Value) -> bool {
    if p.pointer("/_meta/codex_approval_kind")
        .and_then(Value::as_str)
        == Some("mcp_tool_call")
    {
        return true;
    }
    let props_empty = p
        .pointer("/requestedSchema/properties")
        .and_then(Value::as_object)
        .is_none_or(Map::is_empty);
    str_at(p, "mode") == "form" && props_empty
}

/// The command Codex runs, without its `/bin/bash -lc '...'` wrapper: the script, when the
/// rest of the line is exactly one single-quoted word (`'\\''` inside it is a quote) or one
/// double-quoted word, else the command as given. Nothing is trimmed: an approval shows what
/// runs, and a space at an end can be part of it (`echo value\\ `).
pub fn unwrap_shell(command: &str) -> String {
    for prefix in [
        "/bin/bash -lc ",
        "bash -lc ",
        "/bin/zsh -lc ",
        "zsh -lc ",
        "/bin/sh -c ",
        "sh -c ",
    ] {
        let Some(rest) = command.strip_prefix(prefix) else {
            continue;
        };
        if rest.len() >= 2 && rest.starts_with('\'') && rest.ends_with('\'') {
            let inner = &rest[1..rest.len() - 1];
            // One quoted word: no quote in it but the `'\''` escapes.
            if !inner.replace("'\\''", "").contains('\'') {
                return inner.replace("'\\''", "'");
            }
        } else if rest.len() >= 2
            && rest.starts_with('"')
            && rest.ends_with('"')
            && !rest[1..rest.len() - 1].contains(['"', '\\', '$', '`'])
        {
            return rest[1..rest.len() - 1].to_string();
        }
        break;
    }
    command.to_string()
}

/// One-line command of an approval request or `commandExecution` item.
fn command_summary(v: &Value) -> String {
    one_line(&command_text(v), INPUT_SUMMARY_MAX)
}

/// The command of an approval or command item, uncut: `command` (unwrapped from its shell),
/// else its `commandActions` joined with `&&`.
fn command_text(v: &Value) -> String {
    let cmd = str_at(v, "command");
    let text = if cmd.is_empty() {
        v["commandActions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|x| str_at(x, "command"))
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(" && ")
            })
            .unwrap_or_default()
    } else {
        unwrap_shell(cmd)
    };
    text
}

/// Whether any string in `v` matches a [`DESTRUCTIVE_PATTERNS`] entry.
pub fn is_destructive(v: &Value) -> bool {
    match v {
        Value::String(s) => {
            let lower = s.to_lowercase();
            DESTRUCTIVE_PATTERNS.iter().any(|p| lower.contains(p))
        }
        Value::Array(a) => a.iter().any(is_destructive),
        Value::Object(m) => m.values().any(is_destructive),
        _ => false,
    }
}

/// How much an `approvalPolicy` lets run without asking (higher: more), for the string
/// policies twr knows; None for anything else (a granular policy object, a newer value).
pub(crate) fn approval_rank(policy: &str) -> Option<u8> {
    match policy {
        "untrusted" => Some(0),
        "on-failure" | "on-request" => Some(1),
        "never" => Some(2),
        _ => None,
    }
}

/// How much a sandbox lets commands do (higher: more), from its request spelling
/// (`read-only`) or the reported `sandbox.type` (`readOnly`); None when twr does not know it.
pub(crate) fn sandbox_rank(sandbox: &str) -> Option<u8> {
    let key: String = sandbox
        .chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect();
    match key.as_str() {
        "readonly" => Some(0),
        "workspacewrite" => Some(1),
        "dangerfullaccess" => Some(2),
        _ => None,
    }
}

/// Checks the policy a `thread/start` / `thread/resume` result reports (`approvalPolicy`,
/// `sandbox.type`, `approvalsReviewer`) against the request's `params`: each must be reported,
/// known, and no wider than requested (the reviewer `user`, or the `auto_review` asked for). Short of
/// `dangerFullAccess` (which allows everything anyway, and only when asked for), the sandbox's
/// extras are checked too ([`check_sandbox_extras`]): network access only when twr asked for
/// it, and only writable roots twr asked for. Err says what is not.
pub fn check_policy(params: &Value, result: &Value) -> Result<(), String> {
    let asked = str_at(params, "approvalPolicy");
    let got = result.get("approvalPolicy");
    match (
        approval_rank(asked),
        got.and_then(Value::as_str).and_then(approval_rank),
    ) {
        (Some(a), Some(g)) if g <= a => {}
        _ => {
            return Err(format!(
                "the thread reports approvalPolicy {} where twr asked for {asked:?}; it may run \
                 more without asking",
                got.map_or("(none)".to_string(), compact)
            ))
        }
    }
    let asked = str_at(params, "sandbox");
    let got = result
        .get("sandbox")
        .and_then(|s| s.as_str().or_else(|| s.get("type").and_then(Value::as_str)));
    match (sandbox_rank(asked), got.and_then(sandbox_rank)) {
        // `dangerFullAccess` (asked for) allows everything anyway.
        (Some(a), Some(g)) if g <= a => {
            if g < 2 {
                check_sandbox_extras(params, result)?;
            }
        }
        _ => {
            return Err(format!(
                "the thread reports sandbox {:?} where twr asked for {asked:?}; commands may do \
                 more than allowed",
                got.unwrap_or("(none)")
            ))
        }
    }
    // `auto_review` (`[codex] auto`) only when asked for; `guardian_subagent` is Codex's
    // older name for it.
    let asked = str_at(params, "approvalsReviewer");
    match result.get("approvalsReviewer").and_then(Value::as_str) {
        Some("user") => Ok(()),
        Some("auto_review" | "guardian_subagent") if asked == "auto_review" => Ok(()),
        other => Err(format!(
            "the thread reports approvalsReviewer {:?} where twr asked for {asked:?}; approvals \
             would not reach the user",
            other.unwrap_or("(none)")
        )),
    }
}

/// Checks a sandbox's `networkAccess` and `writableRoots` (as a `thread/start` /
/// `thread/resume` result reports them) against what the request's `config` asked for
/// (`codex::NETWORK_KEY`, `codex::WRITABLE_ROOTS_KEY`, which twr always sends with a
/// `workspace-write` sandbox): network access only when asked for, each root one that was
/// asked for (compared as paths). A user's or a project's Codex config adds both to a sandbox
/// unless overridden, so what was not asked for came from there. Not reported (or null): none.
/// Anything else, a value twr does not know the meaning of, is refused.
fn check_sandbox_extras(params: &Value, result: &Value) -> Result<(), String> {
    let config = params.get("config");
    let asked = |key: &str| config.and_then(|c| c.get(key));
    let sandbox = result.get("sandbox");
    let network = sandbox.and_then(|s| s.get("networkAccess"));
    match network {
        None | Some(Value::Null) | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) if asked(crate::codex::NETWORK_KEY) == Some(&json!(true)) => {}
        Some(other) => {
            return Err(format!(
                "the thread's sandbox reports networkAccess {} where twr asked for none; \
                 commands may reach the network (a Codex config's \
                 [sandbox_workspace_write] network_access?)",
                compact(other)
            ))
        }
    }
    let asked_roots: Vec<&Path> = asked(crate::codex::WRITABLE_ROOTS_KEY)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(Path::new)
        .collect();
    match sandbox.and_then(|s| s.get("writableRoots")) {
        None | Some(Value::Null) => Ok(()),
        Some(Value::Array(roots)) => {
            for root in roots {
                let known = root
                    .as_str()
                    .is_some_and(|r| asked_roots.iter().any(|a| *a == Path::new(r)));
                if !known {
                    return Err(format!(
                        "the thread's sandbox reports the writable root {} that twr did not ask \
                         for; commands may write there (a Codex config's \
                         [sandbox_workspace_write] writable_roots?)",
                        compact(root)
                    ));
                }
            }
            Ok(())
        }
        Some(other) => Err(format!(
            "the thread's sandbox reports writableRoots {} that twr cannot check",
            compact(other)
        )),
    }
}

/// `edit N files: a, b` for a `fileChange` item's changes (paths relative to `cwd` when under
/// it). No diffs.
pub fn file_change_summary(changes: &[Value], cwd: Option<&str>) -> String {
    let names: Vec<String> = changes
        .iter()
        .map(|c| {
            let path = str_at(c, "path");
            let rel = cwd
                .filter(|d| !d.is_empty())
                .and_then(|d| path.strip_prefix(d.trim_end_matches('/')))
                .and_then(|r| r.strip_prefix('/'))
                .unwrap_or(path);
            rel.to_string()
        })
        .collect();
    let n = names.len();
    let noun = if n == 1 { "file" } else { "files" };
    let mut listed = names
        .iter()
        .take(SUMMARY_FILES_MAX)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if n > SUMMARY_FILES_MAX {
        listed.push_str(&format!(" and {} more", n - SUMMARY_FILES_MAX));
    }
    one_line(&format!("edit {n} {noun}: {listed}"), INPUT_SUMMARY_MAX)
}

/// A requested permission profile split into what twr recognizes (and shows) and what it
/// does not ([`permission_grant`]).
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionGrant {
    /// The recognized part of the profile: exactly what an `allow` grants.
    pub profile: Value,
    /// What the recognized part allows, one entry per kind (`network access`, `write a, b`),
    /// every path listed.
    pub parts: Vec<String>,
    /// What twr does not recognize (key paths, unknown values): with any, only `deny` is
    /// offered.
    pub unrecognized: Vec<String>,
}

/// The longest summary of a permission or file-change approval, in chars. A permissions
/// grant (or a `grantRoot`, marked like a command) whose summary would be longer (so the user
/// could not see all of what an `allow` grants) offers only `deny`. A command approval shows
/// its command (marked, `render_command`) cut to `GRANT_SUMMARY_MAX - 100` chars (400 after a
/// `grantRoot`), saying how much is left, with the whole command in the item's context
/// (`ext.codex.full_input`); one too long even for that offers only `deny`.
pub const GRANT_SUMMARY_MAX: usize = 1000;

/// Splits a requested permission profile (`{network: {enabled}, fileSystem: {read, write,
/// entries}}`) into what twr recognizes and what it does not. Recognized: `network.enabled`
/// (a bool), `fileSystem.read` / `.write` (arrays of path strings) and `fileSystem.entries`
/// (`{access: read|write, path: {type: path, path} | {type: glob_pattern, pattern} | {type:
/// special, value: {kind: project_roots [, subpath] | tmpdir | slash_tmp | root}}}`); null
/// values count as absent. Paths are shown relative to `cwd` when under it.
pub fn permission_grant(requested: &Value, cwd: Option<&str>) -> PermissionGrant {
    let rel = |path: &str| -> String {
        cwd.filter(|d| !d.is_empty())
            .and_then(|d| path.strip_prefix(d.trim_end_matches('/')))
            .and_then(|r| r.strip_prefix('/'))
            .filter(|r| !r.is_empty())
            .unwrap_or(path)
            .to_string()
    };
    let mut unrecognized = Vec::new();
    let top = fields(
        requested,
        "permissions",
        &mut unrecognized,
        &["network", "fileSystem"],
    );
    let mut profile = Map::new();
    let mut parts = Vec::new();
    let mut by_access: Vec<(&'static str, Vec<String>)> = vec![("write", vec![]), ("read", vec![])];
    let mut fs_out = Map::new();
    for (k, v) in top {
        if k == "network" {
            for (nk, nv) in fields(&v, "network", &mut unrecognized, &["enabled"]) {
                match nv.as_bool() {
                    Some(true) => {
                        profile.insert("network".into(), json!({nk: true}));
                        parts.push("network access".to_string());
                    }
                    Some(false) => {}
                    None => unrecognized.push("network.enabled".into()),
                }
            }
            continue;
        }
        for (fk, fv) in fields(
            &v,
            "fileSystem",
            &mut unrecognized,
            &["read", "write", "entries"],
        ) {
            let Some(items) = fv.as_array() else {
                unrecognized.push(format!("fileSystem.{fk}"));
                continue;
            };
            if fk == "entries" {
                let mut kept = Vec::new();
                for e in items {
                    let shown = entry_shown(e, &rel);
                    match (str_at(e, "access"), shown) {
                        (access @ ("read" | "write"), Some(shown)) => {
                            let slot = by_access.iter_mut().find(|(a, _)| *a == access);
                            slot.expect("both accesses listed").1.push(shown);
                            kept.push(e.clone());
                        }
                        _ => unrecognized.push(format!("fileSystem.entries: {}", compact(e))),
                    }
                }
                fs_out.insert(fk, Value::Array(kept));
                continue;
            }
            let mut kept = Vec::new();
            for p in items {
                match p.as_str().filter(|p| !p.is_empty()) {
                    Some(path) => {
                        let slot = by_access.iter_mut().find(|(a, _)| *a == fk);
                        slot.expect("read and write listed").1.push(rel(path));
                        kept.push(p.clone());
                    }
                    None => unrecognized.push(format!("fileSystem.{fk}: {}", compact(p))),
                }
            }
            fs_out.insert(fk, Value::Array(kept));
        }
    }
    if !fs_out.is_empty() {
        profile.insert("fileSystem".into(), Value::Object(fs_out));
    }
    for (access, paths) in by_access {
        if !paths.is_empty() {
            parts.push(format!("{access} {}", paths.join(", ")));
        }
    }
    PermissionGrant {
        profile: Value::Object(profile),
        parts,
        unrecognized,
    }
}

/// The non-null entries of object `v` (at key path `at`) whose keys are `known`; every other
/// key (or a non-object `v`) is added to `unrecognized`.
fn fields(
    v: &Value,
    at: &str,
    unrecognized: &mut Vec<String>,
    known: &[&str],
) -> Vec<(String, Value)> {
    match v {
        Value::Null => Vec::new(),
        Value::Object(m) => m
            .iter()
            .filter(|(_, x)| !x.is_null())
            .filter_map(|(k, x)| {
                if known.contains(&k.as_str()) {
                    Some((k.clone(), x.clone()))
                } else {
                    unrecognized.push(format!("{at}.{k}"));
                    None
                }
            })
            .collect(),
        _ => {
            unrecognized.push(at.to_string());
            Vec::new()
        }
    }
}

/// How a recognized `fileSystem.entries` path shows in a summary; None when twr does not
/// recognize it (the entry is then unrecognized).
fn entry_shown(e: &Value, rel: &dyn Fn(&str) -> String) -> Option<String> {
    let only = |v: &Value, keys: &[&str]| {
        v.as_object().is_some_and(|m| {
            m.iter()
                .all(|(k, x)| x.is_null() || keys.contains(&k.as_str()))
        })
    };
    if !only(e, &["access", "path"]) {
        return None;
    }
    let path = &e["path"];
    let text = |key: &str| {
        path.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    match str_at(path, "type") {
        "path" if only(path, &["type", "path"]) => text("path").map(rel),
        "glob_pattern" if only(path, &["type", "pattern"]) => {
            text("pattern").map(|p| format!("files matching {p}"))
        }
        "special" if only(path, &["type", "value"]) => {
            let v = &path["value"];
            let sub = v.get("subpath").and_then(Value::as_str);
            match (v.get("kind").and_then(Value::as_str), sub) {
                (Some("project_roots"), None) if only(v, &["kind"]) => Some("the project".into()),
                (Some("project_roots"), Some(s)) if only(v, &["kind", "subpath"]) => {
                    Some(format!("the project's {s}"))
                }
                (Some("tmpdir" | "slash_tmp"), None) if only(v, &["kind"]) => {
                    Some("temp files".into())
                }
                (Some("root"), None) if only(v, &["kind"]) => {
                    Some("everything under / (the whole file system)".into())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// `network access, write a, b, read c` for a requested permission profile: the recognized
/// part ([`permission_grant`]), every path listed. Empty when nothing recognizable is asked
/// for.
pub fn permissions_summary(requested: &Value, cwd: Option<&str>) -> String {
    permission_grant(requested, cwd).parts.join(", ")
}

/// The tool call a tool-like item stands for, or None for other items.
fn tool_call(item: &Value, cwd: Option<&str>) -> Option<AgentToolCall> {
    let id = str_at(item, "id").to_string();
    let mut codex = Map::new();
    let kind = str_at(item, "type");
    codex.insert("item_type".into(), json!(kind));
    let (tool, input_summary, risky) = match kind {
        "commandExecution" => {
            let summary = command_summary(item);
            let read_only = item["commandActions"].as_array().is_some_and(|a| {
                !a.is_empty()
                    && a.iter()
                        .all(|x| matches!(str_at(x, "type"), "read" | "listFiles" | "search"))
            });
            let destructive = is_destructive(&item["command"]);
            if destructive {
                codex.insert("destructive".into(), json!(true));
            }
            if let Some(cwd) = opt_str(item, "cwd") {
                codex.insert("cwd".into(), json!(cwd));
            }
            ("shell".to_string(), summary, !read_only || destructive)
        }
        "fileChange" => {
            let changes = item["changes"].as_array().cloned().unwrap_or_default();
            (
                "apply_patch".to_string(),
                file_change_summary(&changes, cwd),
                true,
            )
        }
        "mcpToolCall" => {
            let server = str_at(item, "server");
            let name = str_at(item, "tool");
            let args = compact(&item["arguments"]);
            let read_only = item.get("readOnlyHint").and_then(Value::as_bool) == Some(true);
            (format!("mcp__{server}__{name}"), args, !read_only)
        }
        "dynamicToolCall" => {
            let name = str_at(item, "tool");
            let tool = match opt_str(item, "namespace") {
                Some(ns) => format!("{ns}.{name}"),
                None => name.to_string(),
            };
            (tool, compact(&item["arguments"]), true)
        }
        "webSearch" => (
            "web_search".to_string(),
            one_line(str_at(item, "query"), INPUT_SUMMARY_MAX),
            false,
        ),
        _ => return None,
    };
    Some(AgentToolCall {
        call_id: id,
        tool,
        input_summary,
        input_ref: None,
        risky,
        ext: ext_of(codex),
        ..Default::default()
    })
}

fn compact(v: &Value) -> String {
    let text = match v {
        Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    one_line(&text, INPUT_SUMMARY_MAX)
}

/// The result of a completed tool-like item.
fn tool_result(item: &Value) -> AgentToolResult {
    let status = str_at(item, "status");
    let mut codex = Map::new();
    codex.insert("status".into(), json!(status));
    let (ok, text) = match str_at(item, "type") {
        "commandExecution" => {
            let exit = item.get("exitCode").and_then(Value::as_i64);
            if let Some(c) = exit {
                codex.insert("exit_code".into(), json!(c));
            }
            if let Some(d) = item.get("durationMs").and_then(Value::as_u64) {
                codex.insert("duration_ms".into(), json!(d));
            }
            let out = str_at(item, "aggregatedOutput").to_string();
            let ok = status == "completed" && exit.unwrap_or(0) == 0;
            let text = if out.trim().is_empty() && status != "completed" {
                status.to_string()
            } else {
                out
            };
            (ok, text)
        }
        "fileChange" => (status == "completed", status.to_string()),
        "mcpToolCall" => {
            let err = item.pointer("/error/message").and_then(Value::as_str);
            let text = match err {
                Some(e) => e.to_string(),
                None => item
                    .pointer("/result/content")
                    .and_then(Value::as_array)
                    .map(|blocks| {
                        blocks
                            .iter()
                            .map(|b| match str_at(b, "type") {
                                "text" => str_at(b, "text").to_string(),
                                "" => String::new(),
                                other => format!("[{other}]"),
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default(),
            };
            (status == "completed" && err.is_none(), text)
        }
        "dynamicToolCall" => (
            status == "completed" && item.get("success").and_then(Value::as_bool) != Some(false),
            status.to_string(),
        ),
        _ => (
            status != "failed" && status != "declined",
            status.to_string(),
        ),
    };
    AgentToolResult {
        call_id: str_at(item, "id").to_string(),
        ok,
        output_summary: one_line(&text, OUTPUT_SUMMARY_MAX),
        output_ref: None,
        ext: ext_of(codex),
        ..Default::default()
    }
}

/// Appended to an error that says Codex is not signed in (or its login expired).
pub const AUTH_HINT: &str = " (sign in again: run `codex login`)";

/// Whether a turn error (`{message, codexErrorInfo, additionalDetails}`) says Codex could not
/// authenticate: `codexErrorInfo` `unauthorized`, any variant carrying `httpStatusCode` 401,
/// or a message saying so (see the module docs for what Codex sends).
pub fn auth_failure(error: &Value) -> bool {
    let status_401 = |m: &Map<String, Value>| {
        m.values()
            .any(|v| v.get("httpStatusCode").and_then(Value::as_u64) == Some(401))
    };
    match error.get("codexErrorInfo") {
        Some(Value::String(s)) if s == "unauthorized" => return true,
        Some(Value::Object(m)) if status_401(m) => return true,
        _ => {}
    }
    const PATTERNS: &[&str] = &[
        "401 unauthorized",
        "unauthorized (401)",
        "could not be refreshed",
        "sign in again",
        "log in again",
        "not logged in",
        "codex login",
        "invalid_api_key",
        "incorrect api key",
    ];
    ["message", "additionalDetails"].iter().any(|k| {
        error.get(*k).and_then(Value::as_str).is_some_and(|t| {
            let t = t.to_lowercase();
            PATTERNS.iter().any(|p| t.contains(p))
        })
    })
}

/// The cause a turn error (`{message, codexErrorInfo, additionalDetails}`) reports: its
/// `message` (an API error body Codex quotes as JSON, as Codex 0.159.2 does for a model the
/// account cannot use, is reduced to its message and status), else `additionalDetails`, else
/// a readable `codexErrorInfo` (`{"httpConnectionFailed": {"httpStatusCode": 502}}` →
/// "Http connection failed (HTTP 502)").
pub fn error_text(error: &Value) -> Option<String> {
    let text = |k: &str| {
        error
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    if let Some(msg) = text("message") {
        return Some(api_error_body(msg).unwrap_or_else(|| msg.to_string()));
    }
    if let Some(details) = text("additionalDetails") {
        return Some(details.to_string());
    }
    let (name, detail) = match error.get("codexErrorInfo")? {
        Value::String(s) => (s.as_str(), None),
        Value::Object(m) => {
            let (name, detail) = m.iter().next()?;
            (name.as_str(), Some(detail))
        }
        _ => return None,
    };
    let mut words = String::new();
    for (i, c) in name.chars().enumerate() {
        if i == 0 {
            words.extend(c.to_uppercase());
        } else if c.is_uppercase() {
            words.push(' ');
            words.extend(c.to_lowercase());
        } else {
            words.push(c);
        }
    }
    if words.is_empty() {
        return None;
    }
    if let Some(code) = detail
        .and_then(|d| d.get("httpStatusCode"))
        .and_then(Value::as_u64)
    {
        words.push_str(&format!(" (HTTP {code})"));
    }
    Some(words)
}

/// `message (HTTP status)` of an API error body quoted as JSON
/// (`{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"..."}}`).
fn api_error_body(msg: &str) -> Option<String> {
    if !msg.starts_with('{') {
        return None;
    }
    let body: Value = serde_json::from_str(msg).ok()?;
    let inner = ["/error/message", "/message"]
        .iter()
        .find_map(|p| body.pointer(p).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(match body.get("status").and_then(Value::as_u64) {
        Some(code) => format!("{inner} (HTTP {code})"),
        None => inner.to_string(),
    })
}

/// `text` with [`AUTH_HINT`] when it says Codex is not signed in.
fn with_auth_hint(text: String) -> String {
    if auth_failure(&json!({ "message": text })) {
        format!("{text}{AUTH_HINT}")
    } else {
        text
    }
}

/// Why a failed turn looks transient (`turn.error.codexErrorInfo`), if it does.
pub fn transient_reason(error: &Value) -> Option<String> {
    let info = error.get("codexErrorInfo")?;
    let name = match info {
        Value::String(s) => s.clone(),
        Value::Object(m) => m.keys().next()?.clone(),
        _ => return None,
    };
    TRANSIENT_ERRORS
        .contains(&name.as_str())
        .then(|| format!("codexErrorInfo {name}"))
}

/// The text of one answer to a question with option `labels`, or None to decline.
pub(crate) fn question_answer(
    question: &str,
    labels: &[String],
    answer: &Answer,
) -> Option<String> {
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
    (!text.is_empty()).then(|| text.to_string())
}

/// The steer text answering async questions: the answer alone for one question, else one
/// `question: answer` line per question.
fn steer_text(questions: &[String], answers: &[String]) -> String {
    if answers.len() == 1 {
        return answers[0].clone();
    }
    questions
        .iter()
        .zip(answers)
        .map(|(q, a)| format!("{q}: {a}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_unwrapping() {
        assert_eq!(unwrap_shell("/bin/bash -lc 'touch a.txt'"), "touch a.txt");
        assert_eq!(
            unwrap_shell("/bin/bash -lc 'echo '\\''hi'\\'''"),
            "echo 'hi'"
        );
        assert_eq!(unwrap_shell("ls -la"), "ls -la");
        // Not one quoted word: shown as given, so nothing is joined that runs apart.
        assert_eq!(
            unwrap_shell("/bin/bash -lc 'echo a' 'rm -rf ~'"),
            "/bin/bash -lc 'echo a' 'rm -rf ~'"
        );
        assert_eq!(
            unwrap_shell("bash -lc \"echo \\\"$(id)\\\"\""),
            "bash -lc \"echo \\\"$(id)\\\"\""
        );
        assert_eq!(unwrap_shell("bash -lc \"ls -la\""), "ls -la");
        // Nothing trimmed.
        assert_eq!(unwrap_shell("echo value\\ "), "echo value\\ ");
        assert_eq!(unwrap_shell("bash -lc 'echo value\\ '"), "echo value\\ ");
    }

    #[test]
    fn file_change_summaries() {
        let ch = |p: &str| json!({"path": p, "kind": {"type": "add"}, "diff": "x"});
        assert_eq!(
            file_change_summary(&[ch("/w/a.txt")], Some("/w")),
            "edit 1 file: a.txt"
        );
        assert_eq!(
            file_change_summary(&[ch("/w/a"), ch("/x/b")], Some("/w/")),
            "edit 2 files: a, /x/b"
        );
        let many: Vec<Value> = (0..6).map(|i| ch(&format!("/w/f{i}"))).collect();
        assert_eq!(
            file_change_summary(&many, Some("/w")),
            "edit 6 files: f0, f1, f2, f3 and 2 more"
        );
    }

    #[test]
    fn transient_errors() {
        assert!(transient_reason(&json!({"codexErrorInfo": "serverOverloaded"})).is_some());
        assert!(transient_reason(
            &json!({"codexErrorInfo": {"responseStreamDisconnected": {"httpStatusCode": 502}}})
        )
        .is_some());
        assert!(transient_reason(&json!({"codexErrorInfo": "unauthorized"})).is_none());
        assert!(transient_reason(&Value::Null).is_none());
    }

    /// A command approval shows the whole command (a tool call's summary is cut at
    /// [`INPUT_SUMMARY_MAX`]), with the runtime's reason and directory in `ext.codex`.
    #[test]
    fn command_approvals_show_the_whole_command() {
        let mut t = Translator::new(
            TranslatorOptions::default(),
            Open {
                method: "thread/start",
                params: json!({}),
                mode_note: None,
            },
        );
        let cmd = format!("cargo test --workspace -- {}", "--flag ".repeat(60));
        let req = json!({"method": "item/commandExecution/requestApproval", "id": 3,
            "params": {"itemId": "i1", "command": cmd, "cwd": "/w", "reason": "needs the net"}});
        let out = t.translate(&req.to_string());
        let Some(EventBody::AttentionRaised(a)) = out.events.first().map(|e| &e.body) else {
            panic!("{out:?}")
        };
        assert!(cmd.chars().count() > INPUT_SUMMARY_MAX);
        // Its trailing space shows too.
        assert_eq!(
            a.summary,
            format!("Allow shell: {}⟨U+0020⟩", cmd.trim_end())
        );
        let codex = &a.ext.as_ref().unwrap()["codex"];
        assert_eq!(
            (codex["reason"].as_str(), codex["cwd"].as_str()),
            (Some("needs the net"), Some("/w"))
        );
    }

    #[test]
    fn unsupported_server_requests_get_an_error() {
        let mut t = Translator::new(
            TranslatorOptions::default(),
            Open {
                method: "thread/start",
                params: json!({}),
                mode_note: None,
            },
        );
        let out = t.translate(r#"{"method":"item/tool/call","id":7,"params":{}}"#);
        // Never silent: recorded as an item raised and closed at once.
        match &out.events[..] {
            [Emitted {
                body: EventBody::AttentionRaised(a),
                ..
            }, Emitted {
                body: EventBody::AttentionResolved(r),
                ..
            }] => {
                assert_eq!(a.reason, AttentionReason::Error);
                assert!(a.summary.contains("item/tool/call"), "{}", a.summary);
                assert_eq!(r.attn_id, a.attn_id);
                assert_eq!(r.by, ResolvedBy::Timeout);
            }
            other => panic!("{other:?}"),
        }
        let r: Value = serde_json::from_str(&out.replies[0]).unwrap();
        assert_eq!(r["id"], 7);
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        assert!(t.translate("not json").events.is_empty());
    }

    fn open_translator() -> Translator {
        let mut t = Translator::new(
            TranslatorOptions::default(),
            Open {
                method: "thread/start",
                params: json!({"approvalPolicy": "untrusted", "approvalsReviewer": "user",
                    "sandbox": "workspace-write"}),
                mode_note: None,
            },
        );
        t.start();
        t.translate(r#"{"id":1,"result":{}}"#);
        let out = t.translate(
            r#"{"id":2,"result":{"thread":{"id":"th"},"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite","networkAccess":false}}}"#,
        );
        assert!(out.fatal.is_none(), "{out:?}");
        t
    }

    /// The policy Codex reports for the opened thread (start and resume)
    /// must be no wider than requested, with the reviewer exactly `user`; otherwise the
    /// session ends before any prompt is dispatched.
    #[test]
    fn a_thread_reporting_a_wider_policy_never_runs() {
        let open = |method: &'static str, result: Value| {
            let mut t = Translator::new(
                TranslatorOptions::default(),
                Open {
                    method,
                    params: json!({"threadId": "th", "approvalPolicy": "untrusted",
                        "approvalsReviewer": "user", "sandbox": "read-only"}),
                    mode_note: None,
                },
            );
            t.start();
            assert!(t.send("go", 1).is_empty());
            t.translate(r#"{"id":1,"result":{}}"#);
            let mut r = json!({"thread": {"id": "th"}});
            r.as_object_mut()
                .unwrap()
                .extend(result.as_object().unwrap().clone());
            let line = json!({"id": 2, "result": r}).to_string();
            (t.translate(&line), t)
        };
        let ok = json!({"approvalPolicy": "untrusted", "approvalsReviewer": "user",
            "sandbox": {"type": "readOnly"}});
        for method in ["thread/start", "thread/resume"] {
            let (out, t) = open(method, ok.clone());
            assert!(out.fatal.is_none(), "{out:?}");
            assert!(t.busy() && out.replies.iter().any(|l| l.contains("turn/start")));
            let mut bad = Vec::new();
            for (k, v) in [
                ("approvalPolicy", json!("never")),
                ("approvalPolicy", json!("on-request")),
                ("approvalPolicy", json!({"granular": {}})),
                ("approvalPolicy", Value::Null),
                ("sandbox", json!({"type": "workspaceWrite"})),
                ("sandbox", json!({"type": "dangerFullAccess"})),
                ("sandbox", json!({"type": "externalSandbox"})),
                ("sandbox", Value::Null),
                ("approvalsReviewer", json!("auto_review")),
                ("approvalsReviewer", Value::Null),
            ] {
                let mut r = ok.clone();
                r[k] = v.clone();
                bad.push((k, v, r));
            }
            for (k, v, r) in bad {
                let (out, t) = open(method, r);
                let why = out
                    .fatal
                    .unwrap_or_else(|| panic!("{method} {k}={v} accepted"));
                assert!(why.contains(k), "{why}");
                assert!(out.events.is_empty(), "no thread.started: {:?}", out.events);
                assert!(
                    !out.replies.iter().any(|l| l.contains("turn/start")),
                    "no prompt dispatched"
                );
                assert!(t.thread_id().is_none());
            }
        }
        // Narrower than requested is fine.
        let mut t = Translator::new(
            TranslatorOptions::default(),
            Open {
                method: "thread/start",
                params: json!({"approvalPolicy": "never", "approvalsReviewer": "user",
                    "sandbox": "workspace-write"}),
                mode_note: None,
            },
        );
        t.start();
        t.translate(r#"{"id":1,"result":{}}"#);
        let out = t.translate(
            r#"{"id":2,"result":{"thread":{"id":"th"},"approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"readOnly"}}}"#,
        );
        assert!(out.fatal.is_none(), "{out:?}");
    }

    /// Codex's reviewer agent answers only where twr asked for it (`[codex] auto =
    /// "auto_review"`): then `auto_review`, its older name `guardian_subagent`, or `user`
    /// (narrower) pass; asked for `user`, only `user` does.
    #[test]
    fn the_auto_reviewer_is_accepted_only_when_asked_for() {
        let params = |reviewer: &str| {
            json!({"approvalPolicy": "on-request", "approvalsReviewer": reviewer,
                "sandbox": "read-only"})
        };
        let result = |reviewer: Value| {
            json!({"approvalPolicy": "on-request", "approvalsReviewer": reviewer,
                "sandbox": {"type": "readOnly"}})
        };
        for got in ["auto_review", "guardian_subagent", "user"] {
            assert_eq!(
                check_policy(&params("auto_review"), &result(json!(got))),
                Ok(()),
                "{got}"
            );
        }
        for got in [
            json!("auto_review"),
            json!("guardian_subagent"),
            json!("someone"),
            Value::Null,
        ] {
            let why = check_policy(&params("user"), &result(got.clone())).unwrap_err();
            assert!(why.contains("approvalsReviewer"), "{why}");
        }
        for got in [json!("someone"), Value::Null] {
            assert!(check_policy(&params("auto_review"), &result(got)).is_err());
        }
    }

    /// Each decision of Codex's reviewer agent (as Codex 0.162.1 sent them) is in the
    /// transcript: the action as an `auto_review` tool call, the decision as its result, `ok`
    /// only when approved. The `started` notification and the `guardianWarning` repeating the
    /// decision add nothing.
    #[test]
    fn auto_review_decisions_are_recorded_as_tool_calls() {
        let mut t = open_translator();
        t.send("go", 1);
        t.translate(r#"{"id":3,"result":{"turn":{"id":"tu1"}}}"#);
        let started = r#"{"method":"item/autoApprovalReview/started","params":{"threadId":"th","turnId":"tu1","startedAtMs":1,"reviewId":"r1","targetItemId":"exec-1","review":{"status":"inProgress","riskLevel":null,"userAuthorization":null,"rationale":null},"action":{"type":"command","source":"unifiedExec","command":"/bin/bash -lc 'git branch -d other'","cwd":"/w"}}}"#;
        assert!(t.translate(started).events.is_empty());
        let warning = r#"{"method":"guardianWarning","params":{"threadId":"th","message":"Automatic approval review approved (risk: medium, authorization: high): ..."}}"#;
        assert!(t.translate(warning).events.is_empty());
        let decided = |id: &str, status: &str, risk: &str| {
            json!({"method": "item/autoApprovalReview/completed", "params": {
                "threadId": "th", "turnId": "tu1", "startedAtMs": 1, "completedAtMs": 2,
                "reviewId": id, "targetItemId": "exec-1", "decisionSource": "agent",
                "review": {"status": status, "riskLevel": risk, "userAuthorization": "high",
                    "rationale": "The user asked for it."},
                "action": {"type": "command", "source": "unifiedExec",
                    "command": "/bin/bash -lc 'git branch -d other'", "cwd": "/w"}}})
            .to_string()
        };
        for (id, status, ok) in [("r1", "approved", true), ("r2", "denied", false)] {
            let out = t.translate(&decided(id, status, "medium"));
            let (call, result) = match &out.events[..] {
                [Emitted {
                    body: EventBody::AgentToolCall(c),
                    turn: Some(1),
                    ..
                }, Emitted {
                    body: EventBody::AgentToolResult(r),
                    turn: Some(1),
                    ..
                }] => (c, r),
                other => panic!("{other:?}"),
            };
            assert_eq!(call.tool, "auto_review");
            assert_eq!(call.call_id, format!("auto_review:{id}"));
            assert_eq!(call.input_summary, "/bin/bash -lc 'git branch -d other'");
            assert_eq!(call.risky, ok);
            assert_eq!(
                call.ext.as_ref().unwrap()["codex"]["target_item_id"],
                "exec-1"
            );
            assert_eq!(result.call_id, call.call_id);
            assert_eq!(result.ok, ok);
            assert_eq!(
                result.output_summary,
                format!("{status} (risk medium, authorization high): The user asked for it.")
            );
            assert_eq!(
                result.ext.as_ref().unwrap()["codex"]["review"]["status"],
                status
            );
        }
        // An action that is not a command shows as JSON.
        let other = json!({"method": "item/autoApprovalReview/completed", "params": {
            "threadId": "th", "turnId": "tu1", "reviewId": "r3",
            "review": {"status": "timedOut"}, "action": {"type": "network", "host": "x.dev"}}})
        .to_string();
        let out = t.translate(&other);
        let [Emitted {
            body: EventBody::AgentToolCall(c),
            ..
        }, Emitted {
            body: EventBody::AgentToolResult(r),
            ..
        }] = &out.events[..]
        else {
            panic!("{:?}", out.events)
        };
        assert!(c.input_summary.contains("x.dev"), "{}", c.input_summary);
        assert!(!r.ok);
        assert_eq!(r.output_summary, "timedOut");
    }

    /// Relay rule: a form or URL elicitation is never answered for the user. It
    /// becomes a question offering decline / cancel; the reply goes out only on an answer.
    #[test]
    fn form_and_url_elicitations_wait_for_the_user() {
        let mut t = open_translator();
        t.send("go", 1);
        t.translate(r#"{"id":3,"result":{"turn":{"id":"tu1"}}}"#);
        let out = t.translate(
            r#"{"method":"mcpServer/elicitation/request","id":8,"params":{"threadId":"th","turnId":"tu1","serverName":"s","mode":"form","message":"name?","requestedSchema":{"type":"object","properties":{"name":{"type":"string"}}}}}"#,
        );
        assert!(out.replies.is_empty(), "no unsolicited reply: {out:?}");
        let a = match &out.events[..] {
            [Emitted {
                body: EventBody::AttentionRaised(a),
                turn: Some(1),
                ..
            }] => a.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.options, ["decline", "cancel"]);
        assert!(a.summary.contains("name?"), "{}", a.summary);
        let out = t.translate(
            r#"{"method":"mcpServer/elicitation/request","id":9,"params":{"threadId":"th","turnId":"tu1","serverName":"auth","mode":"url","message":"Sign in","url":"https://example.com/x","elicitationId":"e"}}"#,
        );
        assert!(out.replies.is_empty(), "no unsolicited reply: {out:?}");
        let b = match &out.events[..] {
            [Emitted {
                body: EventBody::AttentionRaised(b),
                ..
            }] => b.clone(),
            other => panic!("{other:?}"),
        };
        assert!(b.summary.contains("https://example.com/x"), "{}", b.summary);
        assert_eq!(t.pending_attention().len(), 2);
        // Only the user's answer produces the reply: decline (even for allow) or cancel.
        let r = t.respond(&a.attn_id, &Answer::allow()).unwrap();
        let r: Value = serde_json::from_str(&r.replies[0]).unwrap();
        assert_eq!(
            (r["id"].clone(), r["result"]["action"].clone()),
            (json!(8), json!("decline"))
        );
        let r = t.respond(&b.attn_id, &Answer::new("cancel")).unwrap();
        let r: Value = serde_json::from_str(&r.replies[0]).unwrap();
        assert_eq!(
            (r["id"].clone(), r["result"]["action"].clone()),
            (json!(9), json!("cancel"))
        );
        assert!(t.pending_attention().is_empty());
    }

    /// An unreadable `turn/completed` still ends the turn; per-turn maps
    /// are pruned at the turn's end, and the id maps are bounded.
    #[test]
    fn oversized_turn_end_and_pruned_maps() {
        let mut t = open_translator();
        t.send("go", 1);
        t.translate(r#"{"id":3,"result":{"turn":{"id":"tu1"}}}"#);
        t.translate(r#"{"method":"item/started","params":{"turnId":"tu1","item":{"type":"commandExecution","id":"c1","command":"ls","commandActions":[]}}}"#);
        assert_eq!(t.items.len(), 1);
        assert!(t
            .oversized(r#"{"method":"item/completed","params""#, 9)
            .events
            .is_empty());
        let out = t.oversized(
            r#"{"method":"turn/completed","params":{"turn":{"id":"tu1""#,
            50_000_000,
        );
        match &out.events[..] {
            [Emitted {
                body: EventBody::TurnEnded(e),
                turn: Some(1),
                ..
            }] => {
                assert_eq!(e.result, TurnResult::Error);
                assert_eq!(e.ext.as_ref().unwrap()["twr"]["error"], OVERSIZED_ERROR);
            }
            other => panic!("{other:?}"),
        }
        assert!(!t.busy());
        assert!(t.items.is_empty());
        // Turn ids and attention ids stay bounded however long the process runs.
        for i in 0..(TURN_IDS_KEPT as u64 + 10) {
            t.turn_ids.insert_new(format!("x{i}"), Some(i));
        }
        assert_eq!(t.turn_ids.len(), TURN_IDS_KEPT);
        assert!(!t.turn_ids.contains_key("x0"));
        for i in 0..(ATTN_IDS_KEPT + 10) {
            t.attn_id(format!("a{i}"));
        }
        assert_eq!(t.used_attn_ids.len(), ATTN_IDS_KEPT);
        // A still-pending id is never handed out again, even once forgotten.
        t.pending.push(Pending {
            request_id: None,
            turn_id: None,
            waiting: Waiting::Decision { allow: true },
            attn_ids: vec!["keep".into()],
            questions: vec![String::new()],
            labels: vec![Vec::new()],
            answers: vec![None],
        });
        assert_eq!(t.attn_id("keep".into()), "keep#2");
    }
}

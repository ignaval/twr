//! The event model: envelope, typed payloads and (de)serialization.
//!
//! Every event serializes as one JSON object:
//!
//! ```json
//! {"v":1,"id":"01H...","seq":3,"ts":"2026-09-29T12:00:00.000Z","thread":"t1","turn":2,
//!  "source":"adapter:fake","kind":"agent.text","data":{...},"raw":null}
//! ```
//!
//! `kind` + `data` come from [`EventBody`]. Kinds this build does not know deserialize into
//! [`EventBody::Unknown`] so newer logs stay readable. Payload structs keep unknown keys (for
//! example `<field>_ref` siblings and the `_capped` marker added by
//! [`crate::blob::cap_payload`]) in their `extra` map.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, SubsecRound, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

/// Current envelope schema version (`v`).
pub const SCHEMA_VERSION: u32 = 1;

/// Reserved thread id for hub-scope events (runner.*, pre-target route decisions, hub questions).
/// It never appears in thread lists.
pub const HUB_THREAD: &str = "hub";

/// `ext.twr` key of a confirm card: the hub's own question on [`HUB_THREAD`] that lists what
/// the orchestrator proposed from the user's message (answers and mode changes the hub could
/// not match to a short command), for the user to confirm or cancel. Its value holds the
/// actions (`twr-hub` `hub::proposal`). Only the user answers a confirm card.
pub const PROPOSAL_KEY: &str = "proposal";

/// `ext.twr` key of a delete card: the hub's own question on [`HUB_THREAD`] asking the user to
/// confirm deleting one or more threads (`thread.delete`): for each, its runtime, its worktree
/// (even with uncommitted, hidden or ignored files), its `twr/<name>` branch and the thread
/// itself go. Its value is `{threads: [{thread, name}], losses: [{thread, pending, loss}],
/// asked_by, replaces?}`: the threads by id and name, in order, what the card says each one
/// loses (as recorded), who asked, and the card it replaces (`twr-hub` `hub::delete`). Like a
/// confirm card ([`is_confirm_card`]), only the user answers it.
pub const DELETE_KEY: &str = "delete";

/// Whether an `attention.raised` on `thread` with `ext` is a card only the user answers: a
/// confirm card ([`PROPOSAL_KEY`]) or a delete card ([`DELETE_KEY`]).
pub fn is_confirm_card(thread: &str, ext: &Option<JsonMap>) -> bool {
    let twr = ext.as_ref().and_then(|e| e.get("twr"));
    thread == HUB_THREAD
        && [PROPOSAL_KEY, DELETE_KEY]
            .iter()
            .any(|k| twr.and_then(|t| t.get(*k)).is_some_and(|v| !v.is_null()))
}

/// `attention.raised` `ext.twr` key of a hung-turn card (`twr-hub` `hub::stall`): a stalled item
/// the hub raises when a turn has been silent while something waits to start the next one,
/// `{turn, waiting}`, with the options [`HUNG_INTERRUPT`] and [`HUNG_WAIT`].
pub const HUNG_KEY: &str = "hung";
/// A hung-turn card's option that interrupts the turn so that what waits runs.
pub const HUNG_INTERRUPT: &str = "interrupt";
/// A hung-turn card's option that leaves the turn running.
pub const HUNG_WAIT: &str = "wait";

/// Whether an `attention.raised` with `ext` is a hung-turn card ([`HUNG_KEY`]).
pub fn is_hung_card(ext: &Option<JsonMap>) -> bool {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(HUNG_KEY))
        .is_some_and(|v| !v.is_null())
}

/// `attention.resolved` `ext.twr` key of a plain stalled item the hub closed (`by: timeout`)
/// because a hung-turn card took its place, in the same batch: the card's attn_id. Its
/// thread's activity did not resume.
pub const SUPERSEDED_BY_KEY: &str = "superseded_by";

/// The item that took the place of the one an `attention.resolved` with `ext` closed
/// ([`SUPERSEDED_BY_KEY`]), if another did.
pub fn superseded_by(ext: &Option<JsonMap>) -> Option<&str> {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(SUPERSEDED_BY_KEY))
        .and_then(Value::as_str)
}

/// `attention.resolved` `ext.twr` key of a stalled item the hub closed (`by: timeout`) because
/// its thread went on working: its runtime said, did or finished something (`true`). A stall
/// closed for anything else (the thread stopped, crashed, was interrupted or parked, its turn
/// failed, the hub's startup found its work gone, a hung-turn card took its place, or someone's
/// event that is no work of the thread's) has none: a client keeps showing that one.
pub const WENT_ON_KEY: &str = "went_on";

/// Whether the `attention.resolved` with `ext` closed a stall because its thread went on
/// working ([`WENT_ON_KEY`]).
pub fn went_on(ext: &Option<JsonMap>) -> bool {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(WENT_ON_KEY))
        .and_then(Value::as_bool)
        == Some(true)
}

/// `conversation.update` `ext.twr` key of a routine update the hub mirrors into the
/// orchestrator's conversation (`twr-hub` `hub::updates`): one that tells of work going on, not
/// of something to decide. Its value says which: [`ROUTINE_INTERIM`], [`ROUTINE_BACKGROUND_DONE`]
/// or [`ROUTINE_AUTO_WAKE`]. A client may fold a thread's routine updates into one line (and a
/// summary of them, `summary.thread`); the hub drops all but the newest of a thread's from the
/// background facts it gives the orchestrator. `null`: no routine update, whatever its words
/// say (a failed turn's report, though tasks ran on after it). Updates written before it have
/// none.
pub const ROUTINE_KEY: &str = "routine";
/// [`ROUTINE_KEY`]: an interim report (its turn yielded with background tasks still running).
pub const ROUTINE_INTERIM: &str = "interim";
/// [`ROUTINE_KEY`]: "X background work finished."
pub const ROUTINE_BACKGROUND_DONE: &str = "background_done";
/// [`ROUTINE_KEY`]: the hub woke the worker after its background work, or chose not to.
pub const ROUTINE_AUTO_WAKE: &str = "auto_wake";

/// The kind of routine update a `conversation.update` with `ext` is ([`ROUTINE_KEY`]), if it
/// is one.
pub fn routine_of(ext: &Option<JsonMap>) -> Option<&str> {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(ROUTINE_KEY))
        .and_then(Value::as_str)
}

/// Whether an `attention.raised` on `thread` with `ext` is a delete card ([`DELETE_KEY`]).
pub fn is_delete_card(thread: &str, ext: &Option<JsonMap>) -> bool {
    thread == HUB_THREAD
        && ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get(DELETE_KEY))
            .is_some_and(|v| !v.is_null())
}

/// JSON object map used for `ext` and `extra`.
pub type JsonMap = Map<String, Value>;

/// Generates a new ULID string.
pub fn new_id() -> String {
    // Monotonic within the process: ids made in the same millisecond still sort in creation
    // order (threads created back to back list, and tie-break, deterministically).
    static GEN: std::sync::Mutex<ulid::Generator> = std::sync::Mutex::new(ulid::Generator::new());
    let mut g = GEN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match g.generate() {
        Ok(id) => id.to_string(),
        Err(_) => ulid::Ulid::generate().to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------------------------

/// One entry in a thread's append-only log.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Schema version, currently [`SCHEMA_VERSION`].
    pub v: u32,
    /// ULID, globally unique.
    pub id: String,
    /// Per-thread monotonic sequence number.
    pub seq: u64,
    /// Wall-clock timestamp (serialized as RFC3339 UTC with millisecond precision).
    pub ts: DateTime<Utc>,
    /// Thread id ([`HUB_THREAD`] for hub-scope events).
    pub thread: String,
    /// 1-based turn number, set on events belonging to a turn.
    pub turn: Option<u64>,
    /// Who produced the event.
    pub source: Source,
    /// `kind` + `data`.
    pub body: EventBody,
    /// Optional pointer (`sha256:<hex>`) to the stored raw runtime message.
    pub raw: Option<String>,
}

impl Event {
    /// Builds an event with a fresh id and the current time (truncated to milliseconds so it
    /// round-trips exactly through JSON).
    pub fn new(
        thread: impl Into<String>,
        seq: u64,
        turn: Option<u64>,
        source: Source,
        body: EventBody,
    ) -> Self {
        Event {
            v: SCHEMA_VERSION,
            id: new_id(),
            seq,
            ts: Utc::now().trunc_subsecs(3),
            thread: thread.into(),
            turn,
            source,
            body,
            raw: None,
        }
    }

    /// The event kind string, e.g. `agent.text`.
    pub fn kind(&self) -> &str {
        self.body.kind()
    }

    /// Whether this event belongs on the hub control stream.
    pub fn is_control(&self) -> bool {
        self.body.is_control()
    }
}

/// Owned wire shape of an [`Event`], for deserialization.
#[derive(Deserialize)]
struct WireEvent {
    v: u32,
    id: String,
    seq: u64,
    #[serde(with = "rfc3339_millis")]
    ts: DateTime<Utc>,
    thread: String,
    #[serde(default)]
    turn: Option<u64>,
    source: Source,
    kind: String,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    raw: Option<String>,
}

/// [`WireEvent`] with its `data` left as JSON text, parsed by `kind` straight into its type
/// ([`EventBody::from_kind_json`]): only from serde_json's own text deserializer
/// ([`crate::proto::ServerMessage::from_line`]), which alone has the text to lend.
#[derive(Deserialize)]
pub(crate) struct WireEventText<'a> {
    v: u32,
    id: String,
    seq: u64,
    #[serde(with = "rfc3339_millis")]
    ts: DateTime<Utc>,
    thread: String,
    #[serde(default)]
    turn: Option<u64>,
    source: Source,
    kind: String,
    #[serde(default, borrow)]
    data: Option<&'a serde_json::value::RawValue>,
    #[serde(default)]
    raw: Option<String>,
}

impl WireEventText<'_> {
    /// The event.
    pub(crate) fn into_event(self) -> Result<Event, serde_json::Error> {
        Ok(Event {
            body: EventBody::from_kind_json(&self.kind, self.data.map(|d| d.get()))?,
            v: self.v,
            id: self.id,
            seq: self.seq,
            ts: self.ts,
            thread: self.thread,
            turn: self.turn,
            source: self.source,
            raw: self.raw,
        })
    }
}

/// Borrowed wire shape used for serialization (keeps payload field order).
#[derive(Serialize)]
struct WireEventRef<'a> {
    v: u32,
    id: &'a str,
    seq: u64,
    #[serde(with = "rfc3339_millis")]
    ts: &'a DateTime<Utc>,
    thread: &'a str,
    turn: Option<u64>,
    source: &'a Source,
    kind: &'a str,
    data: DataOf<'a>,
    raw: &'a Option<String>,
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        WireEventRef {
            v: self.v,
            id: &self.id,
            seq: self.seq,
            ts: &self.ts,
            thread: &self.thread,
            turn: self.turn,
            source: &self.source,
            kind: self.body.kind(),
            data: DataOf(&self.body),
            raw: &self.raw,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let w = WireEvent::deserialize(deserializer)?;
        let body = EventBody::from_kind_data(&w.kind, w.data).map_err(serde::de::Error::custom)?;
        Ok(Event {
            v: w.v,
            id: w.id,
            seq: w.seq,
            ts: w.ts,
            thread: w.thread,
            turn: w.turn,
            source: w.source,
            body,
            raw: w.raw,
        })
    }
}

/// RFC3339 UTC with milliseconds and a `Z` suffix; accepts any RFC3339 offset on input.
mod rfc3339_millis {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ts: &&DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&ts.to_rfc3339_opts(SecondsFormat::Millis, true))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        let s = String::deserialize(d)?;
        DateTime::parse_from_rfc3339(&s)
            .map(|t| t.with_timezone(&Utc))
            .map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------------------------

/// Producer of an event. Serializes as `adapter:<name>`, `hub`, `user:<client>`,
/// `orchestrator` or `decision:<provider>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Source {
    /// A runtime adapter, e.g. `adapter:claude-code`.
    Adapter(String),
    /// The hub itself.
    Hub,
    /// A user client, e.g. `user:tui`.
    User(String),
    /// The orchestrator thread.
    Orchestrator,
    /// A decision provider, e.g. `decision:code`.
    Decision(String),
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::Adapter(n) => write!(f, "adapter:{n}"),
            Source::Hub => f.write_str("hub"),
            Source::User(c) => write!(f, "user:{c}"),
            Source::Orchestrator => f.write_str("orchestrator"),
            Source::Decision(p) => write!(f, "decision:{p}"),
        }
    }
}

/// Error parsing a [`Source`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid event source: {0:?}")]
pub struct ParseSourceError(pub String);

impl FromStr for Source {
    type Err = ParseSourceError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseSourceError(s.to_string());
        match s {
            "hub" => return Ok(Source::Hub),
            "orchestrator" => return Ok(Source::Orchestrator),
            _ => {}
        }
        let (prefix, rest) = s.split_once(':').ok_or_else(err)?;
        if rest.is_empty() {
            return Err(err());
        }
        match prefix {
            "adapter" => Ok(Source::Adapter(rest.to_string())),
            "user" => Ok(Source::User(rest.to_string())),
            "decision" => Ok(Source::Decision(rest.to_string())),
            _ => Err(err()),
        }
    }
}

impl Serialize for Source {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Source {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------------------------
// Enums used in payloads
// ---------------------------------------------------------------------------------------------

/// Who started a turn (`turn.started.from`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFrom {
    /// The user, directly or via routing.
    #[default]
    User,
    /// The orchestrator thread.
    Orchestrator,
    /// The runtime resumed itself, for example after a background task completed.
    Runtime,
}

/// How a turn ended (`turn.ended.result`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnResult {
    /// The runtime finished the turn normally.
    #[default]
    Completed,
    /// The runtime reported an error.
    Error,
    /// The turn was interrupted (stop, park, user interrupt).
    Interrupted,
}

/// Why an item needs attention, in priority order (`attention.raised.reason`).
///
/// The derived `Ord` follows declaration order, which is the attention priority:
/// permission, question, error, review, stalled.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReason {
    /// The agent wants permission to use a tool.
    #[default]
    Permission,
    /// The agent asked the user a question.
    Question,
    /// The turn failed or the process crashed.
    Error,
    /// A finished turn needs review (graded gave_up / needs_verification).
    Review,
    /// No progress for longer than the stall threshold.
    Stalled,
}

impl AttentionReason {
    /// Priority rank, 0 = most urgent.
    pub fn priority(self) -> u8 {
        self as u8
    }

    /// Whether this reason blocks the agent until the user answers.
    pub fn is_blocking(self) -> bool {
        matches!(
            self,
            AttentionReason::Permission | AttentionReason::Question
        )
    }
}

/// Who resolved an attention item (`attention.resolved.by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolvedBy {
    /// A user client.
    #[default]
    User,
    /// The orchestrator (guarded by the relay rule).
    Orchestrator,
    /// A transient error was retried automatically.
    AutoRetry,
    /// The item timed out.
    Timeout,
    /// The thread stopped or its process died.
    ThreadStopped,
}

/// Decision site (`decision.made.site`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSite {
    /// Routing a user message to a thread.
    #[default]
    Route,
    /// Grading a finished turn.
    Grade,
    /// Classifying a block as user-blocked or transient.
    BlockKind,
}

/// Decision provider (`decision.made.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionProvider {
    /// Deterministic code rules.
    #[default]
    Code,
    /// The orchestrator thread.
    Orchestrator,
    /// The user answered.
    User,
}

impl DecisionProvider {
    /// Wire name (`code`, `orchestrator`, ...), also the `decision:<provider>` source name.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionProvider::Code => "code",
            DecisionProvider::Orchestrator => "orchestrator",
            DecisionProvider::User => "user",
        }
    }
}

/// Turn grade (`report.ready.grade`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grade {
    /// The turn achieved its goal.
    #[default]
    Success,
    /// Partially done.
    Partial,
    /// The agent gave up.
    GaveUp,
    /// Done but needs the user to verify.
    NeedsVerification,
}

impl Grade {
    /// Grades that raise a `review` attention.
    pub fn needs_review(self) -> bool {
        matches!(self, Grade::GaveUp | Grade::NeedsVerification)
    }
}

// ---------------------------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------------------------

/// `thread.created`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadCreated {
    /// Human-facing unique name.
    pub name: String,
    /// Project name, if bound to one.
    pub project: Option<String>,
    /// Repository path.
    pub repo: Option<String>,
    /// Worktree path the agent runs in.
    pub worktree: Option<String>,
    /// Box the thread runs on (`local` by default).
    #[serde(rename = "box")]
    pub box_name: String,
    /// Adapter/runtime name, e.g. `claude-code`.
    pub runtime: String,
    /// Runtime permission mode.
    pub mode: String,
    /// Extra names the thread answers to.
    pub aliases: Vec<String>,
    /// Runtime-specific extras (`ext.<runtime>`, `ext.twr.role`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.started` / `thread.parked` / `thread.resumed` / `thread.stopped`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionPayload {
    /// Runtime session id, when known.
    pub session_id: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `task.updated`: session-scoped work, independent of the foreground turn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskUpdated {
    /// Stable identifier within this runtime process.
    pub task_id: String,
    /// True while work remains; false on completion, failure or cancellation.
    pub running: bool,
    /// Short human-readable description.
    pub description: String,
    /// A liveness observation, rather than forward progress (does not reset stalls).
    pub heartbeat: bool,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown fields.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.crashed`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadCrashed {
    /// Process exit code (None if killed by a signal or unknown).
    pub exit_code: Option<i32>,
    /// Last bytes of stderr.
    pub stderr_tail: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// Who changed a thread's runtime settings (`thread.mode_changed.by`,
/// `thread.model_changed.by`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangedBy {
    /// A user client (`thread.set_mode` / `thread.set_model`, `twr mode` / `twr model`).
    #[default]
    User,
    /// The orchestrator: a mode change relaying what the user explicitly asked for (the hub
    /// checks the user's message, as for answers), or a model / effort change it applied.
    Orchestrator,
}

/// `thread.mode_changed`: the thread's permission mode is now `mode`. A runtime started from
/// here on starts in it; a live one is restarted on its session once it is idle (the hub,
/// `thread.set_mode`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadModeChanged {
    /// The new permission mode, in its canonical spelling.
    pub mode: String,
    /// The mode before.
    pub previous: String,
    /// Who changed it.
    pub by: ChangedBy,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.model_changed`: the thread's model and reasoning effort are now `model` and
/// `effort` (`None`: the runtime's default). A runtime started from here on starts with them;
/// a live one is restarted on its session once it is idle (the hub, `thread.set_model`).
/// Both values are always given, changed or not, next to the ones before.
///
/// With `runtime`, the thread also moved to another runtime (harness): a session cannot move
/// between runtimes, so the thread has no session until its new runtime starts one, and the
/// first turn of that session carries a handoff of what the previous one did (the hub,
/// [`crate::reduce::ThreadView::handoff_pending`]). `ext.twr` holds what the new runtime
/// needs (`acp_command`, `acp_attach`, `acp_preset`, as on `thread.created`) and the previous
/// session's id (`previous_session`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadModelChanged {
    /// The new model (the runtime's id or alias, as given); `None`: the runtime's default.
    pub model: Option<String>,
    /// The new reasoning effort, in its canonical spelling for the runtime; `None`: the
    /// runtime's default.
    pub effort: Option<String>,
    /// The model before.
    pub previous_model: Option<String>,
    /// The reasoning effort before.
    pub previous_effort: Option<String>,
    /// The new runtime, when the thread moved to another one (`None`: the runtime did not
    /// change).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The runtime before, with `runtime`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_runtime: Option<String>,
    /// Who changed them.
    pub by: ChangedBy,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.renamed`: a worker thread's new name (`thread.rename`, `twr rename`, the
/// orchestrator's `rename_thread`). Its worktree path and branch stay as they were made (they
/// keep the name the thread had then), and so do its aliases; its earlier names stay reserved
/// for it ([`crate::reduce::ThreadView::former_names`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadRenamed {
    /// The new name.
    pub name: String,
    /// The name before.
    pub previous: String,
    /// Who renamed it.
    pub by: ChangedBy,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `ext.twr.reason` of the `thread.stopped` that closes a runtime the hub restarts for a
/// permission mode change (`thread.mode_changed`): not a stop the user asked for, so the
/// thread does not count as stopped ([`crate::reduce::ThreadView::stopped`]); the runtime
/// starts again on the same session.
pub const MODE_RESTART_REASON: &str = "mode_change";

/// `ext.twr.reason` of the `thread.stopped` that closes a runtime the hub restarts for a
/// model or reasoning effort change only (`thread.model_changed`; a restart that applies a new
/// permission mode too says [`MODE_RESTART_REASON`]). Like that one, the thread does not count
/// as stopped and the runtime starts again on the same session.
pub const MODEL_RESTART_REASON: &str = "model_change";

/// `ext.twr.reason` of the `thread.stopped` that closes a runtime the hub replaces with
/// another runtime (`thread.model_changed` with a `runtime`). The thread does not count as
/// stopped; its next turn starts a new session of the new runtime, with a handoff.
pub const RUNTIME_RESTART_REASON: &str = "runtime_change";

/// `ext.twr.reason` of the `thread.stopped` that closes an idle runtime the hub stops to trim
/// its thread's worktree (`thread.trim`): its build output goes while no runtime works in it.
/// The thread does not count as stopped; its next prompt resumes the session.
pub const TRIM_RESTART_REASON: &str = "trim";

/// `ext.twr.reason` of the `thread.stopped` that closes an idle runtime on an SSH box the hub
/// switches to the build it launched there (the box's daemon ran another build of the same
/// runner protocol): every runtime of the box is stopped, the daemon replaced, and the thread
/// does not count as stopped; its next prompt resumes the session on the new daemon.
pub const BOX_UPGRADE_REASON: &str = "box_upgrade";

/// `task.updated` `task_id` prefix of a job (`twr job run`, the hub's `job.start`): a command
/// the hub runs for a thread, recorded as one of its background tasks by the hub
/// (`Source::Hub`). Unlike a runtime's own tasks, it outlives the runtime that was up when it
/// started (a restart, a stop, a park, an attach): only its own end removes it
/// ([`crate::reduce::BackgroundTask::job`]).
pub const JOB_TASK_PREFIX: &str = "job:";

/// Whether an event from `source` with `body` is a job's task record (`task.updated` of a
/// [`JOB_TASK_PREFIX`] task, which only the hub writes): what the job does, not the runtime.
pub fn is_job_task(source: &Source, body: &EventBody) -> bool {
    matches!(body, EventBody::TaskUpdated(p) if *source == Source::Hub && p.task_id.starts_with(JOB_TASK_PREFIX))
}

/// `ext.twr` key (`true`) on a `thread.started` / `thread.resumed` of a runtime the thread
/// moved away from (`thread.model_changed` with a `runtime`) before that start was recorded:
/// the start is the previous runtime's, never the new runtime's session
/// ([`crate::reduce::ThreadView::handoff_pending`]), and the hub replaces that runtime.
pub const RUNTIME_MOVED_KEY: &str = "runtime_moved";

/// `ext.twr` key of an `acp` thread's agent: its argv (a JSON array of strings). On
/// `thread.created` and on a `thread.model_changed` that moves the thread to an agent, the
/// agent the thread runs from then on; on a `thread.started` / `thread.resumed`, the agent
/// that process runs (stamped by the hub or the runner that started it; missing from runners
/// before agent stamps). Two agents of one adapter (`acp`) differ only here: a start of
/// another agent than the one the thread moved to is not the new runtime's session
/// ([`crate::reduce::ThreadView::handoff_pending`]).
pub const ACP_COMMAND_KEY: &str = "acp_command";

/// `ext.twr.acp_command` ([`ACP_COMMAND_KEY`]) of `ext`, when it is an array.
pub fn acp_command(ext: Option<&JsonMap>) -> Option<&serde_json::Value> {
    ext?.get("twr")?
        .get(ACP_COMMAND_KEY)
        .filter(|v| v.is_array())
}

/// `ext.twr` key of a turn the runtime started on its own: on its `turn.started{from: runtime}`
/// and its `turn.ended`, the adapter's id for it (a string, unique within the runtime process).
/// The hub numbers such a turn, or drops its start while a turn of its own is open; an end
/// with this key closes only the open turn whose start carried the same id (`Adapter`).
pub const RUNTIME_TURN_KEY: &str = "runtime_turn";

/// `ext.twr.runtime_turn` ([`RUNTIME_TURN_KEY`]) of `ext`.
pub fn runtime_turn(ext: Option<&JsonMap>) -> Option<&str> {
    ext?.get("twr")?.get(RUNTIME_TURN_KEY)?.as_str()
}

/// `ext.twr` key of a tool call: the step the agent's own plan has in progress from this call
/// on, when the call changes it (a string; empty: none is). Claude Code's `TodoWrite` and
/// `TaskUpdate` calls carry it; a thread's activity line leads with it
/// ([`crate::activity`]).
pub const STEP_KEY: &str = "step";

/// `ext.twr` key of a tool call: what the agent said the call is for (a string, an imperative
/// such as `Run the TUI tests`: Claude Code's Bash `description`). A thread's activity line
/// says it ([`crate::activity::tool_phrase`]).
pub const PURPOSE_KEY: &str = "purpose";

/// `ext.twr.<key>` of `ext`, when it is a string.
pub fn twr_str<'a>(ext: Option<&'a JsonMap>, key: &str) -> Option<&'a str> {
    ext?.get("twr")?.get(key)?.as_str()
}

/// `ext.twr` key (`true`) on an adapter's `attention.raised` for a request of the runtime's
/// session rather than of its turn (a Codex child agent's): the hub records it outside any
/// turn, so the end of the turn that was open does not close it.
pub const SESSION_SCOPE_KEY: &str = "session";

/// Sets `ext.twr.<key>` to `value` in `ext`, keeping what else `ext.twr` holds.
pub fn set_twr_ext(ext: &mut Option<JsonMap>, key: &str, value: Value) {
    let twr = ext
        .get_or_insert_with(JsonMap::new)
        .entry("twr")
        .or_insert_with(|| Value::Object(Map::new()));
    if !twr.is_object() {
        *twr = Value::Object(Map::new());
    }
    if let Value::Object(t) = twr {
        t.insert(key.to_string(), value);
    }
}

/// Whether `thread.stopped` `p` closed a runtime the hub restarts in new settings
/// ([`MODE_RESTART_REASON`], [`MODEL_RESTART_REASON`], [`RUNTIME_RESTART_REASON`],
/// [`TRIM_RESTART_REASON`] or [`BOX_UPGRADE_REASON`]).
pub fn is_settings_restart(p: &SessionPayload) -> bool {
    matches!(
        p.ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get("reason"))
            .and_then(Value::as_str),
        Some(
            MODE_RESTART_REASON
                | MODEL_RESTART_REASON
                | RUNTIME_RESTART_REASON
                | TRIM_RESTART_REASON
                | BOX_UPGRADE_REASON
        )
    )
}

/// `thread.deleted`: the user confirmed deleting the thread (a delete card, `thread.delete`).
/// Its runtime was stopped, and its hub-managed worktree (uncommitted changes and all, with its
/// administrative record) and its `twr/<name>` branch moved to the trash of its box
/// ([`ThreadDeleted::trash`]; purged after `[trash] keep_days`, `twr trash restore` puts it
/// back) before this was recorded; from here on the thread is gone from every listing
/// ([`crate::reduce::Registry`] drops it, and every later event of it until a
/// `thread.restored`) and its name is free again. Its earlier events stay in the log.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadDeleted {
    /// The thread's name.
    pub name: String,
    /// The worktree that was moved to the trash, if the hub managed one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The worktree's directory, when part of it is still there: it could not all be moved
    /// (copied to the trash on another file system, then files it may not delete stayed). It
    /// is no worktree any more and twr never deletes it blindly: the user looks at what is
    /// left and removes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_left: Option<String>,
    /// Build output the deletion could not delete, nor put back where it was (its place was
    /// taken meanwhile): kept in the trash item apart, and put back where it was by a restore
    /// ([`crate::git::TrashManifest::build_output_aside`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build_output_aside: Vec<String>,
    /// The trash item that keeps the worktree, its record and the branch (`None`: nothing to
    /// keep, the thread had no worktree of its own).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash: Option<String>,
    /// That item's directory on the thread's box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_dir: Option<String>,
    /// Bytes on disk that item holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trash_bytes: Option<u64>,
    /// What the delete card said it loses, in short (`3 uncommitted files, 2 unpushed
    /// commits`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub losses: Option<String>,
    /// The branch that was moved to the trash, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Uncommitted files the worktree had when it was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_files: Option<u64>,
    /// Commits that were only on the branch (on no other branch, tag or remote) when it was
    /// deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_only_commits: Option<u64>,
    /// Commits only the worktree's own `HEAD` or `HEAD` reflog reached (made on a detached
    /// `HEAD`: on no branch, tag or remote) when it was removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_only_commits: Option<u64>,
    /// Queued prompts dropped with the thread.
    pub dropped_prompts: u64,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.restored`: the user brought a deleted thread back from the trash (`trash.restore`):
/// its box made its branch and a worktree again from the trash item first, copying (the item
/// stays in the trash, listed as restored, until it is purged). The thread is back in every
/// listing as it was when deleted (parked, with its session), under [`ThreadRestored::name`]
/// and with its worktree at [`ThreadRestored::worktree`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadRestored {
    /// The trash item it came back from (restored once: the hub lists it as restored, and
    /// refuses to restore it again).
    pub item: String,
    /// Its name now.
    pub name: String,
    /// Its name when it was deleted, when that is not [`ThreadRestored::name`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_name: Option<String>,
    /// Its worktree now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Its branch now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Its branch existed before the thread (as `thread.created` said).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub existing_branch: bool,
    /// The names it had before its renames that it keeps
    /// ([`crate::reduce::ThreadView::former_names`]): those no other thread had taken (or was
    /// taking) when it was restored, checked and reserved with its name. The others stay with
    /// the threads that took them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub former_names: Vec<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `thread.attached` / `thread.detached`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientPayload {
    /// Client that attached/detached.
    pub client: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `turn.started`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnStarted {
    /// Prompt sent to the runtime.
    pub prompt: String,
    /// Who started the turn.
    pub from: TurnFrom,
    /// Id of the `decision.made` event that routed this turn, if any.
    pub routed_by: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// Token/cost usage of a turn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// Cost in USD, if the runtime reports it.
    pub cost: Option<f64>,
}

impl Usage {
    /// `self` and `other` together: tokens add up; the cost is known only when both are.
    pub fn add(&self, other: &Usage) -> Usage {
        Usage {
            input: self.input + other.input,
            output: self.output + other.output,
            cost: self.cost.zip(other.cost).map(|(a, b)| a + b),
        }
    }
}

/// `turn.ended`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnEnded {
    /// How the turn ended.
    pub result: TurnResult,
    /// Runtime session id.
    pub session_id: Option<String>,
    /// Token/cost usage.
    pub usage: Option<Usage>,
    /// Wall-clock duration.
    pub duration_ms: Option<u64>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `ext.twr` key of a `turn.started` whose prompt came from the queue (`thread.send`, `say`):
/// the queued message's ref, the `ref` those calls returned (an integer, the hub's queue row
/// id, never reused). It links the message to its turn: `message.withdraw` takes a message
/// back by it, and clients find the turn their message started with it.
pub const MESSAGE_REF_KEY: &str = "ref";

/// `ext.twr.ref` ([`MESSAGE_REF_KEY`]) of turn start `p`.
pub fn message_ref(p: &TurnStarted) -> Option<i64> {
    p.ext.as_ref()?.get("twr")?.get(MESSAGE_REF_KEY)?.as_i64()
}

/// `prompt.withdrawn` (the message's thread, no turn): the user took back a message that was
/// still queued (`message.withdraw`). The hub removed it from the queue in the same
/// transaction, so it never starts a turn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptWithdrawn {
    /// The message's ref ([`MESSAGE_REF_KEY`]).
    #[serde(rename = "ref")]
    pub msg_ref: i64,
    /// The message as it was queued.
    pub prompt: String,
    /// Extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `turn.withdrawn` (the message's thread, in the turn it started): the user took back the
/// message of this turn while it ran and had not acted yet (`message.withdraw`). The turn has
/// no authority from then on (the orchestrator's relay guard refuses its relays, mode changes
/// and every other change it asks for), the hub asked the runtime to interrupt it, and it ends
/// `interrupted` whatever the runtime reports.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnWithdrawn {
    /// The message's ref ([`MESSAGE_REF_KEY`]).
    #[serde(rename = "ref")]
    pub msg_ref: i64,
    /// The turn (also the event's `turn`).
    pub turn: u64,
    /// Extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `turn.interrupted` (the thread, in the turn it interrupts): the user asked to send the next
/// queued message now (`message.send_now`), so the hub asked the runtime to interrupt the turn
/// that runs. The turn keeps what it did and its authority, as any turn the user interrupts;
/// it ends `interrupted` whatever the runtime reports (no grade, report, review or retry), and
/// the queued message `ref` is the next prompt the thread runs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnInterrupted {
    /// The queued message sent now ([`MESSAGE_REF_KEY`]).
    #[serde(rename = "ref")]
    pub msg_ref: i64,
    /// The turn (also the event's `turn`).
    pub turn: u64,
    /// Extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// A hub-authored update shown in the orchestrator conversation without a model turn.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConversationUpdate {
    /// The user message acknowledged by a direct command, when any.
    pub user_message: Option<String>,
    /// Plain text authored from hub facts.
    pub text: String,
    /// The worker this update concerns, when any.
    pub about_thread: Option<String>,
    /// Originating attention item, report or routing event.
    pub ext: Option<JsonMap>,
    /// Unknown keys.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `agent.text`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentText {
    /// Text (a chunk when `delta`, the full message when `final`).
    pub text: String,
    /// Streaming chunk.
    pub delta: bool,
    /// Complete final message.
    #[serde(rename = "final")]
    pub is_final: bool,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `agent.tool_call`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentToolCall {
    /// Runtime tool-call id.
    pub call_id: String,
    /// Tool name.
    pub tool: String,
    /// Short human-readable summary of the input.
    pub input_summary: String,
    /// Blob pointer to the full input.
    pub input_ref: Option<String>,
    /// Whether the call can change state outside the agent (writes, shell, network).
    pub risky: bool,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `agent.tool_result`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentToolResult {
    /// Runtime tool-call id this result belongs to.
    pub call_id: String,
    /// Whether the tool succeeded.
    pub ok: bool,
    /// Short human-readable summary of the output.
    pub output_summary: String,
    /// Blob pointer to the full output.
    pub output_ref: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `agent.thinking` (only emitted when the thread enables thinking)
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentThinking {
    /// Thinking text.
    pub text: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `attention.raised`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionRaised {
    /// Unique attention id. An adapter emits its own id; the hub records it as
    /// `<thread>:<adapter id>` (see [`crate::adapter::hub_attn_id`]).
    pub attn_id: String,
    /// Why it needs attention.
    pub reason: AttentionReason,
    /// One-line summary for the card.
    pub summary: String,
    /// Answer options (e.g. `allow`, `deny`), possibly empty.
    pub options: Vec<String>,
    /// Tool call this is about, for permissions.
    pub call_id: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `attention.annotated` (orchestrator note, updates the card in place)
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionAnnotated {
    /// Attention id being annotated.
    pub attn_id: String,
    /// The note.
    pub note: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// An answer to an attention item (also what adapters receive via `respond`).
///
/// Like the payload structs it tolerates missing fields and keeps unknown keys, because it is
/// nested in `attention.resolved` data and [`crate::blob::cap_payload`] may move
/// `answer.message` to a blob (adding `answer.message_ref` and `answer._capped`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Answer {
    /// `allow`, `deny`, or one of the item's options.
    pub decision: String,
    /// Optional free-text message passed to the agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

impl Answer {
    /// An answer with only a decision.
    pub fn new(decision: impl Into<String>) -> Self {
        Answer {
            decision: decision.into(),
            ..Default::default()
        }
    }

    /// Shorthand for `allow`.
    pub fn allow() -> Self {
        Self::new("allow")
    }

    /// Shorthand for `deny`.
    pub fn deny() -> Self {
        Self::new("deny")
    }

    /// Whether the decision is `allow`.
    pub fn is_allow(&self) -> bool {
        self.decision == "allow"
    }
}

/// `attention.resolved`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttentionResolved {
    /// Attention id being resolved.
    pub attn_id: String,
    /// Who resolved it.
    pub by: ResolvedBy,
    /// The answer given, if any (none for timeouts / stopped threads).
    pub answer: Option<Answer>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `decision.made`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionMade {
    /// Decision site.
    pub site: DecisionSite,
    /// Provider that decided.
    pub provider: DecisionProvider,
    /// Options considered.
    pub options: Vec<String>,
    /// Option chosen.
    pub chosen: String,
    /// Confidence in [0, 1].
    pub confidence: f64,
    /// Time the provider took.
    pub latency_ms: u64,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `report.ready`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportReady {
    /// 2-3 sentence summary.
    pub summary: String,
    /// Grade of the turn.
    pub grade: Grade,
    /// Turn number the report covers.
    pub turn: Option<u64>,
    /// Runtime session id.
    pub session_id: Option<String>,
    /// Who wrote it (`orchestrator` or `hub`).
    pub written_by: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `git.snapshot`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GitSnapshot {
    /// Current branch (None when detached).
    pub branch: Option<String>,
    /// Number of dirty (modified, staged or untracked) files.
    pub dirty_files: u64,
    /// Commits ahead of upstream.
    pub ahead: u64,
    /// Commits behind upstream.
    pub behind: u64,
    /// Merge readiness: the branch it is compared with (`[projects.<name>] base`, else what
    /// the worktree was made from), when there is one and it could be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Commits `HEAD` has that [`GitSnapshot::base`] has not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ahead: Option<u64>,
    /// Commits [`GitSnapshot::base`] has that `HEAD` has not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_behind: Option<u64>,
    /// The files a merge of `HEAD` and [`GitSnapshot::base`] would conflict in (at most
    /// [`CONFLICT_FILES_MAX`]; empty: it merges cleanly); `None`: not known (a git without `--attr-source`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<String>>,
    /// How many files conflict, when more than [`GitSnapshot::conflicts`] names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict_count: Option<u64>,
    /// Whether a merge of `HEAD` and [`GitSnapshot::base`] would conflict, even where git names
    /// no file (some directory renames); `None`: not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicted: Option<bool>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// Most conflicting files a [`GitSnapshot`] names.
pub const CONFLICT_FILES_MAX: usize = 20;

/// `runner.connected` / `runner.disconnected`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunnerPayload {
    /// Box name.
    #[serde(rename = "box")]
    pub box_name: String,
    /// Runner version.
    pub version: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `project.read`: one call of the orchestrator's project tools (`project.files`,
/// `project.read`, `project.search`), served or refused, appended by the hub on the
/// orchestrator's thread so the user sees every read.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectRead {
    /// `files`, `read` or `search`.
    pub tool: String,
    /// The project as asked.
    pub project: String,
    /// The path read or listed (the top: empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What `search` looked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The worker thread whose branch was read; `None`: the repository's `HEAD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_of: Option<String>,
    /// The commit read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Bytes of repository content returned.
    pub bytes: u64,
    /// Why the call was refused; `None`: served.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refused: Option<String>,
    /// Extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

impl ProjectRead {
    /// One line saying what was read, as the TUI and transcripts show it: `read
    /// api:README.md at 1a2b3c4d5e6f (4.2 KB)`, `searched api for "x" ...`, `listed api:src
    /// ...`, or what was refused and why.
    pub fn line(&self) -> String {
        let at = |p: &str| {
            if p.is_empty() {
                format!("{}'s top", self.project)
            } else {
                format!("{}:{p}", self.project)
            }
        };
        let path = self.path.as_deref().unwrap_or("");
        let what = match self.tool.as_str() {
            "files" => format!("list {}", at(path)),
            "search" => {
                let mut s = format!(
                    "search {} for {:?}",
                    self.project,
                    self.query.as_deref().unwrap_or("")
                );
                if !path.is_empty() {
                    s.push_str(&format!(" under {path}"));
                }
                s
            }
            _ => format!("read {}", at(path)),
        };
        let mut s = match &self.refused {
            Some(why) => format!("refused to {what}: {why}"),
            None => {
                let done = match self.tool.as_str() {
                    "files" => what.replacen("list", "listed", 1),
                    "search" => what.replacen("search", "searched", 1),
                    _ => what,
                };
                format!("{done} ({})", crate::git::human_bytes(self.bytes))
            }
        };
        if let Some(t) = &self.branch_of {
            s.push_str(&format!(", on {t}'s branch"));
        }
        if let (Some(c), None) = (&self.commit, &self.refused) {
            s.push_str(&format!(", at {}", c.chars().take(12).collect::<String>()));
        }
        s
    }
}

/// `context.updated`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextUpdated {
    /// Hash of the new standing context.
    pub hash: String,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `runtime.limits`: a runtime's report of its plan's usage windows (Claude's
/// `rate_limit_event` and `get_usage`, Codex's `account/rateLimits/*`, Grok Build's
/// `_x.ai/billing`), sent by its adapter. The hub folds it into `account.usage` and does not
/// record it ([`crate::usage`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeLimits {
    /// The windows the report covers (a passive report may cover only some).
    pub windows: Vec<crate::usage::UsageWindow>,
    /// `event` (the runtime's own report) or `read` (twr asked).
    pub source: String,
    /// The plan, when the runtime says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `account.usage` (hub thread): the latest known usage windows of one account of a runtime
/// ([`crate::usage`]), recorded when they change or are [`crate::usage::REFRESH_AFTER`] old.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountUsage {
    /// Runtime name.
    pub runtime: String,
    /// Account name; absent for the runtime's own login.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Every window known now, shortest first.
    pub windows: Vec<crate::usage::UsageWindow>,
    /// How the latest report came: `event` or `read`.
    pub source: String,
    /// The plan, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// The thread whose runtime reported it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// When the runtime reported it (a runner's capture time; the event's `ts` keeps the log
    /// in order and may be later). Absent: the event's `ts`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `worktree.disk` (hub, on the thread): what its worktree takes on disk, as its box last
/// measured it ([`crate::git::WorktreeDisk`]). Recorded when it changed noticeably.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeDiskMeasured {
    /// Bytes in all.
    pub bytes: u64,
    /// Of those, bytes in build output (by name: `[threads] park_disposable`).
    pub build_bytes: u64,
    /// The walk stopped early: the sizes are lower bounds.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

/// `worktree.trimmed` (on the thread, by whoever asked: `twr trim`, a disk card's Trim): its
/// worktree's build output was deleted ([`crate::git::TrimOutcome`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeTrimmed {
    /// The entries deleted (`target/`).
    pub deleted: Vec<String>,
    /// Bytes they took.
    pub freed: u64,
    /// What did not go as it should.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Runtime-specific extras.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ext: Option<JsonMap>,
    /// Unknown keys, including `<field>_ref` blob pointers.
    #[serde(flatten, default, skip_serializing_if = "JsonMap::is_empty")]
    pub extra: JsonMap,
}

// ---------------------------------------------------------------------------------------------
// EventBody
// ---------------------------------------------------------------------------------------------

macro_rules! event_body {
    ($( $(#[$meta:meta])* $variant:ident($payload:ty) => $kind:literal, )*) => {
        /// `kind` + typed `data` of an event.
        #[derive(Debug, Clone, PartialEq)]
        pub enum EventBody {
            $( $(#[$meta])* $variant($payload), )*
            /// A kind this build does not know; kept verbatim for forward compatibility.
            Unknown {
                /// The kind string.
                kind: String,
                /// The raw `data` value.
                data: Value,
            },
        }

        /// Serializes a body's payload in place (no intermediate `Value`, so field order is
        /// the struct's declaration order).
        struct DataOf<'a>(&'a EventBody);

        impl Serialize for DataOf<'_> {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                match self.0 {
                    $( EventBody::$variant(p) => p.serialize(s), )*
                    EventBody::Unknown { data, .. } => data.serialize(s),
                }
            }
        }

        /// Every kind string this build knows.
        pub const KNOWN_KINDS: &[&str] = &[$($kind),*];

        impl EventBody {
            /// The kind string, e.g. `thread.created`.
            pub fn kind(&self) -> &str {
                match self {
                    $( EventBody::$variant(_) => $kind, )*
                    EventBody::Unknown { kind, .. } => kind,
                }
            }

            /// Serializes the payload as the `data` value.
            pub fn data(&self) -> Result<Value, serde_json::Error> {
                match self {
                    $( EventBody::$variant(p) => serde_json::to_value(p), )*
                    EventBody::Unknown { data, .. } => Ok(data.clone()),
                }
            }

            /// Builds a body from `kind` + `data`. Unknown kinds become [`EventBody::Unknown`];
            /// a known kind with malformed data is an error.
            pub fn from_kind_data(kind: &str, data: Value) -> Result<Self, serde_json::Error> {
                match kind {
                    $( $kind => Ok(EventBody::$variant(from_data(data)?)), )*
                    _ => Ok(EventBody::Unknown { kind: kind.to_string(), data }),
                }
            }

            /// [`EventBody::from_kind_data`] from `data`'s JSON text (`None`: `null`), parsed
            /// straight into the kind's payload, without a [`Value`] in between: what reading
            /// the log and the hub's streams spends most of its time on. JSON that does not
            /// parse that way (a key given twice, which a [`Value`] takes the last of) goes the
            /// way of [`EventBody::from_kind_data`]: what that one accepts, this one builds the
            /// same. It also takes a number out of `f64`'s range in a field the payload ignores,
            /// which a [`Value`] refuses (serde_json never writes one).
            pub fn from_kind_json(kind: &str, data: Option<&str>) -> Result<Self, serde_json::Error> {
                let Some(json) = data else {
                    return Self::from_kind_data(kind, Value::Null);
                };
                let direct = match kind {
                    $( $kind => serde_json::from_str(json).map(EventBody::$variant), )*
                    _ => return Self::from_kind_data(kind, serde_json::from_str(json)?),
                };
                direct.or_else(|_| Self::from_kind_data(kind, serde_json::from_str(json)?))
            }

            /// Whether the payload may hold a field moved to the blob store
            /// ([`crate::blob::has_capped`] of [`EventBody::data`]). `false` is certain:
            /// serialized, it has no `_capped` anywhere (twr's JSON never escapes the
            /// underscore or the letters); `true` asks for the full check.
            pub fn maybe_capped(&self) -> bool {
                serde_json::to_string(&DataOf(self))
                    .map_or(true, |s| s.contains(crate::blob::CAPPED_KEY))
            }
        }
    };
}

fn from_data<T: DeserializeOwned + Default>(data: Value) -> Result<T, serde_json::Error> {
    if data.is_null() {
        Ok(T::default())
    } else {
        serde_json::from_value(data)
    }
}

event_body! {
    /// `thread.created`
    ThreadCreated(ThreadCreated) => "thread.created",
    /// `thread.started`
    ThreadStarted(SessionPayload) => "thread.started",
    /// `thread.parked`
    ThreadParked(SessionPayload) => "thread.parked",
    /// `thread.resumed`
    ThreadResumed(SessionPayload) => "thread.resumed",
    /// `thread.stopped`
    ThreadStopped(SessionPayload) => "thread.stopped",
    /// `thread.crashed`
    ThreadCrashed(ThreadCrashed) => "thread.crashed",
    /// `thread.attached`
    ThreadAttached(ClientPayload) => "thread.attached",
    /// `thread.detached`
    ThreadDetached(ClientPayload) => "thread.detached",
    /// `thread.mode_changed`
    ThreadModeChanged(ThreadModeChanged) => "thread.mode_changed",
    /// `thread.model_changed`
    ThreadModelChanged(ThreadModelChanged) => "thread.model_changed",
    /// `thread.renamed`
    ThreadRenamed(ThreadRenamed) => "thread.renamed",
    /// `thread.deleted`
    ThreadDeleted(ThreadDeleted) => "thread.deleted",
    /// `thread.restored`
    ThreadRestored(ThreadRestored) => "thread.restored",
    /// `turn.started`
    TurnStarted(TurnStarted) => "turn.started",
    /// `turn.ended`
    TurnEnded(TurnEnded) => "turn.ended",
    /// `turn.withdrawn`
    TurnWithdrawn(TurnWithdrawn) => "turn.withdrawn",
    /// `turn.interrupted`
    TurnInterrupted(TurnInterrupted) => "turn.interrupted",
    /// `prompt.withdrawn`
    PromptWithdrawn(PromptWithdrawn) => "prompt.withdrawn",
    /// Session-scoped background task lifecycle and liveness.
    TaskUpdated(TaskUpdated) => "task.updated",
    /// `conversation.update`
    ConversationUpdate(ConversationUpdate) => "conversation.update",
    /// `agent.text`
    AgentText(AgentText) => "agent.text",
    /// `agent.tool_call`
    AgentToolCall(AgentToolCall) => "agent.tool_call",
    /// `agent.tool_result`
    AgentToolResult(AgentToolResult) => "agent.tool_result",
    /// `agent.thinking`
    AgentThinking(AgentThinking) => "agent.thinking",
    /// `attention.raised`
    AttentionRaised(AttentionRaised) => "attention.raised",
    /// `attention.annotated`
    AttentionAnnotated(AttentionAnnotated) => "attention.annotated",
    /// `attention.resolved`
    AttentionResolved(AttentionResolved) => "attention.resolved",
    /// `decision.made`
    DecisionMade(DecisionMade) => "decision.made",
    /// `report.ready`
    ReportReady(ReportReady) => "report.ready",
    /// `git.snapshot`
    GitSnapshot(GitSnapshot) => "git.snapshot",
    /// `runner.connected`
    RunnerConnected(RunnerPayload) => "runner.connected",
    /// `runner.disconnected`
    RunnerDisconnected(RunnerPayload) => "runner.disconnected",
    /// `context.updated`
    ContextUpdated(ContextUpdated) => "context.updated",
    /// `runtime.limits`
    RuntimeLimits(RuntimeLimits) => "runtime.limits",
    /// `account.usage`
    AccountUsage(AccountUsage) => "account.usage",
    /// `project.read`
    ProjectRead(ProjectRead) => "project.read",
    /// `worktree.disk`
    WorktreeDisk(WorktreeDiskMeasured) => "worktree.disk",
    /// `worktree.trimmed`
    WorktreeTrimmed(WorktreeTrimmed) => "worktree.trimmed",
}

impl EventBody {
    /// Whether events of this body's kind belong on the hub control stream.
    pub fn is_control(&self) -> bool {
        is_control_kind(self.kind())
    }

    /// Whether this is a per-thread `agent.*` event (never on the control stream).
    pub fn is_agent(&self) -> bool {
        self.kind().starts_with("agent.")
    }
}

/// Control-stream filter.
///
/// The control stream's set: `thread.*`, `turn.started`, `turn.ended`, `turn.withdrawn` and
/// `prompt.withdrawn` (a message the user took back), `turn.interrupted` (the user sent the next
/// queued message now), `task.updated`, `attention.*`,
/// `report.*`, `decision.*`, and the small hub-relevant kinds `git.snapshot`, `runner.*`,
/// `context.updated`, `conversation.update`, `account.*` (the hub's `account.usage`),
/// `project.read` (an orchestrator read, shown to the user) and `worktree.*`. `agent.*` never
/// goes on the control stream.
pub fn is_control_kind(kind: &str) -> bool {
    kind.starts_with("thread.")
        || kind == "turn.started"
        || kind == "turn.ended"
        || kind == "turn.withdrawn"
        || kind == "turn.interrupted"
        || kind == "prompt.withdrawn"
        || kind == "task.updated"
        || kind.starts_with("attention.")
        || kind.starts_with("report.")
        || kind.starts_with("decision.")
        || kind == "git.snapshot"
        || kind.starts_with("runner.")
        || kind == "context.updated"
        || kind == "conversation.update"
        || kind.starts_with("account.")
        || kind == "project.read"
        || kind.starts_with("worktree.")
}

impl Serialize for EventBody {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = serializer.serialize_struct("EventBody", 2)?;
        st.serialize_field("kind", self.kind())?;
        st.serialize_field("data", &DataOf(self))?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for EventBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct KindData {
            kind: String,
            #[serde(default)]
            data: Value,
        }
        let kd = KindData::deserialize(deserializer)?;
        EventBody::from_kind_data(&kd.kind, kd.data).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn ids_are_monotonic_within_the_process() {
        let ids: Vec<String> = (0..2000).map(|_| new_id()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert!(ids.iter().all(|i| i.len() == 26));
    }

    use super::*;
    use serde_json::json;

    fn ext(runtime: &str) -> Option<JsonMap> {
        let mut m = JsonMap::new();
        m.insert(runtime.into(), json!({"k": 1}));
        Some(m)
    }

    /// One body of every known kind, with non-default values.
    fn all_bodies() -> Vec<EventBody> {
        let session = SessionPayload {
            session_id: Some("s1".into()),
            ext: ext("fake"),
            ..Default::default()
        };
        let runner = RunnerPayload {
            box_name: "gpu".into(),
            version: Some("0.1.0".into()),
            ..Default::default()
        };
        vec![
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                project: Some("proj".into()),
                repo: Some("/r".into()),
                worktree: Some("/w".into()),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                aliases: vec!["a".into()],
                ext: ext("twr"),
                ..Default::default()
            }),
            EventBody::ThreadStarted(session.clone()),
            EventBody::ThreadParked(session.clone()),
            EventBody::ThreadResumed(session.clone()),
            EventBody::ThreadStopped(session),
            EventBody::ThreadCrashed(ThreadCrashed {
                exit_code: Some(1),
                stderr_tail: "boom".into(),
                ..Default::default()
            }),
            EventBody::ThreadAttached(ClientPayload {
                client: "cli".into(),
                ..Default::default()
            }),
            EventBody::ThreadDetached(ClientPayload {
                client: "cli".into(),
                ..Default::default()
            }),
            EventBody::ThreadModeChanged(ThreadModeChanged {
                mode: "auto".into(),
                previous: "manual".into(),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                model: Some("claude-fable-5-1".into()),
                effort: Some("high".into()),
                previous_model: None,
                previous_effort: Some("low".into()),
                runtime: Some("codex".into()),
                previous_runtime: Some("claude-code".into()),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }),
            EventBody::ThreadRenamed(ThreadRenamed {
                name: "notifications-design".into(),
                previous: "twr-1".into(),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }),
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "api".into(),
                worktree: Some("/w".into()),
                branch: Some("twr/api".into()),
                dirty_files: Some(2),
                branch_only_commits: Some(1),
                head_only_commits: Some(1),
                dropped_prompts: 1,
                trash: Some("01J00000000000000000000000".into()),
                trash_bytes: Some(4096),
                losses: Some("2 uncommitted files".into()),
                ..Default::default()
            }),
            EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: "api-2".into(),
                previous_name: Some("api".into()),
                worktree: Some("/w2".into()),
                branch: Some("twr/api-2".into()),
                ..Default::default()
            }),
            EventBody::TurnStarted(TurnStarted {
                prompt: "do it".into(),
                from: TurnFrom::Orchestrator,
                routed_by: Some("01DEC".into()),
                ..Default::default()
            }),
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Interrupted,
                session_id: Some("s1".into()),
                usage: Some(Usage {
                    input: 10,
                    output: 20,
                    cost: Some(0.25),
                }),
                duration_ms: Some(1500),
                ..Default::default()
            }),
            EventBody::TurnWithdrawn(TurnWithdrawn {
                msg_ref: 7,
                turn: 2,
                ..Default::default()
            }),
            EventBody::TurnInterrupted(TurnInterrupted {
                msg_ref: 9,
                turn: 3,
                ..Default::default()
            }),
            EventBody::PromptWithdrawn(PromptWithdrawn {
                msg_ref: 8,
                prompt: "never mind".into(),
                ..Default::default()
            }),
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "review".into(),
                running: true,
                description: "Reviewing the change".into(),
                heartbeat: true,
                ext: ext("fake"),
                ..Default::default()
            }),
            EventBody::ConversationUpdate(ConversationUpdate {
                user_message: Some("allow api".into()),
                text: "Applied.".into(),
                about_thread: Some("api".into()),
                ..Default::default()
            }),
            EventBody::AgentText(AgentText {
                text: "hi".into(),
                delta: true,
                is_final: false,
                ..Default::default()
            }),
            EventBody::AgentToolCall(AgentToolCall {
                call_id: "c1".into(),
                tool: "Bash".into(),
                input_summary: "ls".into(),
                input_ref: Some("sha256:00".into()),
                risky: true,
                ..Default::default()
            }),
            EventBody::AgentToolResult(AgentToolResult {
                call_id: "c1".into(),
                ok: false,
                output_summary: "nope".into(),
                output_ref: None,
                ..Default::default()
            }),
            EventBody::AgentThinking(AgentThinking {
                text: "hmm".into(),
                ..Default::default()
            }),
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "a1".into(),
                reason: AttentionReason::Stalled,
                summary: "stuck".into(),
                options: vec!["x".into(), "y".into()],
                call_id: Some("c1".into()),
                ..Default::default()
            }),
            EventBody::AttentionAnnotated(AttentionAnnotated {
                attn_id: "a1".into(),
                note: "fine".into(),
                ..Default::default()
            }),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "a1".into(),
                by: ResolvedBy::ThreadStopped,
                answer: Some(Answer {
                    decision: "deny".into(),
                    message: Some("no".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::BlockKind,
                provider: DecisionProvider::Code,
                options: vec!["transient".into(), "user_blocked".into()],
                chosen: "transient".into(),
                confidence: 1.0,
                latency_ms: 42,
                ..Default::default()
            }),
            EventBody::ReportReady(ReportReady {
                summary: "Done.".into(),
                grade: Grade::NeedsVerification,
                turn: Some(2),
                session_id: Some("s1".into()),
                written_by: "hub".into(),
                ..Default::default()
            }),
            EventBody::GitSnapshot(GitSnapshot {
                branch: Some("twr/api".into()),
                dirty_files: 3,
                ahead: 1,
                behind: 2,
                ..Default::default()
            }),
            EventBody::RunnerConnected(runner.clone()),
            EventBody::RunnerDisconnected(runner),
            EventBody::ContextUpdated(ContextUpdated {
                hash: "sha256:ab".into(),
                ..Default::default()
            }),
            EventBody::RuntimeLimits(RuntimeLimits {
                windows: vec![crate::usage::UsageWindow {
                    window: "five_hour".into(),
                    used_pct: 22.5,
                    resets_at: Some(Utc::now().trunc_subsecs(0)),
                    minutes: None,
                    observed_at: None,
                }],
                source: "event".into(),
                ..Default::default()
            }),
            EventBody::AccountUsage(AccountUsage {
                runtime: "codex".into(),
                windows: vec![crate::usage::UsageWindow {
                    window: "seven_day".into(),
                    used_pct: 42.0,
                    resets_at: None,
                    minutes: Some(10080),
                    observed_at: None,
                }],
                source: "read".into(),
                plan: Some("pro".into()),
                ..Default::default()
            }),
            EventBody::ProjectRead(ProjectRead {
                tool: "read".into(),
                project: "api".into(),
                path: Some("README.md".into()),
                commit: Some("a1b2c3d".into()),
                bytes: 42,
                ..Default::default()
            }),
            EventBody::WorktreeDisk(WorktreeDiskMeasured {
                bytes: 7 << 30,
                build_bytes: 6 << 30,
                ..Default::default()
            }),
            EventBody::WorktreeTrimmed(WorktreeTrimmed {
                deleted: vec!["target/".into()],
                freed: 6 << 30,
                ..Default::default()
            }),
        ]
    }

    #[test]
    fn every_known_kind_round_trips() {
        let bodies = all_bodies();
        assert_eq!(bodies.len(), KNOWN_KINDS.len());
        for (i, body) in bodies.into_iter().enumerate() {
            assert_eq!(body.kind(), KNOWN_KINDS[i]);
            let ev = Event::new("t1", i as u64 + 1, Some(1), Source::Hub, body);
            let json = serde_json::to_string(&ev).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ev, "round trip of {json}");
        }
    }

    /// The payload parsed straight from its text ([`EventBody::from_kind_json`]) is what the
    /// `Value` way builds ([`EventBody::from_kind_data`]), for every kind, with unknown fields,
    /// `null`, an unknown kind and a key given twice (where a `Value` keeps the last); and both
    /// refuse what the other refuses.
    #[test]
    fn a_payload_parsed_from_its_text_is_the_one_a_value_gives() {
        let value_way = |kind: &str, json: &str| {
            serde_json::from_str::<Value>(json).and_then(|v| EventBody::from_kind_data(kind, v))
        };
        let mut cases: Vec<(String, String)> = all_bodies()
            .into_iter()
            .map(|b| {
                let mut data = b.data().unwrap();
                if let Some(m) = data.as_object_mut() {
                    m.insert("from_a_later_build".into(), json!({"x": [1, 2.5, null]}));
                }
                (b.kind().to_string(), data.to_string())
            })
            .collect();
        for kind in KNOWN_KINDS {
            cases.push((kind.to_string(), "null".into()));
            cases.push((kind.to_string(), "{}".into()));
            cases.push((kind.to_string(), "[1]".into()));
            cases.push((kind.to_string(), "{\"text\": 1}".into()));
        }
        cases.push(("agent.text".into(), r#"{"text":"a","text":"b"}"#.into()));
        cases.push((
            "thread.renamed".into(),
            r#"{"name":"x","name":"y","by":"user"}"#.into(),
        ));
        cases.push(("from.a.later.build".into(), r#"{"a":1,"a":2}"#.into()));
        cases.push(("agent.text".into(), "{".into()));
        for (kind, json) in &cases {
            let direct = EventBody::from_kind_json(kind, Some(json));
            match (direct, value_way(kind, json)) {
                (Ok(d), Ok(v)) => assert_eq!(d, v, "{kind} {json}"),
                (Err(_), Err(_)) => {}
                (d, v) => panic!("{kind} {json}: {d:?} but {v:?}"),
            }
        }
        assert_eq!(
            EventBody::from_kind_json("agent.text", None).unwrap(),
            EventBody::from_kind_data("agent.text", Value::Null).unwrap()
        );
        // The envelope too, `null` and missing `data` alike.
        let ev = Event::new(
            "t1",
            1,
            None,
            Source::Hub,
            EventBody::AgentText(Default::default()),
        );
        let mut v = serde_json::to_value(&ev).unwrap();
        v["data"] = Value::Null;
        assert_eq!(serde_json::from_value::<Event>(v.clone()).unwrap(), ev);
        v.as_object_mut().unwrap().remove("data");
        assert_eq!(serde_json::from_str::<Event>(&v.to_string()).unwrap(), ev);
    }

    /// [`EventBody::maybe_capped`] is never `false` for a payload with a field moved to the
    /// blob store, wherever the marker is.
    #[test]
    fn maybe_capped_never_misses_a_capped_payload() {
        use crate::blob::{cap_payload, has_capped, MemoryBlobStore};
        let store = MemoryBlobStore::new();
        for body in all_bodies() {
            assert_eq!(
                body.maybe_capped(),
                has_capped(&body.data().unwrap()),
                "{body:?}"
            );
            let mut data = body.data().unwrap();
            if let Some(m) = data.as_object_mut() {
                m.insert("big".into(), json!("x".repeat(10_000)));
                m.insert("nested".into(), json!({"list": vec!["y".repeat(100); 100]}));
            }
            cap_payload(&mut data, &store).unwrap();
            let capped = EventBody::from_kind_data(body.kind(), data.clone()).unwrap();
            if has_capped(&capped.data().unwrap()) {
                assert!(capped.maybe_capped(), "{}", body.kind());
            }
        }
    }

    #[test]
    fn golden_envelope_shape() {
        let ev = Event {
            v: 1,
            id: "01J00000000000000000000000".into(),
            seq: 3,
            ts: DateTime::parse_from_rfc3339("2026-09-29T12:00:00.5Z")
                .unwrap()
                .with_timezone(&Utc),
            thread: "t1".into(),
            turn: Some(2),
            source: Source::Adapter("fake".into()),
            body: EventBody::AgentText(AgentText {
                text: "hello".into(),
                delta: false,
                is_final: true,
                ..Default::default()
            }),
            raw: None,
        };
        let s = serde_json::to_string(&ev).unwrap();
        assert_eq!(
            s,
            r#"{"v":1,"id":"01J00000000000000000000000","seq":3,"ts":"2026-09-29T12:00:00.500Z","thread":"t1","turn":2,"source":"adapter:fake","kind":"agent.text","data":{"text":"hello","delta":false,"final":true},"raw":null}"#
        );
    }

    #[test]
    fn golden_payloads() {
        let body = EventBody::ThreadCreated(ThreadCreated {
            name: "api".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            json!({"kind":"thread.created","data":{"name":"api","project":null,"repo":null,
                "worktree":null,"box":"local","runtime":"fake","mode":"manual","aliases":[]}})
        );
        let body = EventBody::AttentionResolved(AttentionResolved {
            attn_id: "a".into(),
            by: ResolvedBy::AutoRetry,
            answer: None,
            ..Default::default()
        });
        assert_eq!(
            body.data().unwrap(),
            json!({"attn_id":"a","by":"auto_retry","answer":null})
        );
        let body = EventBody::DecisionMade(DecisionMade {
            site: DecisionSite::BlockKind,
            provider: DecisionProvider::Orchestrator,
            ..Default::default()
        });
        let d = body.data().unwrap();
        assert_eq!(d["site"], "block_kind");
        assert_eq!(d["provider"], "orchestrator");
        let body = EventBody::ReportReady(ReportReady {
            grade: Grade::GaveUp,
            ..Default::default()
        });
        assert_eq!(body.data().unwrap()["grade"], "gave_up");
        let body = EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Orchestrator,
            ..Default::default()
        });
        assert_eq!(body.data().unwrap()["from"], "orchestrator");
        let body = EventBody::TurnEnded(TurnEnded::default());
        assert_eq!(body.data().unwrap()["result"], "completed");
        let body = EventBody::AttentionRaised(AttentionRaised::default());
        assert_eq!(body.data().unwrap()["reason"], "permission");
        let body = EventBody::ThreadModeChanged(ThreadModeChanged {
            mode: "auto".into(),
            previous: "manual".into(),
            by: ChangedBy::User,
            ..Default::default()
        });
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            json!({"kind":"thread.mode_changed",
                "data":{"mode":"auto","previous":"manual","by":"user"}})
        );
        assert!(body.is_control());
        // A cleared value is null; a log line without the effort fields reads them as `None`.
        let body = EventBody::ThreadModelChanged(ThreadModelChanged {
            model: Some("claude-fable-5-1".into()),
            effort: None,
            previous_model: None,
            previous_effort: Some("high".into()),
            by: ChangedBy::Orchestrator,
            ..Default::default()
        });
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(
            v,
            json!({"kind":"thread.model_changed",
                "data":{"model":"claude-fable-5-1","effort":null,"previous_model":null,
                    "previous_effort":"high","by":"orchestrator"}})
        );
        assert!(body.is_control());
        let old = EventBody::from_kind_data("thread.model_changed", json!({"model": "m"}));
        let EventBody::ThreadModelChanged(p) = old.unwrap() else {
            panic!()
        };
        assert_eq!(
            (p.model.as_deref(), p.effort, p.by),
            (Some("m"), None, ChangedBy::User)
        );
    }

    /// Confirm cards and delete cards are both cards only the user answers; only the hub's
    /// own thread holds them.
    #[test]
    fn delete_cards_are_confirm_cards() {
        let ext = |v: Value| v.as_object().cloned();
        let delete = ext(json!({"twr": {"delete": {"threads": [{"thread": "t1", "name": "a"}]}}}));
        let proposal = ext(json!({"twr": {"proposal": {"actions": []}}}));
        assert!(is_delete_card(HUB_THREAD, &delete) && is_confirm_card(HUB_THREAD, &delete));
        assert!(!is_delete_card(HUB_THREAD, &proposal) && is_confirm_card(HUB_THREAD, &proposal));
        assert!(!is_delete_card("t1", &delete) && !is_confirm_card("t1", &delete));
        let null = ext(json!({"twr": {"delete": null}}));
        assert!(!is_delete_card(HUB_THREAD, &null) && !is_confirm_card(HUB_THREAD, &null));
        assert!(!is_confirm_card(HUB_THREAD, &None));
    }

    #[test]
    fn settings_restart_stops_are_recognized() {
        let mut p = SessionPayload::default();
        assert!(!is_settings_restart(&p));
        for reason in [
            MODE_RESTART_REASON,
            MODEL_RESTART_REASON,
            TRIM_RESTART_REASON,
            RUNTIME_RESTART_REASON,
            BOX_UPGRADE_REASON,
        ] {
            let mut ext = JsonMap::new();
            ext.insert("twr".into(), json!({ "reason": reason }));
            p.ext = Some(ext);
            assert!(is_settings_restart(&p), "{reason}");
        }
        let mut ext = JsonMap::new();
        ext.insert("twr".into(), json!({"reason": "hub_shutdown"}));
        p.ext = Some(ext);
        assert!(!is_settings_restart(&p));
    }

    #[test]
    fn source_strings() {
        let cases = [
            (Source::Adapter("claude-code".into()), "adapter:claude-code"),
            (Source::Hub, "hub"),
            (Source::User("tui".into()), "user:tui"),
            (Source::Orchestrator, "orchestrator"),
            (Source::Decision("code".into()), "decision:code"),
        ];
        for (src, s) in cases {
            assert_eq!(src.to_string(), s);
            assert_eq!(serde_json::to_value(&src).unwrap(), json!(s));
            assert_eq!(s.parse::<Source>().unwrap(), src);
        }
        for bad in ["", "adapter", "adapter:", "robot:x", "HUB"] {
            assert!(bad.parse::<Source>().is_err(), "{bad}");
        }
    }

    #[test]
    fn unknown_kind_is_preserved() {
        let s = r#"{"v":1,"id":"x","seq":1,"ts":"2026-09-29T12:00:00Z","thread":"t","turn":null,"source":"hub","kind":"future.thing","data":{"a":[1,2]},"raw":null}"#;
        let ev: Event = serde_json::from_str(s).unwrap();
        assert_eq!(
            ev.body,
            EventBody::Unknown {
                kind: "future.thing".into(),
                data: json!({"a":[1,2]})
            }
        );
        assert_eq!(ev.kind(), "future.thing");
        let again = serde_json::to_value(&ev).unwrap();
        assert_eq!(again["kind"], "future.thing");
        assert_eq!(again["data"], json!({"a":[1,2]}));
    }

    #[test]
    fn malformed_known_kind_is_an_error() {
        let s = r#"{"v":1,"id":"x","seq":1,"ts":"2026-09-29T12:00:00Z","thread":"t","turn":null,"source":"hub","kind":"agent.text","data":{"text":5},"raw":null}"#;
        assert!(serde_json::from_str::<Event>(s).is_err());
        let s = r#"{"v":1,"id":"x","seq":1,"ts":"2026-09-29T12:00:00Z","thread":"t","turn":null,"source":"hub","kind":"turn.ended","data":{"result":"exploded"},"raw":null}"#;
        assert!(serde_json::from_str::<Event>(s).is_err());
    }

    #[test]
    fn unknown_fields_and_refs_survive_round_trip() {
        let data = json!({"text":"short","delta":false,"final":true,"text_ref":"sha256:ab",
            "ext":{"claude-code":{"id":"m1"}},"future_field":7});
        let body = EventBody::from_kind_data("agent.text", data.clone()).unwrap();
        let EventBody::AgentText(t) = &body else {
            panic!("wrong variant")
        };
        assert_eq!(t.extra.get("text_ref"), Some(&json!("sha256:ab")));
        assert_eq!(t.ext.as_ref().unwrap()["claude-code"]["id"], "m1");
        assert_eq!(body.data().unwrap(), data);
    }

    #[test]
    fn missing_fields_default_and_null_data_is_accepted() {
        let body = EventBody::from_kind_data("thread.stopped", Value::Null).unwrap();
        assert_eq!(body, EventBody::ThreadStopped(SessionPayload::default()));
        let body = EventBody::from_kind_data("turn.ended", json!({})).unwrap();
        assert_eq!(body, EventBody::TurnEnded(TurnEnded::default()));
    }

    #[test]
    fn ts_accepts_offsets_and_normalizes_to_utc() {
        let s = r#"{"v":1,"id":"x","seq":1,"ts":"2026-09-29T14:00:00+02:00","thread":"t","source":"hub","kind":"thread.stopped","data":{}}"#;
        let ev: Event = serde_json::from_str(s).unwrap();
        let out = serde_json::to_value(&ev).unwrap();
        assert_eq!(out["ts"], "2026-09-29T12:00:00.000Z");
        assert_eq!(out["turn"], Value::Null);
        assert_eq!(out["raw"], Value::Null);
    }

    #[test]
    fn withdrawals_carry_the_messages_ref() {
        let body = EventBody::TurnWithdrawn(TurnWithdrawn {
            msg_ref: 7,
            turn: 2,
            ..Default::default()
        });
        assert_eq!(body.data().unwrap(), json!({"ref": 7, "turn": 2}));
        let body = EventBody::TurnInterrupted(TurnInterrupted {
            msg_ref: 9,
            turn: 3,
            ..Default::default()
        });
        assert_eq!(body.data().unwrap(), json!({"ref": 9, "turn": 3}));
        let back = EventBody::from_kind_data("prompt.withdrawn", json!({"ref": 8, "prompt": "x"}));
        assert_eq!(
            back.unwrap(),
            EventBody::PromptWithdrawn(PromptWithdrawn {
                msg_ref: 8,
                prompt: "x".into(),
                ..Default::default()
            })
        );
        let started = TurnStarted {
            ext: Some(json!({"twr": {"ref": 12}}).as_object().unwrap().clone()),
            ..Default::default()
        };
        assert_eq!(message_ref(&started), Some(12));
        assert_eq!(message_ref(&TurnStarted::default()), None);
    }

    #[test]
    fn control_filter() {
        let yes = [
            "thread.created",
            "project.read",
            "thread.crashed",
            "thread.attached",
            "thread.renamed",
            "turn.started",
            "turn.ended",
            "turn.withdrawn",
            "turn.interrupted",
            "prompt.withdrawn",
            "task.updated",
            "attention.raised",
            "attention.annotated",
            "attention.resolved",
            "decision.made",
            "report.ready",
            "git.snapshot",
            "runner.connected",
            "runner.disconnected",
            "context.updated",
            "account.usage",
            "worktree.disk",
            "worktree.trimmed",
        ];
        let no = [
            "runtime.limits",
            "agent.text",
            "agent.tool_call",
            "agent.tool_result",
            "agent.thinking",
            "turn.other",
            "future.thing",
        ];
        for k in yes {
            assert!(is_control_kind(k), "{k}");
        }
        for k in no {
            assert!(!is_control_kind(k), "{k}");
        }
        for body in all_bodies() {
            // A usage report is neither: the hub folds it and never records it.
            let recorded = body.kind() != "runtime.limits";
            assert_eq!(
                body.is_control(),
                recorded && !body.is_agent(),
                "{}",
                body.kind()
            );
        }
    }

    #[test]
    fn body_serializes_as_kind_and_data() {
        let body = EventBody::AgentThinking(AgentThinking {
            text: "t".into(),
            ..Default::default()
        });
        let v = serde_json::to_value(&body).unwrap();
        assert_eq!(v, json!({"kind":"agent.thinking","data":{"text":"t"}}));
        let back: EventBody = serde_json::from_value(v).unwrap();
        assert_eq!(back, body);
    }

    #[test]
    fn reason_priority_order() {
        use AttentionReason::*;
        let mut v = vec![Stalled, Review, Error, Question, Permission];
        v.sort();
        assert_eq!(v, vec![Permission, Question, Error, Review, Stalled]);
        assert_eq!(Permission.priority(), 0);
        assert_eq!(Stalled.priority(), 4);
        assert!(Question.is_blocking() && !Error.is_blocking());
        assert!(Grade::GaveUp.needs_review() && !Grade::Partial.needs_review());
    }

    /// A twr from before runtime turns decodes `turn.started` with the `TurnFrom` below: a
    /// runtime's turn is an error to it, which is why such a client is refused at `hello`
    /// ([`crate::proto::PROTOCOL_VERSION`]) and such a hub refuses the database (schema 7).
    #[test]
    fn a_decoder_from_before_runtime_turns_cannot_read_one() {
        #[derive(Debug, Default, Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum PreviousTurnFrom {
            #[default]
            User,
            Orchestrator,
        }
        #[derive(Debug, Default, Deserialize)]
        #[serde(default)]
        #[allow(dead_code)]
        struct PreviousTurnStarted {
            prompt: String,
            from: PreviousTurnFrom,
            #[serde(flatten)]
            extra: JsonMap,
        }
        let data = |from| {
            EventBody::TurnStarted(TurnStarted {
                prompt: "go".into(),
                from,
                ..Default::default()
            })
            .data()
            .unwrap()
        };
        for from in [TurnFrom::User, TurnFrom::Orchestrator] {
            assert!(serde_json::from_value::<PreviousTurnStarted>(data(from)).is_ok());
        }
        assert!(serde_json::from_value::<PreviousTurnStarted>(data(TurnFrom::Runtime)).is_err());
        assert_eq!(
            EventBody::from_kind_data("turn.started", data(TurnFrom::Runtime)).unwrap(),
            EventBody::TurnStarted(TurnStarted {
                prompt: "go".into(),
                from: TurnFrom::Runtime,
                ..Default::default()
            })
        );
    }
}

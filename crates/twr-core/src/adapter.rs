//! The adapter trait: how twr drives an agent runtime.
//!
//! An adapter turns a runtime CLI into a stream of normalized [`EventBody`] values. The hub
//! stamps envelopes (id, seq, ts, thread, turn, `source = adapter:<name>`). Handles are
//! channel-based so no async trait is needed: [`Adapter::spawn`] starts the runtime (on the
//! current tokio runtime) and returns an [`AdapterHandle`] carrying a command sender and an
//! event receiver.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;

pub use crate::event::Answer;
use crate::event::EventBody;

/// How faithfully an adapter observes its runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Structured events from a headless JSON mode.
    Headless,
    /// The runtime reports state via hooks.
    Hooks,
    /// Heuristics over terminal output (degraded).
    Pty,
}

/// Everything an adapter needs to start (or resume) a runtime session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpawnConfig {
    /// Thread id this session belongs to.
    pub thread: String,
    /// Working directory (usually the thread's worktree).
    pub cwd: PathBuf,
    /// Runtime permission mode.
    pub mode: String,
    /// Model override.
    pub model: Option<String>,
    /// Reasoning effort override (`None`: the runtime's default), in the runtime's own
    /// vocabulary (the hub checks it, `twr_adapters::check_effort`).
    pub effort: Option<String>,
    /// Text appended to the runtime's system prompt.
    pub system_append: Option<String>,
    /// Rebuild the system prompt on resume when the runtime supports it (orchestrators).
    pub refresh_system_prompt: bool,
    /// MCP server configuration to attach.
    pub mcp_config: Option<Value>,
    /// A file holding the MCP server configuration, used instead of `mcp_config` when set.
    /// The hub uses it when the configuration carries a secret (the orchestrator token): a
    /// command line is readable by every local user, a 0600 file only by its owner.
    pub mcp_config_file: Option<PathBuf>,
    /// Tools the runtime may use without asking (empty = the runtime's default rules).
    pub allowed_tools: Vec<String>,
    /// Built-in tools the runtime offers at all: `None` = the runtime's default set,
    /// `Some(empty)` = none (MCP tools stay available). The orchestrator runs with none, so it
    /// cannot run commands whatever the user's own permission rules allow.
    pub builtin_tools: Option<Vec<String>>,
    /// Claude Code `--settings` (JSON), layered over the user's own settings: the Claude Code
    /// orchestrator's file guard hook (`twr_adapters::claude_code::guard`). Other runtimes
    /// ignore it.
    pub settings: Option<Value>,
    /// Emit `agent.thinking` events.
    pub thinking: bool,
    /// Extra environment variables for the runtime process. Adapters must pass them through;
    /// the hub sets `TWR_THREAD=<thread id>` here to recognize the process later, and
    /// `TWR_AGENT=<thread id>:<generation>` so its socket tells the process (and whatever it
    /// starts) from the user.
    pub env: Vec<(String, String)>,
    /// Session to resume instead of starting a new one.
    pub resume_session: Option<String>,
    /// Turn number of the initial `prompt` passed to [`Adapter::spawn`] (default 1).
    pub prompt_turn: Option<u64>,
    /// A command line the runtime's process runs under (`systemd-run ... --`, `nice -n 7`):
    /// its first word is the program started, the runtime's own program and arguments follow
    /// its last. Empty (the default): the runtime's program itself. The hub sets it for a
    /// local worker under `[threads] cpu_weight` or `memory_high`; it never goes to a runner.
    /// Only the headless session's process runs under it, not `attach`.
    pub wrap: Vec<String>,
    /// The repository the thread's worktree was made from (`ThreadView::repo`, recorded when
    /// the thread was created; a path on the box the runtime runs on). What the worktree's own
    /// git pointers (its `.git` file, its git dir's `commondir`) say is checked against it,
    /// since the agent can rewrite them: the Codex `auto` sandbox makes git metadata writable
    /// only where both agree (`twr_adapters::codex::auto_writable_roots`). `None`: not known
    /// (no git metadata is made writable).
    pub repo: Option<PathBuf>,
    /// What a Codex thread in `auto` does with a command its sandbox does not allow (`[codex]
    /// auto`, `[projects.<name>] codex_auto`). Other runtimes and modes ignore it.
    pub codex_auto: CodexAuto,
    /// Turn the runtime's own cross-session memory off (the orchestrator's: it keeps twr's
    /// memory instead, `twr-hub`'s `hub::memory`): Codex's `memories` feature and Grok
    /// Build's memory (`GROK_MEMORY=0`). Claude Code ignores it (its orchestrator keeps its
    /// own memory directory). Never sent to a runner.
    pub native_memory_off: bool,
}

/// What a Codex thread in `auto` mode does with a command its sandbox does not allow, when
/// Codex asks to run it outside the sandbox: the config's `[codex] auto`, over which a
/// project's `codex_auto` wins (`twr_adapters::codex`, "The `auto` sandbox"). The config's
/// default is [`CodexAuto::AutoReview`] (`twr_core::config::CodexConfig`); this type's own
/// default, [`CodexAuto::Ask`], is what a thread runs with when nothing names a value: the
/// orchestrator, and a runner told nothing by its hub.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexAuto {
    /// Codex asks the user: a permission card (`approvalPolicy: on-request`, reviewer `user`).
    #[default]
    Ask,
    /// Codex's own reviewer agent decides, and the user is not asked (`approvalsReviewer:
    /// auto_review`): what it approves runs outside the sandbox.
    AutoReview,
    /// Nobody is asked (`approvalPolicy: never`): what the sandbox refuses fails.
    Never,
}

impl CodexAuto {
    /// Every value, as `[codex] auto` spells it.
    pub const ALL: [CodexAuto; 3] = [CodexAuto::Ask, CodexAuto::AutoReview, CodexAuto::Never];

    /// The config spelling: `ask`, `auto_review`, `never`.
    pub fn as_str(self) -> &'static str {
        match self {
            CodexAuto::Ask => "ask",
            CodexAuto::AutoReview => "auto_review",
            CodexAuto::Never => "never",
        }
    }

    /// The value a config spelling names; None for anything else.
    pub fn parse(s: &str) -> Option<CodexAuto> {
        CodexAuto::ALL.into_iter().find(|v| v.as_str() == s)
    }
}

/// Static description of an adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterInfo {
    /// Runtime name (`fake`, `claude-code`, ...), used in `source = adapter:<name>`.
    pub name: &'static str,
    /// Fidelity tier.
    pub tier: Tier,
    /// One-line description.
    pub description: &'static str,
}

/// Which of a [`SpawnConfig`]'s settings a session runs with once the runtime's interactive
/// CLI has resumed it through [`Adapter::attach_command`] (or
/// [`Adapter::remote_attach_command`]) for that configuration ([`Adapter::attach_applies`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttachApplies {
    /// [`SpawnConfig::model`].
    pub model: bool,
    /// [`SpawnConfig::effort`].
    pub effort: bool,
}

/// Commands from the hub to a running adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterCmd {
    /// Start turn `turn` with `text` as the prompt. Adapters must accept this while another
    /// turn runs (queue internally or rely on the runtime's own queue), although the hub
    /// serializes turns and only sends the next prompt after `turn.ended`.
    Send {
        /// Prompt text.
        text: String,
        /// Turn number assigned by the hub; echoed on the turn's events.
        turn: u64,
    },
    /// Answer an attention item the adapter raised.
    Respond {
        /// The adapter's own attention id, as it raised it (the hub strips its thread
        /// namespace, see [`hub_attn_id`]).
        attn_id: String,
        /// The answer.
        answer: Answer,
    },
    /// Cancel only the current turn, keeping the runtime session alive.
    Interrupt,
    /// Start turn `turn` as a compaction of the session's context (the user's `/compact`):
    /// Claude Code and Grok Build get `/compact` as the turn's message, Codex
    /// `thread/compact/start`. Like a prompt, the hub sends it only between turns. Only an
    /// adapter that [`Adapter::compacts`] is sent one; any other ends the turn `error`.
    Compact {
        /// Turn number assigned by the hub; echoed on the turn's events.
        turn: u64,
    },
    /// Stop the runtime (interrupting any running turn).
    Stop,
}

/// Output of an adapter.
// `Emit` is the hot variant and `Exited` happens once per session, so boxing `body` would only
// add an allocation per event.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterEvent {
    /// A normalized event.
    Emit {
        /// Event kind + payload.
        body: EventBody,
        /// Raw runtime line, if the adapter keeps it (stored as a blob by the hub).
        raw: Option<String>,
        /// Turn this event belongs to (None for out-of-turn events like `thread.started`).
        turn: Option<u64>,
    },
    /// The runtime process exited. Always the last event.
    Exited {
        /// Exit code, None if killed by a signal.
        code: Option<i32>,
        /// Last bytes of stderr.
        stderr_tail: String,
    },
}

/// Error returned when the adapter behind a handle is gone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("adapter is no longer running")]
pub struct AdapterGone;

/// The hub's side of a running adapter session.
#[derive(Debug)]
pub struct AdapterHandle {
    cmd: mpsc::UnboundedSender<AdapterCmd>,
    events: Option<mpsc::UnboundedReceiver<AdapterEvent>>,
    /// The process the adapter started, 0 until it says ([`AdapterEnd::spawned`]).
    pid: Arc<AtomicU32>,
}

/// The adapter's side of a session: receives commands, sends events.
#[derive(Debug)]
pub struct AdapterEnd {
    /// Commands from the hub.
    pub cmds: mpsc::UnboundedReceiver<AdapterCmd>,
    /// Events to the hub.
    pub events: mpsc::UnboundedSender<AdapterEvent>,
    /// Shared with the handle ([`AdapterHandle::pid`]).
    pid: Arc<AtomicU32>,
}

impl AdapterEnd {
    /// Says which process the adapter started for the session, as soon as its spawn returns
    /// (before the runtime reports anything): the hub counts that process and what it starts
    /// as the thread's agent from then on, not only once `thread.started` records it.
    pub fn spawned(&self, pid: u32) {
        self.pid.store(pid, Ordering::SeqCst);
    }

    /// Sends an in-turn or out-of-turn event. Returns false if the hub side is gone.
    pub fn emit(&self, body: EventBody, turn: Option<u64>) -> bool {
        self.events
            .send(AdapterEvent::Emit {
                body,
                raw: None,
                turn,
            })
            .is_ok()
    }

    /// Sends the final `Exited` event.
    pub fn exited(&self, code: Option<i32>, stderr_tail: impl Into<String>) -> bool {
        self.events
            .send(AdapterEvent::Exited {
                code,
                stderr_tail: stderr_tail.into(),
            })
            .is_ok()
    }
}

impl AdapterHandle {
    /// Cancel the current turn while retaining the runtime session.
    pub fn interrupt(&self) -> Result<(), AdapterGone> {
        self.command(AdapterCmd::Interrupt)
    }

    /// Creates a connected handle/end pair.
    pub fn pair() -> (AdapterHandle, AdapterEnd) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let pid = Arc::new(AtomicU32::new(0));
        (
            AdapterHandle {
                cmd: cmd_tx,
                events: Some(ev_rx),
                pid: Arc::clone(&pid),
            },
            AdapterEnd {
                cmds: cmd_rx,
                events: ev_tx,
                pid,
            },
        )
    }

    /// The process the adapter started for the session, once it said
    /// ([`AdapterEnd::spawned`]); `None` before, and for an adapter that starts none here (a
    /// remote box's session, an in-process runtime).
    pub fn pid(&self) -> Option<u32> {
        Some(self.pid.load(Ordering::SeqCst)).filter(|p| *p != 0)
    }

    /// Sends a prompt as turn `turn`.
    pub fn send(&self, text: impl Into<String>, turn: u64) -> Result<(), AdapterGone> {
        self.command(AdapterCmd::Send {
            text: text.into(),
            turn,
        })
    }

    /// Starts turn `turn` as a compaction of the session's context ([`AdapterCmd::Compact`]).
    pub fn compact(&self, turn: u64) -> Result<(), AdapterGone> {
        self.command(AdapterCmd::Compact { turn })
    }

    /// Answers attention item `attn_id`.
    pub fn respond(&self, attn_id: impl Into<String>, answer: Answer) -> Result<(), AdapterGone> {
        self.command(AdapterCmd::Respond {
            attn_id: attn_id.into(),
            answer,
        })
    }

    /// Asks the runtime to stop.
    pub fn stop(&self) -> Result<(), AdapterGone> {
        self.command(AdapterCmd::Stop)
    }

    /// Sends a raw command.
    pub fn command(&self, cmd: AdapterCmd) -> Result<(), AdapterGone> {
        self.cmd.send(cmd).map_err(|_| AdapterGone)
    }

    /// Takes the event receiver (once), e.g. to move it into a supervisor task.
    pub fn take_events(&mut self) -> Option<mpsc::UnboundedReceiver<AdapterEvent>> {
        self.events.take()
    }

    /// A clone of the command sender.
    pub fn commander(&self) -> mpsc::UnboundedSender<AdapterCmd> {
        self.cmd.clone()
    }
}

/// An agent runtime. Adding a runtime means implementing this in one file and adding it to
/// `twr_adapters::registry()`.
///
/// `spawn` and `resume` are called inside a tokio runtime and must not block; adapters run
/// their work in spawned tasks and talk through the handle's channels. The first event of a
/// session is `thread.started { session_id }`; the last is [`AdapterEvent::Exited`].
///
/// Adapters that run a runtime process start it in its own process group (pgid = pid) with
/// [`SpawnConfig::env`] in its environment, and report its pid as `ext.twr.pid` on
/// `thread.started` / `thread.resumed`. The hub adds `ext.twr.start_time` and uses the three
/// to kill the process safely if it outlives a crashed hub.
///
/// The hub records only these adapter kinds: `thread.started`, `thread.resumed`, `agent.*`,
/// `attention.raised`, `task.updated` and `turn.ended`, plus one narrow form of
/// `attention.resolved` and of `turn.started` (below). Everything else is dropped:
/// `thread.stopped` / `thread.crashed` are the hub's to write when the adapter reports
/// [`AdapterEvent::Exited`], and reports, decisions and turn starts are the hub's too. In-turn
/// events without a turn get the open turn, and a `turn.ended` for a turn that is not open is
/// dropped.
///
/// Runtime turns: a turn the runtime starts on its own (after a background task, say) is a
/// `turn.started{from: runtime}` with no turn, and its events carry none either: the hub alone
/// numbers turns and gives it the next one. Its start and its `turn.ended` carry the adapter's
/// id for it ([`crate::event::RUNTIME_TURN_KEY`]). The hub drops that start while a turn of its
/// own is open (its prompt is on its way to the adapter): the runtime turn's events then go to
/// the open turn, and its end, which closes only the turn its start opened, is dropped (its
/// usage counts for the open turn). A request of the runtime's session rather than of the turn
/// (a Codex child agent's) is marked [`crate::event::SESSION_SCOPE_KEY`]: no turn's end closes
/// it.
///
/// Relay rule: an adapter never answers an attention item. The only `attention.resolved` it may
/// emit is a withdrawal: `by: timeout` with `answer: null`, for an item that this thread's
/// runtime raised and that is still open (the runtime cancelled its own request, e.g. Claude's
/// `control_cancel_request`). Any other `attention.resolved` from an adapter is dropped. When
/// the open turn ends, the hub itself resolves (`by: timeout`) every permission / question item
/// the runtime raised that is still open: a runtime cannot still be blocked on a request once
/// its turn is over.
///
/// Attention ids: an adapter's ids only need to be unique among its own session's open items
/// (runtimes number their requests per process, e.g. grok's `perm-<rpc id>`, so two threads,
/// or one thread's resumed session, raise the same id). The hub records every adapter-raised
/// id in its thread's namespace, `<thread id>:<adapter id>` ([`hub_attn_id`]), for local and
/// remote runtimes alike (a thread lives on exactly one box, so the thread id also names the
/// box). That covers `attention.raised` and an adapter's withdrawal (`attention.resolved`):
/// the adapter emits its own id and the hub prefixes it. [`AdapterCmd::Respond`] carries the
/// adapter's own id back ([`adapter_attn_id`]). Clients only ever see the namespaced id and
/// treat it as opaque. Ids the hub raises itself (hub questions, review, error, stalled) are
/// ULIDs and are not namespaced.
///
/// Open items are immutable: an adapter never changes what an open item asks under the same
/// id. When its runtime replaces a pending request (asks again under the same request id), the
/// adapter withdraws the old item (`attention.resolved{by: timeout, answer: null}` with
/// `ext.<runtime>.superseded = true`) and raises the new content under a new id (an
/// incarnation, `<request id>~<n>`); it keeps, per open id, exactly the content that id was
/// raised with, builds the runtime's answer only from the content bound to the answered id,
/// and ignores a [`AdapterCmd::Respond`] for an id that is withdrawn or unknown. So an answer
/// that crossed a replacement in flight (the hub had not recorded the withdrawal yet when the
/// user answered) can never answer the replacement. The hub still treats an open id raised
/// again as a new revision of the item (`AttentionStack`, answers name the revision they
/// answer): defense in depth for re-raises that do not come from an adapter's translation
/// (a runner replaying its spool, a scripted test adapter). The namespaced id is held to the hub's identifier limit, so an
/// adapter id may use up to that limit minus the thread id and the `:`.
pub trait Adapter: Send + Sync + 'static {
    /// Static description.
    fn info(&self) -> AdapterInfo;

    /// Starts a session. With `prompt`, a turn starts immediately, numbered
    /// `cfg.prompt_turn` (default 1); later turns arrive via [`AdapterCmd::Send`].
    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle>;

    /// Resumes an existing session. Default: `spawn` with `resume_session` set.
    fn resume(&self, cfg: SpawnConfig, session_id: String) -> anyhow::Result<AdapterHandle> {
        self.spawn(
            SpawnConfig {
                resume_session: Some(session_id),
                ..cfg
            },
            None,
        )
    }

    /// The command that opens the runtime's own interactive CLI on `session_id` (`twr attach`),
    /// or `None` if the runtime has none. The hub has stopped the headless session before the
    /// command runs; the CLI runs it in the user's terminal with inherited stdio. The program,
    /// arguments, working directory and any environment set on the command are passed to the
    /// CLI; `cfg.env` holds at most the thread's runtime command entries (`TWR_ACP_COMMAND` /
    /// `TWR_ACP_ATTACH` of an `acp` thread), never the hub's markers (the attached process is
    /// the user's, not the hub's: it carries no `TWR_THREAD` marker, so stale-process reaping
    /// never touches it).
    ///
    /// The command runs on this box: it may be fitted to what is known of this box's binaries
    /// (Claude Code's probed `--help`). A command for another box is
    /// [`Adapter::remote_attach_command`]'s.
    fn attach_command(&self, _cfg: &SpawnConfig, _session_id: &str) -> Option<Command> {
        None
    }

    /// [`Adapter::attach_command`] for a session on another box (the hub's attach to a thread
    /// on a runner's box, run there over ssh): made from `cfg` alone, never fitted to what is
    /// known of this box's binaries (the box's own may be another version), so every setting
    /// is passed and a binary there that does not take one refuses the command line itself.
    /// Default: `attach_command`, for an adapter whose command depends on nothing it learned
    /// of this box; an adapter that fits `attach_command` to this box must override it.
    fn remote_attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<Command> {
        self.attach_command(cfg, session_id)
    }

    /// Which of `cfg`'s settings the session runs with once [`Adapter::attach_command`] (or
    /// [`Adapter::remote_attach_command`]) for `cfg` has resumed it: the command hands the
    /// value to the interactive CLI and the CLI switches a resumed session to it, or the value
    /// is the runtime's default (`None`) and the runtime does not keep the session's own on
    /// resume. The hub refuses an attach whose session last ran with another model or effort
    /// than the thread's when the command does not apply it (the interactive CLI would go on
    /// with the old one). Default: neither.
    fn attach_applies(&self, _cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies::default()
    }

    /// Whether this runtime can be the orchestrator (`twr orchestrator use`): it attaches the
    /// MCP servers of [`SpawnConfig::mcp_config`] / [`SpawnConfig::mcp_config_file`] (the twr
    /// tools) and honors [`SpawnConfig::builtin_tools`] / [`SpawnConfig::allowed_tools`] (the
    /// orchestrator runs with no built-in tools). Default: a [`Tier::Headless`] adapter (its
    /// runtime's structured mode takes MCP servers); a PTY one cannot. An adapter that does
    /// not attach MCP servers overrides this with `false`.
    fn hosts_orchestrator(&self) -> bool {
        self.info().tier == Tier::Headless
    }

    /// Whether [`AdapterCmd::Interrupt`] ends the running turn here and keeps the session:
    /// what lets the hub take back a message whose turn started (`message.withdraw`). Default:
    /// a [`Tier::Headless`] adapter (its runtime's structured mode can cancel a turn); a PTY
    /// one cannot, and an adapter that ignores the command overrides this with `false`.
    fn interrupts_turns(&self) -> bool {
        self.info().tier == Tier::Headless
    }

    /// Whether [`AdapterCmd::Compact`] compacts the session's context here (the user's
    /// `/compact`, `thread.compact`). Default: no; the hub refuses the request naming the
    /// runtime.
    fn compacts(&self) -> bool {
        false
    }

    /// The program this adapter starts (its argv[0], as the spawn would resolve it now: an
    /// override such as a `TWR_*_BIN` variable, else the runtime's binary name) and what to do
    /// when it is not there, the hint of its start error ("install Codex, or set
    /// TWR_CODEX_BIN"). `orchestrator.use` warns with them when the program is missing.
    /// Default `None`: the program comes from the thread's runtime command (`acp`, `pty`), or
    /// the adapter starts none.
    fn program(&self) -> Option<(PathBuf, String)> {
        None
    }
}

/// The hub's id of attention item `adapter_id` raised by thread `thread`'s runtime:
/// `<thread>:<adapter_id>` (see [`Adapter`], attention ids).
pub fn hub_attn_id(thread: &str, adapter_id: &str) -> String {
    format!("{thread}:{adapter_id}")
}

/// The adapter's own id of `hub_id`, an item of thread `thread` ([`hub_attn_id`]); `None` when
/// `hub_id` is not in that thread's namespace (a hub-raised item).
pub fn adapter_attn_id<'a>(thread: &str, hub_id: &'a str) -> Option<&'a str> {
    hub_id.strip_prefix(thread)?.strip_prefix(':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentText, SessionPayload};

    #[test]
    fn attention_ids_are_namespaced_by_thread() {
        let id = hub_attn_id("01T", "req-1#q0");
        assert_eq!(id, "01T:req-1#q0");
        assert_eq!(adapter_attn_id("01T", &id), Some("req-1#q0"));
        // The adapter's own id may hold `:` itself.
        assert_eq!(adapter_attn_id("01T", "01T:a:b"), Some("a:b"));
        assert_eq!(adapter_attn_id("01T", "01TX:a"), None);
        assert_eq!(adapter_attn_id("01T", "01ULIDOFTHEHUB"), None);
        assert_eq!(adapter_attn_id("01U", &id), None);
    }

    #[tokio::test]
    async fn handle_and_end_talk() {
        let (mut h, mut end) = AdapterHandle::pair();
        h.send("hi", 3).unwrap();
        h.respond("a1", Answer::allow()).unwrap();
        h.stop().unwrap();
        assert_eq!(
            end.cmds.recv().await,
            Some(AdapterCmd::Send {
                text: "hi".into(),
                turn: 3
            })
        );
        assert_eq!(
            end.cmds.recv().await,
            Some(AdapterCmd::Respond {
                attn_id: "a1".into(),
                answer: Answer::allow()
            })
        );
        assert_eq!(end.cmds.recv().await, Some(AdapterCmd::Stop));

        let mut rx = h.take_events().unwrap();
        assert!(h.take_events().is_none());
        assert!(end.emit(EventBody::ThreadStarted(SessionPayload::default()), None));
        assert!(end.emit(EventBody::AgentText(AgentText::default()), Some(3)));
        assert!(end.exited(Some(0), ""));
        assert!(matches!(
            rx.recv().await,
            Some(AdapterEvent::Emit { turn: None, .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AdapterEvent::Emit { turn: Some(3), .. })
        ));
        assert!(matches!(
            rx.recv().await,
            Some(AdapterEvent::Exited { code: Some(0), .. })
        ));
    }

    #[test]
    fn send_to_dead_adapter_errors() {
        let (h, end) = AdapterHandle::pair();
        drop(end);
        assert_eq!(h.send("x", 1), Err(AdapterGone));
        assert_eq!(h.stop(), Err(AdapterGone));
    }

    #[test]
    fn tier_serializes_snake_case() {
        assert_eq!(serde_json::to_value(Tier::Headless).unwrap(), "headless");
        assert_eq!(serde_json::to_value(Tier::Pty).unwrap(), "pty");
    }
}

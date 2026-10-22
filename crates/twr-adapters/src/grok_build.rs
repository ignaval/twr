//! Grok Build adapter. Tier: headless (`grok agent stdio`, ACP JSON-RPC over NDJSON).
//!
//! **Verified against a signed-in grok 1.0.44 on 2026-09-30** (recordings in
//! `tests/fixtures/grok/real_*_v1044.both.jsonl`): new sessions, prompts and streamed updates,
//! permission requests (allow once / reject once), questions, plan approval, cancel (also of a
//! running shell command), resume in a new process (the session remembers), MCP servers passed
//! over ACP, and the orchestrator's tool restriction. See [`acp`] for the protocol details and
//! what each observation was. The protocol core and the IO driver are the vendor-neutral
//! [`crate::acp`] ones (the generic `acp:<command>` runtime); [`acp`] is Grok's layer on top.
//!
//! Not verified: `$/cancel_request` and a request reusing a pending id (never observed), a
//! question request with several questions (grok asked one at a time), the `session/load` path
//! (grok 1.0.44 advertises `session/resume`, which is used; a manual `session/load` replayed
//! history as expected), `session/set_mode` narrowing (1.0.44 reports no modes), HTTP/SSE MCP
//! servers, threads on a remote runner box, and the interactive TUI that `attach` opens beyond
//! checking that it resumes the session with the command below.
//!
//! Command:
//!
//! ```text
//! grok --no-auto-update --permission-mode <mode> [--allow <rule>]... [--deny <tool>]...
//!      agent --no-leader stdio
//! ```
//!
//! plus `GROK_DISABLE_AUTOUPDATER=1`. `--no-auto-update`, `--permission-mode`, `--allow` and
//! `--deny` are top-level options (`grok agent` rejects them after `agent`). What grok 1.0.44
//! does with each in `agent stdio` (checked with real prompts):
//!
//! | option / lever | honored? |
//! |---|---|
//! | `--permission-mode default` | yes: permission requests reach twr although the user's `config.toml` says `permission_mode = "always-approve"` (without it, and without `_meta.yoloMode: false`, tools run unasked) |
//! | `_meta.yoloMode: false` on `session/new` | yes, on its own too |
//! | `GROK_CONFIG` `{"ui": {"permission_mode": ...}}` overlay | no: grok's overlay allowlist drops `ui` (so twr no longer sets it) |
//! | `session/set_mode` | accepted for any id, but only toggles plan mode |
//! | `--allow 'Bash(touch *)'` | yes (no prompt) |
//! | `--allow mcp__<server>` / `MCPTool(<server>__*)` / `mcp__<server>__<tool>` | no: MCP calls go through grok's `use_tool` and still ask |
//! | `--deny Bash`, `--deny 'Bash(*)'` | no: the command ran |
//! | `--tools ""`, `--disallowed-tools` | no (headless `-p` only): all built-ins stay |
//! | `--rules` | no; `_meta.rules` on `session/new` yes |
//! | `agent -m <model>` | no (the session keeps the configured default); `session/set_config_option{configId: model}` yes |
//! | `--reasoning-effort <e>` (top-level) | no (checked 2026-10-03: the session still reports `high`); `session/set_config_option{configId: reasoning_effort}` yes |
//! | `_meta.agentProfile: {name, description, tools: [...]}` | yes: the model gets exactly those tools |
//!
//! Even in `default` mode grok runs some shell commands it deems safe without a permission
//! request (verified 2026-09-30: `touch t.txt` ran unasked with `--permission-mode default` and
//! `_meta.yoloMode: false`; file writes ask).
//!
//! So: `system_append` → `_meta.rules`; `model` → `session/set_config_option` (an unknown id
//! fails the session); `effort` → `session/set_config_option{configId: reasoning_effort}`
//! after the model ([`EFFORTS`]; an unknown value fails the session); `allowed_tools` → `--allow` (effective for built-in tool rules such as
//! `Bash(git *)`, not for MCP tools); `builtin_tools: Some(list)` → `_meta.agentProfile` with
//! `tools` = grok's MCP meta-tools (`search_tool`, `use_tool`) plus the grok names of the
//! listed tools ([`profile_tools`]). With `Some([])` (the orchestrator) the model has no
//! shell, file or web tool at all (verified: asked to run `ls` it reports no shell tool) and
//! still reaches its MCP servers; each MCP call asks for permission unless the mode is
//! bypass (grok ignores MCP allow rules), so a grok orchestrator in `default` mode raises an
//! attention item per twr tool call. Every [`BUILTIN_DENY`] name also gets `--deny` (not
//! enforced by 1.0.44 in `agent stdio`, kept for the TUI `attach` and later versions).
//! `--no-leader` keeps the agent in this process (a shared leader would outlive `stop` and
//! escape the process-group kill).
//!
//! Modes: grok's own ids ([`NATIVE_MODES`]) pass as written; another known mode maps by its
//! level (restricted → `plan`, ask → `default`, ...); an unknown mode is refused (the runtime
//! does not start).
//!
//! Permission relay: unless the twr mode is `bypassPermissions`, `--permission-mode` and
//! `_meta.yoloMode: false` keep always-approve off, and a session grok reports as `yolo`
//! anyway is cancelled and ended ([`acp`]). The process gets twr's secrets removed from its
//! environment and stdout lines are capped at [`crate::process::MAX_LINE`] (a skipped prompt
//! response still ends its turn).
//!
//! Mapping ([`acp`] has the full table): `agent_message_chunk` → `agent.text` (deltas + one
//! final per run), `agent_thought_chunk` → `agent.thinking` (with `cfg.thinking`), `tool_call` /
//! `tool_call_update` → `agent.tool_call` / `agent.tool_result` (a `use_tool` call, which runs
//! an MCP server's tool, is summarized as that tool, `<server>__<tool>` then its input, named in
//! `ext.grok.mcp_tool`, and its result as the tool's text), `session/request_permission`
//! → `attention.raised{permission}` (`allow` → first `allow_once` option, `deny` → first
//! `reject_once`, else the `cancelled` outcome; never `allow_always` or `reject_always`),
//! `_x.ai/ask_user_question` → `attention.raised{question}`, `_x.ai/exit_plan_mode` →
//! `attention.raised{permission}` (plan approval), `session/prompt` result → `turn.ended`
//! (`end_turn` → completed, `cancelled` → interrupted, other stop reasons → error; a rejected
//! permission ends grok's turn as `cancelled`).
//!
//! MCP servers: `SpawnConfig::mcp_config_file` (read at spawn) or `mcp_config`, in
//! Claude's `{"mcpServers": {name: {command, args, env}}}` shape (or already an ACP list),
//! become the ACP `mcpServers` list of `session/new|load|resume` (`{name, command, args, env:
//! [{name, value}]}`, verified: grok connects and lists the server's tools), sent over stdin
//! so a token in the config never shows on a command line. grok also loads the user's own MCP
//! servers (its config, and Claude's) into every session. `attach` runs
//! `grok <the same permission options> --resume <session>` with `GROK_DISABLE_AUTOUPDATER=1`.
//!
//! Commands: `send` → `session/prompt` (queued while another prompt runs or the session is
//! opening); `respond` → the JSON-RPC response to the pending request; `stop` →
//! `session/cancel` for a running prompt, a `cancelled` answer to pending requests, then stdin
//! is closed (grok 1.0.44 exits on EOF), and after [`STOP_GRACE`] the process group is killed.
//! The process runs in `cwd` with `cfg.env`, in its own session and process group,
//! kill-on-drop, with the last 4 KB of stderr kept for `Exited`; the rest of its group is
//! killed whenever it exits.
//! Stdin is written by a dedicated task, so a runtime that stops reading cannot block `stop`.
//! The binary is `grok` on `PATH` unless [`BIN_ENV`] (or [`GrokBuildAdapter::with_bin`])
//! names another one.

pub mod acp;

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use twr_core::adapter::{Adapter, AdapterHandle, AdapterInfo, AttachApplies, SpawnConfig, Tier};

pub use crate::acp::driver::STDERR_TAIL;
pub use crate::acp::{acp_mcp_servers, mcp_servers_of};
pub use acp::{Emitted, Translated, Translator, TranslatorOptions};

use crate::acp::driver;
use crate::acp::translate::{native_mode, ModeLevel};
use crate::process;

/// Adapter name (`source = adapter:grok-build`).
pub const NAME: &str = "grok-build";

/// Environment variable overriding the `grok` binary (tests point it at a stub).
pub const BIN_ENV: &str = "TWR_GROK_BIN";

/// How long `stop` waits for the process to exit after closing stdin.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// The Grok Build runtime adapter.
#[derive(Debug, Clone)]
pub struct GrokBuildAdapter {
    bin: Option<PathBuf>,
    stop_grace: Duration,
    max_line: usize,
}

impl Default for GrokBuildAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl GrokBuildAdapter {
    /// Uses [`BIN_ENV`] if set, else `grok` from `PATH`.
    pub fn new() -> Self {
        GrokBuildAdapter {
            bin: None,
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
        }
    }

    /// Uses `bin` as the `grok` binary.
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        GrokBuildAdapter {
            bin: Some(bin.into()),
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
        }
    }

    /// Changes how long `stop` waits before killing the process group.
    pub fn stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// Changes the longest stdout line buffered ([`process::MAX_LINE`] by default).
    pub fn max_line(mut self, bytes: usize) -> Self {
        self.max_line = bytes;
        self
    }

    /// The binary this adapter runs ([`crate::resolve::select_program`]: this adapter's own,
    /// else [`BIN_ENV`], else `grok`; an empty one counts as unset).
    pub fn bin(&self) -> PathBuf {
        crate::resolve::select_program(NAME, self.bin.as_deref(), |k| std::env::var_os(k))
            .unwrap_or_else(|| PathBuf::from("grok"))
    }
}

/// What to do when the `grok` program is not there ([`process::start_error`]).
fn missing_fix() -> String {
    format!("install Grok Build, or set {BIN_ENV}")
}

/// The reasoning efforts grok takes (the `reasoning_effort` config option's values in grok
/// 1.0.44, verified 2026-10-03; any other is refused by grok and fails the session).
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

/// grok's own `--permission-mode` ids, passed as written (another spelling of one, such as
/// `dont-ask`, is passed as the id; another known mode maps by its level, see
/// [`permission_mode`]).
pub const NATIVE_MODES: &[&str] = &[
    "default",
    "plan",
    "dontAsk",
    "acceptEdits",
    "auto",
    "bypassPermissions",
];

/// The `--permission-mode` for a known mode of `level` that is not a [`NATIVE_MODES`] id:
/// restricted ones (`read-only`) are `plan`, ask-level ones (twr's `manual`) `default`.
fn level_mode(level: ModeLevel) -> &'static str {
    match level {
        ModeLevel::Restricted => "plan",
        ModeLevel::Ask => "default",
        ModeLevel::AcceptEdits => "acceptEdits",
        ModeLevel::Auto => "auto",
        ModeLevel::Bypass => "bypassPermissions",
    }
}

/// grok's `--permission-mode` for `cfg.mode`: `default` when unset, a [`NATIVE_MODES`] id, or a
/// known mode mapped by its level; an unknown mode is an error (the runtime does not start:
/// twr does not know what it would allow).
pub fn permission_mode(cfg: &SpawnConfig) -> anyhow::Result<&'static str> {
    match cfg.mode.trim() {
        "" => Ok("default"),
        m => {
            native_mode(m, NATIVE_MODES, level_mode).map_err(|e| anyhow::anyhow!("grok-build: {e}"))
        }
    }
}

/// [`permission_mode`] for building a command line; `spawn` and `attach_command` refuse an
/// unknown mode before, so the fallback (the narrowest, `plan`) is never used by them.
fn mode_of(cfg: &SpawnConfig) -> &'static str {
    permission_mode(cfg).unwrap_or("plan")
}

/// Every tool name grok's permission rules recognize except `MCPTool` (grok 1.0.44 docs,
/// "Tool Names": `Bash`, `Read`, `Edit` (and `Write`), `Grep` (and `Glob`), `MCPTool`,
/// `WebFetch`, `WebSearch`). With `builtin_tools: Some([])` each gets a `--deny` rule, which
/// grok 1.0.44 does not enforce in `agent stdio` (the agent profile is what restricts the
/// tools there, see [`agent_profile`]).
pub const BUILTIN_DENY: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Grep",
    "Glob",
    "WebFetch",
    "WebSearch",
];

/// grok's tools that reach MCP servers (`search_tool` finds a server's tool, `use_tool` calls
/// it): every agent profile twr sends keeps them.
pub const MCP_META_TOOLS: &[&str] = &["search_tool", "use_tool"];

/// Name of the agent profile twr sends with `builtin_tools`.
pub const PROFILE_NAME: &str = "twr-restricted";

/// The `--deny` rules for `cfg`: [`BUILTIN_DENY`] when it allows no built-in tool.
pub fn denied_builtins(cfg: &SpawnConfig) -> Vec<String> {
    if cfg.builtin_tools.as_ref().is_some_and(Vec::is_empty) {
        BUILTIN_DENY.iter().map(|t| t.to_string()).collect()
    } else {
        Vec::new()
    }
}

/// grok's tool names for twr's (Claude's) built-in tool names; a name twr does not know is
/// taken as a grok tool name.
pub fn grok_tool_names(name: &str) -> &[&str] {
    match name {
        "Bash" => &[
            "run_terminal_command",
            "get_command_or_subagent_output",
            "kill_command_or_subagent",
        ],
        "Read" => &["read_file", "list_dir"],
        "Edit" | "MultiEdit" => &["search_replace"],
        "Write" => &["write"],
        "Grep" => &["grep"],
        "Glob" | "LS" => &["list_dir"],
        "WebFetch" => &["web_fetch"],
        "WebSearch" => &["web_search"],
        "TodoWrite" => &["todo_write"],
        _ => &[],
    }
}

/// The tools of the agent profile for `builtin_tools`: [`MCP_META_TOOLS`] plus the grok names
/// of the listed tools, without duplicates, in order.
pub fn profile_tools(builtin_tools: &[String]) -> Vec<String> {
    let mut out: Vec<String> = MCP_META_TOOLS.iter().map(|t| t.to_string()).collect();
    for t in builtin_tools {
        let t = t.trim();
        if t.is_empty() {
            continue;
        }
        let names: Vec<String> = match grok_tool_names(t) {
            [] => vec![t.to_string()],
            names => names.iter().map(|n| n.to_string()).collect(),
        };
        for n in names {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

/// `_meta.agentProfile` for `cfg`: with `builtin_tools` set, an inline agent whose toolset is
/// [`profile_tools`] (grok 1.0.44 gives the model exactly those tools); else None (grok's
/// default agent and tools).
pub fn agent_profile(cfg: &SpawnConfig) -> Option<Value> {
    let tools = cfg.builtin_tools.as_ref()?;
    Some(json!({
        "name": PROFILE_NAME,
        "description": "twr: only the tools twr allows",
        "tools": profile_tools(tools),
    }))
}

/// The top-level options fixing what the session may do: `--no-auto-update`,
/// `--permission-mode`, `--allow` per `allowed_tools` rule and, with no built-in tools at
/// all (`Some([])`, e.g. the orchestrator), a `--deny` per [`BUILTIN_DENY`] name.
fn permission_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "--no-auto-update".into(),
        "--permission-mode".into(),
        mode_of(cfg).into(),
    ];
    for rule in &cfg.allowed_tools {
        a.extend(["--allow".into(), rule.clone()]);
    }
    for tool in denied_builtins(cfg) {
        a.extend(["--deny".into(), tool]);
    }
    a
}

/// Arguments for `grok ... agent --no-leader stdio`.
pub fn build_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut a = permission_args(cfg);
    a.extend(["agent".into(), "--no-leader".into(), "stdio".into()]);
    a
}

/// Environment added to the grok process: `cfg.env` and `GROK_DISABLE_AUTOUPDATER=1`.
pub fn build_env(cfg: &SpawnConfig) -> Vec<(String, String)> {
    let mut env = cfg.env.clone();
    env.push(("GROK_DISABLE_AUTOUPDATER".into(), "1".into()));
    env
}

impl Adapter for GrokBuildAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Headless,
            description: "Grok Build via `grok agent stdio` (ACP; verified with grok 1.0.44)",
        }
    }

    fn program(&self) -> Option<(PathBuf, String)> {
        Some((self.bin(), missing_fix()))
    }

    /// The prompt `/compact` (checked with grok 1.0.62, see `acp`).
    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let bin = self.bin();
        let mode = permission_mode(&cfg)?;
        let mcp_servers = mcp_servers_of(&cfg)?;
        let cwd = driver::absolute_cwd(&cfg.cwd)?;
        let fix = missing_fix();
        let child = driver::launch(
            &cfg.wrap,
            &bin,
            &build_args(&cfg),
            &build_env(&cfg),
            &cwd,
            &fix,
        )?;
        let translator = Translator::new(TranslatorOptions {
            thinking: cfg.thinking,
            pid: child.id(),
            exe: Some(bin.to_string_lossy().into_owned()),
            cwd: cwd.to_string_lossy().into_owned(),
            mcp_servers,
            resume_session: cfg.resume_session.clone(),
            // The `--permission-mode` grok was started with: the session mode is checked
            // against what grok was actually asked for.
            mode: mode.to_string(),
            model: cfg.model.clone().filter(|m| !m.trim().is_empty()),
            effort: cfg.effort.clone().filter(|e| !e.trim().is_empty()),
            rules: cfg.system_append.clone().filter(|s| !s.is_empty()),
            agent_profile: agent_profile(&cfg),
            denied_tools: denied_builtins(&cfg),
            usage: true,
        });
        tracing::info!(
            thread = %cfg.thread,
            pid = ?child.id(),
            resume = ?cfg.resume_session,
            "grok-build: started"
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let (handle, end) = AdapterHandle::pair();
        if let Some(pid) = child.id() {
            end.spawned(pid);
        }
        tokio::spawn(driver::drive(
            child,
            translator,
            end,
            first,
            self.stop_grace,
            self.max_line,
        ));
        Ok(handle)
    }

    /// `grok <permission options> --resume <session>` in the thread's directory, with the
    /// headless session's environment ([`build_env`]: `GROK_DISABLE_AUTOUPDATER=1`) and the
    /// same `--no-auto-update`, `--permission-mode`, `--allow` and `--deny` (so attaching
    /// never widens what the thread may do: a `config.toml` saying always-approve is
    /// overridden here too). A resumed session keeps the rules and agent profile (tool
    /// restriction) it was created with (verified over ACP; not checked in the TUI); MCP
    /// servers are not passed again. The thread's model and reasoning effort go along as
    /// `--model` / `--reasoning-effort`: the interactive CLI switches a resumed session to them
    /// (grok 1.0.46 showed "Switched to Grok 4.6 (high effort)" on a session made on 4.5 at
    /// low, and kept the new values), unlike `agent stdio`. With none, the session keeps its
    /// own, as a headless resume does.
    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        // An id that would read as an option of the command line: no command.
        if !crate::valid_session_id(session_id) {
            return None;
        }
        permission_mode(cfg).ok()?;
        let mut cmd = std::process::Command::new(self.bin());
        cmd.args(permission_args(cfg));
        if let Some(m) = cfg.model.as_deref().filter(|m| !m.trim().is_empty()) {
            cmd.args(["--model", m]);
        }
        if let Some(e) = cfg.effort.as_deref().filter(|e| !e.trim().is_empty()) {
            cmd.args(["--reasoning-effort", e]);
        }
        cmd.args(["--resume", session_id]).envs(build_env(cfg));
        process::scrub_std(&mut cmd);
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        Some(cmd)
    }

    /// Each setting the thread has (`--model`, `--reasoning-effort`: the interactive CLI
    /// switches a resumed session to them). One left to the runtime's default is not passed,
    /// and grok keeps the session's own on resume (`crate::resume_keeps`).
    fn attach_applies(&self, cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies {
            model: cfg.model.as_deref().is_some_and(|m| !m.trim().is_empty()),
            effort: cfg.effort.as_deref().is_some_and(|e| !e.trim().is_empty()),
        }
    }
}

//! Claude Code adapter. Tier: headless (`claude -p` stream-json on stdin and stdout).
//!
//! Command (verified against Claude Code 2.1.284; works with [`cli::MIN_VERSION`] and newer):
//!
//! ```text
//! claude -p --input-format stream-json --output-format stream-json --verbose
//!   --include-partial-messages --permission-mode <mode> [--permission-prompts host]
//!   --permission-prompt-tool stdio (--resume <id> | --session-id <new uuid v4>)
//!   [--model M] [--effort E] [--append-system-prompt S] [--mcp-config <json> --strict-mcp-config]
//!   [--allowedTools a,b] [--tools <built-in tools, "" = none>] [--settings <json>]
//! ```
//!
//! `<mode>` is a [`NATIVE_MODES`] id as written, another known mode mapped by its level
//! (restricted → `plan`), or `default` when unset; an unknown mode is refused (the runtime does
//! not start, and there is no attach command).
//!
//! `--permission-prompt-tool stdio` routes permission prompts to twr (without it, calls that
//! need approval are silently auto-denied). Claude Code 2.1.259 added `--permission-prompts`
//! (default `host`); it is passed only when the binary's `--help` lists it. What the binary
//! supports is read from its `--help` and `--version` before its first spawn and cached per
//! binary ([`cli`]): a version older than [`cli::MIN_VERSION`], or one lacking an option or the
//! permission mode the spawn needs, does not start ("`claude` 2.1.100 is too old for twr
//! (needs 2.1.110 or newer): run `claude update`"); `manual` is passed as `default` to versions
//! before 2.1.200, which call it that. While a binary is being probed, the spawn returns at
//! once and the process starts after the probe (a refusal then is the runtime's exit);
//! commands sent meanwhile are handed to the driver in order, and a `stop` (or the handle
//! dropped) cancels the start: the process never runs, and the exit is reported as a stop's
//! (code 0). A `--help` that could not be read is not cached ([`cli`]). A process that exits
//! refusing its command line gets the same update hint as the last line of its stderr tail,
//! after why `--help` was not read when it was not. The binary is `claude` on `PATH` unless
//! [`BIN_ENV`] (or [`ClaudeCodeAdapter::with_bin`]) names another one; the `PATH` is the
//! runtime's ([`crate::resolve::resolve`]: its thread's `env`, which a runner fills with the
//! login shell's), and the probe reads the binary found there.
//!
//! The process runs in `SpawnConfig::cwd` with `SpawnConfig::env` added and twr's secrets
//! removed ([`crate::process::SCRUBBED_ENV`], inherited or configured; `attach` too), and, with
//! `--settings` (the orchestrator's file guard), the variables that would keep Claude Code from
//! running its hook ([`guard::HOOK_ENV`], inherited or configured; `attach` too), in its
//! own session and process group (no controlling terminal), with stdin/stdout piped and the last 4 KB of stderr kept for `Exited`.
//! Stdout lines (at most [`crate::process::MAX_LINE`] bytes each: a longer one is skipped, and
//! a skipped `result` still ends its turn as an error with `ext.twr.error`) go through the
//! pure [`translate::Translator`]; see there for the mapping:
//!
//! | Claude stream-json                              | twr event                                   |
//! |-------------------------------------------------|---------------------------------------------|
//! | first `system/init` of the process              | `thread.started{session_id}` + `ext.twr.pid` |
//! | `stream_event` `text_delta`                     | `agent.text{delta:true}`                    |
//! | `assistant` text block                          | `agent.text{final:true}`                    |
//! | `assistant` thinking block (if enabled, non-empty) | `agent.thinking`                         |
//! | `assistant` tool_use block                      | `agent.tool_call`                           |
//! | `user` tool_result block                        | `agent.tool_result`                         |
//! | `control_request` can_use_tool                  | `attention.raised{permission}`              |
//! | `control_request` can_use_tool AskUserQuestion  | `attention.raised{question}` per question   |
//! | `control_request` elicitation (MCP form / URL)   | `attention.raised{question}`, options `decline` / `cancel` (answered only by the user) |
//! | `control_request` of any other subtype           | error reply, recorded as `attention.raised{error}` + `attention.resolved{by: timeout}` |
//! | `control_cancel_request` for a pending item      | `attention.resolved{by: timeout}`           |
//! | `control_request` reusing a pending `request_id` | `attention.resolved{by: timeout}` (`ext.claude.superseded`) + the new request raised under `<request_id>~<n>` |
//! | `result`                                        | `turn.ended` (+ `ext.claude`; failed: `ext.twr.error` with the cause, plus the `/login` hint and never transient when not signed in) |
//! | `rate_limit_event` with usage windows           | `runtime.limits{source: event}` (the hub folds it into `account.usage`) |
//! | `control_response` to twr's `get_usage`         | `runtime.limits{source: read}`              |
//!
//! twr writes a `get_usage` control request of its own (the plan's usage; no model turn)
//! after the first `system/init`, and after an `init` or `result` once the last usage report
//! is 10 minutes old (see [`translate`]).
//!
//! Commands: `send` writes a stream-json user message (Claude queues messages sent while a
//! turn runs and runs them as separate turns, so turn numbers are queued FIFO); `respond`
//! writes the `control_response` for a pending `can_use_tool` (unknown ids are ignored with a
//! warning; a multi-question `AskUserQuestion` is answered once all its questions are);
//! `stop` interrupts a running turn (`control_request{subtype: interrupt}`), closes stdin,
//! waits up to [`STOP_GRACE`] for the process to exit, then kills its process group.
//! Whenever the process exits (stopped, crashed or on its own), the rest of its process group
//! (for example commands the CLI moved to the background) is killed too.
//!
//! Stdin is written by a separate task fed through an unbounded queue, so the driver never
//! waits on a write: a runtime that stops reading its stdin (with a full pipe) cannot keep
//! `stop` from being handled or the kill deadline from running. At the deadline the writer is
//! dropped with whatever it still had queued.

pub mod cli;
pub mod guard;
pub mod translate;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterEvent, AdapterHandle, AdapterInfo, AttachApplies,
    SpawnConfig, Tier,
};

pub use translate::{Emitted, Translated, Translator, TranslatorOptions};

use crate::acp::translate::{native_mode, ModeLevel};
use crate::process::{self, BoundedLines, Line};

/// Adapter name (`source = adapter:claude-code`).
pub const NAME: &str = "claude-code";

/// Environment variable overriding the `claude` binary (tests point it at a stub).
pub const BIN_ENV: &str = "TWR_CLAUDE_BIN";

/// How long `stop` waits for the process to exit after closing stdin.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long stdout is still read after the process exited.
const DRAIN: Duration = Duration::from_secs(1);

/// Bytes of stderr kept for `Exited::stderr_tail`.
pub const STDERR_TAIL: usize = 4096;

/// The environment variable that turns off Claude Code's attachments: what it adds to a prompt
/// on its own, `@<path>` file mentions above all (read and inlined as a `Read` call's result
/// before the model sees the prompt, without a tool call, so no permission rule or
/// `PreToolUse` hook sees it; checked with 2.1.296 in `-p` stream-json mode, `--tools ""`
/// included: `@/etc/hostname` in a user message reached the model). Any non-empty value turns
/// them off (Claude Code tests the variable for truth as a string). With it set, the same
/// 2.1.296 still gave the model its environment, date and model reminders and its MCP tools;
/// it dropped the file attachment and the token-count reminder. The hub sets it for the
/// orchestrator's runtime and attach: hub notices quote agents' text, which could otherwise
/// have a user file inlined into the orchestrator's context.
pub const DISABLE_ATTACHMENTS_ENV: &str = "CLAUDE_CODE_DISABLE_ATTACHMENTS";

/// The Claude Code runtime adapter.
#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    bin: Option<PathBuf>,
    stop_grace: Duration,
    max_line: usize,
    probe_timeout: Duration,
    /// [`TranslatorOptions::missing_result_after`] and
    /// [`TranslatorOptions::notice_run_after`] (`None`: the defaults).
    waits: (Option<Duration>, Option<Duration>),
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClaudeCodeAdapter {
    /// Uses [`BIN_ENV`] if set, else `claude` from `PATH`.
    pub fn new() -> Self {
        ClaudeCodeAdapter {
            bin: None,
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
            probe_timeout: cli::PROBE_TIMEOUT,
            waits: (None, None),
        }
    }

    /// Uses `bin` as the `claude` binary.
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        ClaudeCodeAdapter {
            bin: Some(bin.into()),
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
            probe_timeout: cli::PROBE_TIMEOUT,
            waits: (None, None),
        }
    }

    /// Changes how long `stop` waits before killing the process group.
    pub fn stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// Changes how long after the main agent's message stopped a missing `result` ends the
    /// turn ([`translate::MISSING_RESULT_AFTER`] by default), and how long a turn opened for a
    /// background task's notification waits for its run ([`translate::NOTICE_RUN_AFTER`]).
    pub fn turn_waits(mut self, missing_result: Duration, notice_run: Duration) -> Self {
        self.waits = (Some(missing_result), Some(notice_run));
        self
    }

    /// Changes the longest stdout line buffered ([`process::MAX_LINE`] by default).
    pub fn max_line(mut self, bytes: usize) -> Self {
        self.max_line = bytes;
        self
    }

    /// Changes how long reading the binary's `--help` / `--version` may take
    /// ([`cli::PROBE_TIMEOUT`] by default).
    pub fn probe_timeout(mut self, limit: Duration) -> Self {
        self.probe_timeout = limit;
        self
    }

    /// The binary this adapter runs ([`crate::resolve::select_program`]: this adapter's own,
    /// else [`BIN_ENV`], else `claude`; an empty one counts as unset).
    pub fn bin(&self) -> PathBuf {
        crate::resolve::select_program(NAME, self.bin.as_deref(), |k| std::env::var_os(k))
            .unwrap_or_else(|| PathBuf::from("claude"))
    }
}

/// What to do when the `claude` program is not there ([`process::start_error`]).
fn missing_fix() -> String {
    format!("install Claude Code, or set {BIN_ENV}")
}

/// Claude Code's `--effort` levels (2.1.288 `--help`: "Effort level for the current session
/// (low, medium, high, xhigh, max)"). Checked 2026-10-03: 2.1.288 runs with any other value,
/// warning "Unknown --effort value ... using the default effort", so the hub refuses one up
/// front.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// Why the `claude` a thread with environment `env` and working directory `cwd` would run (the
/// adapter's binary: [`BIN_ENV`], else `claude` on the `PATH` `env` gives, resolved in `cwd`, as
/// a spawn resolves it) cannot take a reasoning effort, when an earlier spawn probed it and its
/// `--help` does not list `--effort` ([`cli::effort_refusal`]). Never probes: `None` when
/// nothing is known yet (a spawn then refuses the effort itself, with the same hint).
pub fn effort_refusal(env: &[(String, String)], cwd: &std::path::Path) -> Option<String> {
    let bin = ClaudeCodeAdapter::new().bin();
    let resolved = crate::resolve::resolve(&bin, env, None, cwd);
    let support = cli::cached(&resolved, cwd)?;
    cli::effort_refusal(&bin, &support)
}

/// Claude Code's own `--permission-mode` ids, passed as written (another spelling of one,
/// such as `dont-ask`, is passed as the id; another known mode maps by its level, see
/// [`permission_mode`]).
pub const NATIVE_MODES: &[&str] = &[
    "default",
    "manual",
    "plan",
    "dontAsk",
    "acceptEdits",
    "auto",
    "bypassPermissions",
];

/// The `--permission-mode` for a known mode of `level` that is not a [`NATIVE_MODES`] id:
/// restricted ones (`read-only`) are `plan`.
fn level_mode(level: ModeLevel) -> &'static str {
    match level {
        ModeLevel::Restricted => "plan",
        ModeLevel::Ask => "default",
        ModeLevel::AcceptEdits => "acceptEdits",
        ModeLevel::Auto => "auto",
        ModeLevel::Bypass => "bypassPermissions",
    }
}

/// The `--permission-mode` for `cfg.mode`: `default` when unset, a [`NATIVE_MODES`] id, or a
/// known mode mapped by its level; an unknown mode is an error (the runtime does not start).
pub fn permission_mode(cfg: &SpawnConfig) -> anyhow::Result<&'static str> {
    match cfg.mode.trim() {
        "" => Ok("default"),
        m => native_mode(m, NATIVE_MODES, level_mode)
            .map_err(|e| anyhow::anyhow!("claude-code: {e}")),
    }
}

/// [`permission_mode`] for building a command line; `spawn` and `attach_command` refuse an
/// unknown mode before, so the fallback (the narrowest, `plan`) is never used by them.
fn mode_of(cfg: &SpawnConfig) -> &'static str {
    permission_mode(cfg).unwrap_or("plan")
}

/// Arguments for a headless session on a `claude` that takes every option (the newest
/// versions). `session_id` is the session to resume when `cfg.resume_session` is set, else the
/// id of the new session (`--session-id`).
pub fn build_args(cfg: &SpawnConfig, session_id: &str) -> Vec<String> {
    let plan = cli::Plan {
        system_prompt_snapshot: true,
        permission_prompts: true,
        mode: mode_of(cfg),
    };
    build_args_with(cfg, session_id, plan)
}

/// [`build_args`] as `plan` says ([`cli::plan`]): its permission mode, and
/// `--permission-prompts host` only if the binary takes it.
pub fn build_args_with(cfg: &SpawnConfig, session_id: &str, plan: cli::Plan) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-mode",
        plan.mode,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if plan.permission_prompts {
        a.extend([cli::PERMISSION_PROMPTS.into(), "host".into()]);
    }
    a.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    if cfg.resume_session.is_some() {
        a.extend(["--resume".into(), session_id.into()]);
    } else {
        a.extend(["--session-id".into(), session_id.into()]);
    }
    if let Some(m) = cfg.model.as_deref().filter(|m| !m.is_empty()) {
        a.extend(["--model".into(), m.into()]);
    }
    if let Some(e) = cfg.effort.as_deref().filter(|e| !e.is_empty()) {
        a.extend([cli::EFFORT.into(), e.into()]);
    }
    if let Some(s) = cfg.system_append.as_deref().filter(|s| !s.is_empty()) {
        a.extend(["--append-system-prompt".into(), s.into()]);
    }
    if cfg.refresh_system_prompt && plan.system_prompt_snapshot {
        a.extend([cli::SYSTEM_PROMPT_SNAPSHOT.into(), "off".into()]);
    }
    a.extend(tool_args(cfg));
    a
}

/// The MCP and tool restriction flags of `cfg`, shared by the headless session and the
/// interactive attach (so attaching never widens which tools the thread has).
fn tool_args(cfg: &SpawnConfig) -> Vec<String> {
    let mut a = Vec::new();
    // `--mcp-config` takes JSON or a file path; a file keeps secrets off the command line.
    let mcp = match (&cfg.mcp_config_file, &cfg.mcp_config) {
        (Some(file), _) => Some(file.to_string_lossy().into_owned()),
        (None, Some(v)) if !v.is_null() => Some(v.to_string()),
        _ => None,
    };
    if let Some(mcp) = mcp {
        a.extend(["--mcp-config".into(), mcp, "--strict-mcp-config".into()]);
    }
    if !cfg.allowed_tools.is_empty() {
        a.extend(["--allowedTools".into(), cfg.allowed_tools.join(",")]);
    }
    if let Some(tools) = &cfg.builtin_tools {
        // `--tools ""` disables every built-in tool; MCP tools are not affected.
        a.extend(["--tools".into(), tools.join(",")]);
    }
    // Layered over the user's settings (the orchestrator's file guard hook); no secret in it.
    if let Some(settings) = cfg.settings.as_ref().filter(|s| !s.is_null()) {
        a.extend(["--settings".into(), settings.to_string()]);
    }
    a
}

/// The environment variables to remove from a runtime (or attach command) of `cfg`: with
/// `--settings`, those that would keep Claude Code from running its hooks ([`guard::HOOK_ENV`]),
/// else none.
fn guard_env(cfg: &SpawnConfig) -> &'static [&'static str] {
    match cfg.settings.as_ref().filter(|s| !s.is_null()) {
        Some(_) => &guard::HOOK_ENV,
        None => &[],
    }
}

impl Adapter for ClaudeCodeAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Headless,
            description: "Claude Code via `claude -p` stream-json (permissions over stdio)",
        }
    }

    fn program(&self) -> Option<(PathBuf, String)> {
        Some((self.bin(), missing_fix()))
    }

    /// `/compact` as the turn's message ([`Translator::compact_message`]).
    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let bin = self.bin();
        let mode = permission_mode(&cfg)?;
        let start = Start {
            first: prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1))),
            bin,
            mode,
            cfg,
            grace: self.stop_grace,
            max_line: self.max_line,
            waits: self.waits,
        };
        let (handle, end) = AdapterHandle::pair();
        // The binary the process will run, on the `PATH` it gets: what the probe reads.
        let resolved = crate::resolve::resolve(&start.bin, &start.cfg.env, None, &start.cfg.cwd);
        match cli::lookup(&resolved, &start.cfg.cwd, self.probe_timeout) {
            cli::Lookup::Known(help) => {
                let started = start.launch(&help)?;
                started.report_pid(&end);
                tokio::spawn(drive(started, end, VecDeque::new()));
            }
            // The binary is being probed: read what it supports without holding up the
            // caller, then start it (or report why it cannot be used as the runtime's exit).
            cli::Lookup::Probe(probe) => {
                tokio::spawn(start.after_probe(probe, end));
            }
        }
        Ok(handle)
    }

    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        // Only what is already known of this box's binary (no probe here).
        let known = cli::cached(
            &crate::resolve::resolve(&self.bin(), &cfg.env, None, &cfg.cwd),
            &cfg.cwd,
        );
        self.interactive(cfg, session_id, known.as_ref())
    }

    /// Nothing this box learned of its own `claude` applies to another box's: every setting is
    /// passed as the thread has it, and a version there that does not take one (`--effort`, a
    /// mode under its newer name) refuses the command line in the user's terminal.
    fn remote_attach_command(
        &self,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> Option<std::process::Command> {
        self.interactive(cfg, session_id, None)
    }

    /// The model when the thread has one (`--model`; without it the session keeps its own,
    /// `crate::resume_keeps`), and always the effort: `--effort`, or none for the runtime's
    /// default, which a resumed session takes (Claude Code does not keep a session's effort). A
    /// binary known not to take `--effort` gets none, but it cannot run a thread with an
    /// effort headless either (its start is refused with "run `claude update`").
    fn attach_applies(&self, cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies {
            model: cfg.model.as_deref().is_some_and(|m| !m.is_empty()),
            effort: true,
        }
    }
}

impl ClaudeCodeAdapter {
    /// The interactive `claude --resume` of `twr attach`, fitted to `known`, what is known of
    /// the binary that will run it (`None`: nothing, every setting is passed). None for a
    /// session id twr does not put on a command line ([`crate::valid_session_id`]: `--resume`
    /// takes an optional value, so `--dangerously-skip-permissions` there would be a flag).
    fn interactive(
        &self,
        cfg: &SpawnConfig,
        session_id: &str,
        known: Option<&cli::Support>,
    ) -> Option<std::process::Command> {
        if !crate::valid_session_id(session_id) {
            return None;
        }
        let mode = permission_mode(cfg).ok()?;
        let bin = self.bin();
        // `manual` is `default` to a version that calls it that; anything it does not take, it
        // refuses itself.
        let mode = match known {
            Some(s) => cli::mode_for(s, mode).unwrap_or(mode),
            None => mode,
        };
        // The effort only where the binary may take it (a version known not to list
        // `--effort` would refuse the whole command line; it then runs on its default effort:
        // Claude Code does not keep a session's effort on resume, `crate::resume_keeps`).
        let effort = cfg
            .effort
            .as_deref()
            .filter(|e| !e.is_empty())
            .filter(|_| known.is_none_or(|s| cli::effort_refusal(&bin, s).is_none()));
        let mut cmd = std::process::Command::new(bin);
        cmd.args(["--resume", session_id, "--permission-mode", mode]);
        if let Some(m) = cfg.model.as_deref().filter(|m| !m.is_empty()) {
            cmd.args(["--model", m]);
        }
        if let Some(e) = effort {
            cmd.args([cli::EFFORT, e]);
        }
        cmd.args(tool_args(cfg)).envs(process::attach_env(cfg));
        process::scrub_std(&mut cmd);
        for k in guard_env(cfg) {
            cmd.env_remove(k);
        }
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        Some(cmd)
    }
}

/// One spawn, before its process starts.
struct Start {
    bin: PathBuf,
    cfg: SpawnConfig,
    mode: &'static str,
    first: Option<(String, u64)>,
    grace: Duration,
    max_line: usize,
    waits: (Option<Duration>, Option<Duration>),
}

/// A started process with what its driver needs.
struct Started {
    child: Child,
    translator: Translator,
    first: Option<(String, u64)>,
    grace: Duration,
    max_line: usize,
    program: PathBuf,
    /// Why the binary's `--help` was not read, if it was not (for a refused command line).
    unread: Option<String>,
}

impl Started {
    /// Tells the hub which process was started ([`AdapterEnd::spawned`]).
    fn report_pid(&self, end: &AdapterEnd) {
        if let Some(pid) = self.child.id() {
            end.spawned(pid);
        }
    }
}

impl Start {
    /// Waits for the probe of the binary, then starts the process and drives it, or reports
    /// why it cannot be used as the runtime's exit. Commands sent meanwhile are kept, in order,
    /// for the driver; a `stop` (or the hub dropping the handle) ends the wait: the process
    /// never starts, the probe is killed unless another spawn waits for it, and the exit is
    /// reported as a stop's.
    async fn after_probe(self, probe: cli::Probe, mut end: AdapterEnd) {
        let mut early = VecDeque::new();
        let wait = probe.wait();
        tokio::pin!(wait);
        let help = loop {
            tokio::select! {
                // A stop sent at the moment the probe ends still wins.
                biased;
                cmd = end.cmds.recv() => match cmd {
                    Some(AdapterCmd::Stop) | None => {
                        tracing::info!(thread = %self.cfg.thread, "claude-code: stopped before it started");
                        end.exited(Some(0), "");
                        return;
                    }
                    Some(cmd) => early.push_back(cmd),
                },
                help = &mut wait => break help,
            }
        };
        match self.launch(&help) {
            Ok(started) => {
                started.report_pid(&end);
                drive(started, end, early).await
            }
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!(thread = %self.cfg.thread, %why, "claude-code: cannot start");
                end.exited(Some(1), format!("{why}\n"));
            }
        }
    }

    /// Starts the process as what the binary supports allows ([`cli::plan`]; `--help` not
    /// read: every option is passed). Errors say what to do, program first.
    fn launch(&self, help: &cli::Help) -> anyhow::Result<Started> {
        let cfg = &self.cfg;
        let bin = &self.bin;
        let resumed = cfg.resume_session.is_some();
        let session_id = cfg
            .resume_session
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if !crate::valid_session_id(&session_id) {
            anyhow::bail!(
                "claude-code: session id {session_id:?} is not one twr passes to `--resume`"
            );
        }
        let full = build_args(cfg, &session_id);
        let plan = cli::plan(bin, help.support(), self.mode, &full).map_err(anyhow::Error::msg)?;
        let mut cmd = process::wrapped(&cfg.wrap, bin);
        cmd.args(build_args_with(cfg, &session_id, plan))
            .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        process::new_session(&mut cmd);
        process::scrub(&mut cmd);
        for k in guard_env(cfg) {
            cmd.env_remove(k);
        }
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        process::check_wrapped(&cfg.wrap, bin, &cfg.env, &cfg.cwd, &missing_fix())?;
        let child = cmd
            .spawn()
            .map_err(|e| process::start_error(e, bin, &cfg.env, &cfg.cwd, &missing_fix()))?;
        let mut translator = Translator::new(TranslatorOptions {
            thinking: cfg.thinking,
            pid: child.id(),
            exe: Some(bin.to_string_lossy().into_owned()),
            resumed,
            mode: plan.mode.to_string(),
            missing_result_after: self.waits.0,
            notice_run_after: self.waits.1,
        });
        if cfg.refresh_system_prompt {
            let mut metadata = serde_json::Map::new();
            metadata.insert(
                "system_prompt_snapshot_off".into(),
                serde_json::json!(plan.system_prompt_snapshot),
            );
            if let Some(version) = help.support().and_then(|s| s.version) {
                metadata.insert("cli_version".into(), serde_json::json!(version.to_string()));
            }
            if let Some((_, fingerprint)) = cfg
                .env
                .iter()
                .find(|(k, _)| k == "TWR_ORCH_PROMPT_FINGERPRINT")
            {
                metadata.insert("prompt_fingerprint".into(), serde_json::json!(fingerprint));
            }
            translator.set_harness_metadata(metadata);
        }
        tracing::info!(
            thread = %cfg.thread,
            pid = ?child.id(),
            session = %session_id,
            resumed,
            mode = plan.mode,
            permission_prompts = plan.permission_prompts,
            "claude-code: started"
        );
        Ok(Started {
            child,
            translator,
            first: self.first.clone(),
            grace: self.grace,
            max_line: self.max_line,
            program: bin.clone(),
            unread: help.unread().map(str::to_string),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// IO driver
// ---------------------------------------------------------------------------------------------

/// Last [`STDERR_TAIL`] bytes of a stream.
#[derive(Debug, Default)]
struct Tail(VecDeque<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
        let excess = self.0.len().saturating_sub(STDERR_TAIL);
        self.0.drain(..excess);
    }

    fn text(&self) -> String {
        let (a, b) = self.0.as_slices();
        let mut v = Vec::with_capacity(self.0.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }
}

async fn collect_tail(mut r: impl AsyncRead + Unpin, tail: Arc<Mutex<Tail>>) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => tail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&buf[..n]),
        }
    }
}

/// Queue of lines for the stdin writer task; None once stdin is closed (or the writer gave up).
type Stdin = Option<mpsc::UnboundedSender<String>>;

/// Writes queued lines to the runtime's stdin until the queue closes (then drops stdin: EOF)
/// or a write fails (the process is exiting and its exit is reported separately).
async fn write_stdin(mut w: ChildStdin, mut lines: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = lines.recv().await {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let res = async {
            w.write_all(&bytes).await?;
            w.flush().await
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "claude-code: writing to stdin failed");
            return;
        }
    }
}

/// Queues one line for the runtime's stdin. Never waits.
fn queue_line(stdin: &mut Stdin, line: String) {
    let Some(tx) = stdin.as_ref() else {
        tracing::warn!("claude-code: stdin already closed; dropping a line");
        return;
    };
    if tx.send(line).is_err() {
        tracing::warn!("claude-code: stdin writer is gone; dropping a line");
        *stdin = None;
    }
}

/// Sends `SIGKILL` to process group `pgid` with the `kill` command (no libc in this crate).
fn kill_group(pgid: u32) {
    let mut cmd = std::process::Command::new("kill");
    cmd.args(["-KILL", "--", &format!("-{pgid}")])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    // No terminal: whatever `kill` on `PATH` is.
    process::new_session_std(&mut cmd);
    let status = cmd.status();
    tracing::debug!(pgid, ?status, "claude-code: killed process group");
}

/// Drives a started process; `early` are commands received before it started, handled first
/// (none are written if a `stop` is among them, or already queued: the stop is).
async fn drive(started: Started, mut end: AdapterEnd, early: VecDeque<AdapterCmd>) {
    let Started {
        mut child,
        translator: mut tr,
        first,
        grace,
        max_line,
        program,
        unread,
    } = started;
    let pid = child.id();
    let (mut stdin, writer): (Stdin, _) = match child.stdin.take() {
        Some(w) => {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(tokio::spawn(write_stdin(w, rx))))
        }
        None => (None, None),
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let tail = Arc::new(Mutex::new(Tail::default()));
    let stderr_task = child
        .stderr
        .take()
        .map(|e| tokio::spawn(collect_tail(e, Arc::clone(&tail))));
    let mut lines = BoundedLines::new(stdout, max_line);
    let tasks = Tasks {
        writer,
        stderr: stderr_task,
    };

    // Commands sent before the process started, and any already queued: with a stop (or the
    // handle dropped) among them, nothing is written, the first prompt included.
    let mut pending = early;
    let mut dropped = false;
    loop {
        match end.cmds.try_recv() {
            Ok(cmd) => pending.push_back(cmd),
            Err(mpsc::error::TryRecvError::Empty) => break,
            Err(mpsc::error::TryRecvError::Disconnected) => {
                dropped = true;
                break;
            }
        }
    }
    let mut stop_deadline: Option<tokio::time::Instant> = None;
    if dropped || pending.contains(&AdapterCmd::Stop) {
        tracing::info!(?pid, "claude-code: stopped before the first prompt");
        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
    } else {
        if let Some((prompt, turn)) = first {
            let line = tr.user_message(&prompt, turn);
            queue_line(&mut stdin, line);
        }
        for cmd in pending {
            if stop_deadline.is_none() && on_cmd(Some(cmd), &mut tr, &mut stdin, &end, &tail) {
                stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
            }
        }
    }

    let mut stdout_open = true;
    // Whether the process wrote anything to stdout (one refusing its command line does not).
    let mut heard = false;
    loop {
        // What the translator awaits by a deadline (a missing `result`, a notification's run).
        let awaited = tr.deadline().map(|at| {
            tokio::time::Instant::now() + (at - chrono::Utc::now()).to_std().unwrap_or_default()
        });
        tokio::select! {
            _ = sleep_until_opt(awaited), if awaited.is_some() && stop_deadline.is_none() => {
                let out = tr.expire(chrono::Utc::now());
                if emit(&mut stdin, &end, &tail, out) {
                    stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                }
            }
            line = lines.next_line(), if stdout_open => match line {
                Ok(Some(l)) => {
                    heard = true;
                    let stop = on_line(&mut tr, &mut stdin, &end, &tail, l);
                    if stop && stop_deadline.is_none() {
                        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                    }
                }
                Ok(None) => stdout_open = false,
                Err(e) => {
                    tracing::warn!(error = %e, "claude-code: reading stdout failed");
                    stdout_open = false;
                }
            },
            status = child.wait() => {
                // Leftovers in the group (e.g. commands the CLI moved to the background) die
                // with it, whatever the exit path. Lines already in the pipe stay readable.
                if let Some(p) = pid {
                    kill_group(p);
                }
                if stdout_open {
                    // Lines still buffered in the pipe; a leftover process holding stdout
                    // open must not keep the exit from being reported.
                    let drain_until = tokio::time::Instant::now() + DRAIN;
                    while let Ok(Ok(Some(l))) =
                        tokio::time::timeout_at(drain_until, lines.next_line()).await
                    {
                        heard = true;
                        on_line(&mut tr, &mut stdin, &end, &tail, l);
                    }
                }
                let refused = (!heard).then_some((program.as_path(), unread.as_deref()));
                return finish(status, pid, &end, &tail, tasks, refused).await;
            }
            cmd = end.cmds.recv(), if stop_deadline.is_none() => {
                if on_cmd(cmd, &mut tr, &mut stdin, &end, &tail) {
                    stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                }
            }
            _ = sleep_until_opt(stop_deadline) => {
                tracing::warn!(?pid, "claude-code: runtime did not exit after stop; killing its process group");
                if let Some(p) = pid {
                    kill_group(p);
                }
                let status = child.wait().await;
                return finish(status, pid, &end, &tail, tasks, None).await;
            }
        }
    }
}

/// Handles one command from the hub (`None`: the handle was dropped). Returns true if the
/// session must stop: asked to, or the hub no longer listens.
fn on_cmd(
    cmd: Option<AdapterCmd>,
    tr: &mut Translator,
    stdin: &mut Stdin,
    end: &AdapterEnd,
    tail: &Arc<Mutex<Tail>>,
) -> bool {
    match cmd {
        Some(AdapterCmd::Send { text, turn }) => {
            let line = tr.user_message(&text, turn);
            queue_line(stdin, line);
            false
        }
        Some(AdapterCmd::Respond { attn_id, answer }) => match tr.respond(&attn_id, &answer) {
            Some(out) => emit(stdin, end, tail, out),
            None => {
                tracing::warn!(%attn_id, "claude-code: ignoring respond for an attention that is not pending");
                false
            }
        },
        Some(AdapterCmd::Interrupt) => {
            if let Some(line) = tr.interrupt() {
                queue_line(stdin, line);
            }
            false
        }
        Some(AdapterCmd::Compact { turn }) => {
            let line = tr.compact_message(turn);
            queue_line(stdin, line);
            false
        }
        Some(AdapterCmd::Stop) | None => true,
    }
}

/// Helper tasks of one runtime process.
struct Tasks {
    writer: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

/// Translates one stdout line, queues any reply and emits the events. Returns true if the
/// session must stop (the hub no longer listens, or it cannot go on). An oversized line is
/// dropped (warned about); a `result` one still ends the turn ([`Translator::oversized`]).
fn on_line(
    tr: &mut Translator,
    stdin: &mut Stdin,
    end: &AdapterEnd,
    tail: &Arc<Mutex<Tail>>,
    line: Line,
) -> bool {
    let out = match line {
        Line::Text(l) => tr.translate(&l),
        Line::Oversized { len, head } => {
            tracing::debug!(len, head = %translate::one_line(&head, 200), "claude-code: dropping an oversized stdout line");
            tr.oversized(&head, len)
        }
    };
    emit(stdin, end, tail, out)
}

/// Queues `out.reply` and `out.requests` and emits `out.events`. A fatal translation (the session runs in a mode
/// twr does not accept) is recorded in the stderr tail (so the exit says why). Returns true if
/// the session must stop: the hub no longer listens, or the translation was fatal.
fn emit(stdin: &mut Stdin, end: &AdapterEnd, tail: &Arc<Mutex<Tail>>, out: Translated) -> bool {
    if let Some(reply) = out.reply {
        queue_line(stdin, reply);
    }
    for request in out.requests {
        queue_line(stdin, request);
    }
    let mut stop = false;
    for e in out.events {
        let ev = AdapterEvent::Emit {
            body: e.body,
            raw: e.raw,
            turn: e.turn,
        };
        if end.events.send(ev).is_err() && !stop {
            tracing::warn!("claude-code: hub dropped the event stream; stopping");
            stop = true;
        }
    }
    if let Some(why) = out.fatal {
        tracing::warn!(%why, "claude-code: session cannot go on; stopping");
        tail.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("twr: {why}\n").as_bytes());
        stop = true;
    }
    stop
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Queues an interrupt for a running turn and closes the queue: the writer sends what it has,
/// then closes stdin (Claude exits on EOF). Returns the kill deadline, which starts now
/// whether or not the runtime is still reading its stdin.
fn begin_stop(tr: &mut Translator, stdin: &mut Stdin, grace: Duration) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + grace;
    if let Some(line) = tr.interrupt() {
        queue_line(stdin, line);
    }
    *stdin = None;
    deadline
}

/// How long the stderr collector may keep reading after the process exited.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// Reports the exit. The process group was already killed by the caller. `silent` is the
/// program of a process that wrote nothing to stdout, with why its `--help` was not read (if
/// it was not): if its stderr shows it refused its command line, the update hint
/// ([`cli::refused_options_hint`]) is added as the last line.
async fn finish(
    status: std::io::Result<std::process::ExitStatus>,
    pid: Option<u32>,
    end: &AdapterEnd,
    tail: &Arc<Mutex<Tail>>,
    tasks: Tasks,
    silent: Option<(&std::path::Path, Option<&str>)>,
) {
    // Whatever the writer still had queued (or was blocked writing) is dropped with stdin.
    if let Some(w) = tasks.writer {
        w.abort();
    }
    if let Some(mut t) = tasks.stderr {
        // A process that left the group (setsid) can hold stderr open: stop reading instead
        // of leaking the task and the pipe.
        if tokio::time::timeout(STDERR_DRAIN, &mut t).await.is_err() {
            t.abort();
        }
    }
    let code = match status {
        Ok(s) => s.code(),
        Err(e) => {
            tracing::warn!(error = %e, "claude-code: waiting for the runtime failed");
            None
        }
    };
    let mut text = tail.lock().unwrap_or_else(|e| e.into_inner()).text();
    if let Some(hint) = silent.and_then(|(p, unread)| cli::refused_options_hint(&text, p, unread)) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&hint);
        text.push('\n');
    }
    tracing::info!(?pid, ?code, "claude-code: exited");
    end.exited(code, text);
}

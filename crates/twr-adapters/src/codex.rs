//! Codex adapter. Tier: headless (`codex app-server --stdio`, JSON-RPC over stdin/stdout).
//!
//! `codex exec --json` has no approval channel, so twr drives the app server.
//! Verified against Codex 0.155.1. The binary is `codex` on `PATH` unless [`BIN_ENV`] (or
//! [`CodexAdapter::with_bin`]) names another one.
//!
//! Session: `initialize` → `initialized` → `thread/start` (or `thread/resume{threadId}`), then
//! one `turn/start` per prompt. The session id is the Codex thread id. See [`translate`] for
//! the wire facts and the event mapping:
//!
//! | Codex app-server                                   | twr event                                    |
//! |----------------------------------------------------|----------------------------------------------|
//! | `thread/start` / `thread/resume` response          | `thread.started{session_id: thread.id}` + `ext.twr.pid` |
//! | `item/agentMessage/delta`                          | `agent.text{delta:true}`                     |
//! | `item/completed` agentMessage                      | `agent.text{final:true}` (`ext.codex.phase`) |
//! | `item/completed` reasoning (if `thinking`)         | `agent.thinking`                             |
//! | `item/started` commandExecution / fileChange / mcpToolCall | `agent.tool_call` (`shell` / `apply_patch` / `mcp__<server>__<tool>`) |
//! | `item/completed` of those                          | `agent.tool_result`                          |
//! | `item/commandExecution/requestApproval`            | `attention.raised{permission}` "Allow shell: <cmd>" (marked so every character shows; when cut, the whole command in `ext.codex.full_input`) |
//! | `item/fileChange/requestApproval`                  | `attention.raised{permission}` "Allow edit N files: a, b", prefixed with what a `grantRoot` grants (writes anywhere under it for the session) |
//! | `mcpServer/elicitation/request` (MCP tool approval) | `attention.raised{permission}`              |
//! | `mcpServer/elicitation/request` (form / URL)       | `attention.raised{question}`, options `decline` / `cancel` (answered only by the user) |
//! | any other server request                           | `-32601` error reply, recorded as `attention.raised{error}` + `attention.resolved{by: timeout}` |
//! | `item/permissions/requestApproval`                 | `attention.raised{permission}` "Allow network access, write a for this turn? <reason>" (every path listed; anything twr does not recognize, or a summary over 1000 chars, makes it deny-only) |
//! | `item/tool/requestUserInput`, `agentMessage.questions` | `attention.raised{question}` per question |
//! | `serverRequest/resolved` for an unanswered request | `attention.resolved{by: timeout}`            |
//! | `turn/completed{status}`                           | `turn.ended{completed / interrupted / error}` (failed: `ext.twr.error` with the cause; not signed in: plus the `codex login` hint, never transient) |
//!
//! Answers: `allow` → `{decision: "accept"}`, anything else → `{decision: "decline"}` (MCP
//! tool approvals: `{action: "accept" | "decline"}`; permission requests: the recognized part
//! of the requested profile with `scope: "turn"`, or an empty profile); question answers go back as
//! `{answers}` or, for async questions, as `turn/steer`. Tokens per turn are the growth of the
//! thread's `tokenUsage.total`; Codex reports no USD cost (`usage.cost` is null). Duration is
//! `turn.durationMs`.
//!
//! ## `SpawnConfig` mapping
//!
//! | `SpawnConfig`          | Codex                                                                |
//! |------------------------|----------------------------------------------------------------------|
//! | `cwd`                  | process cwd + `cwd` param                                            |
//! | `mode`                 | `approvalPolicy` + `sandbox`, see "Permission modes" below; always `approvalsReviewer: "user"` |
//! | `model`                | `model` param                                                        |
//! | `effort`               | `config.model_reasoning_effort` (one of [`EFFORTS`])                 |
//! | `system_append`        | `developerInstructions` (added to the developer message; `baseInstructions` would replace Codex's own prompt) |
//! | `mcp_config(_file)`    | `config.mcp_servers`, as an allowlist like Claude's `--strict-mcp-config`: Codex merges that table into the user's and the project's servers, so the translator reads the effective config first and turns every other server off by name, with plugins and apps off too ([`STRICT_MCP_FEATURES_OFF`]; [`translate`], "MCP allowlist"); sent over stdin, so secrets in it (the orchestrator token) never reach a command line |
//! | `allowed_tools`        | `mcp__<server>` → that server's `default_tools_approval_mode = "approve"`; other rules have no Codex equivalent and are ignored (logged) |
//! | `builtin_tools`        | `None` = Codex's defaults; `Some(list)` = only the named built-in groups stay on, see [`builtin_overrides`] |
//! | `thinking`             | `agent.thinking` from reasoning items + `summary: "auto"` on `turn/start` |
//! | `env`                  | process environment                                                  |
//! | `repo`                 | where the `auto` sandbox's git roots come from, see "The `auto` sandbox" below |
//! | `resume_session`       | `thread/resume{threadId}` with every setting passed again (Codex does not keep the permission settings across processes; a model or effort left out keeps the thread's, see [`crate::resume_keeps`]) |
//!
//! ## Permission modes
//!
//! A twr mode is the runtime's own permission mode (twr never answers a prompt for the user),
//! so a mapping may be narrower than what the user set (Codex asks more, or its sandbox allows
//! less) but never wider. Codex has no equivalent for several Claude-style modes; those map to
//! the nearest narrower policy, and `thread.started` carries `ext.codex.mode_note` saying so
//! ([`mode_note`]):
//!
//! | mode                                              | approvalPolicy | sandbox            | exact? |
//! |---------------------------------------------------|----------------|--------------------|--------|
//! | `manual` (twr default), `default`, `untrusted`, `""` | `untrusted`  | `workspace-write`  | yes |
//! | `plan`                                            | `untrusted`    | `read-only`        | no: Codex has no plan mode |
//! | `read-only`                                       | `on-request`   | `read-only`        | yes (Codex names) |
//! | `on-request`                                      | `on-request`   | `workspace-write`  | yes (Codex names) |
//! | `auto`                                            | `on-request`   | `workspace-write` + network, git metadata, caches (below) | no: sandboxed commands run without asking, anything beyond the sandbox asks |
//! | `acceptEdits`                                     | `untrusted`    | `workspace-write`  | no: Codex cannot accept edits alone; edits and commands still ask |
//! | `dontAsk`                                         | `never`        | `read-only`        | no: deny by default (like Claude's `dontAsk`); nothing asks, writes fail; MCP servers approved via `allowed_tools` still run |
//! | `never`                                           | `never`        | `workspace-write`  | yes (Codex names) |
//! | `bypassPermissions`, `danger-full-access`, `yolo` | `never`        | `danger-full-access` | yes |
//! | another spelling of one of these (`dont-ask`)    | as that mode   |                    | |
//! | another mode twr knows (`readOnly`, `ask`, `autoEdit`, `always-approve`, ...) | as its level: restricted → `plan`, ask → `manual`, accept-edits → `acceptEdits`, auto → `auto`, bypass → `bypassPermissions` | | |
//! | anything else                                     | refused: the runtime does not start | | |
//!
//! `untrusted` asks before every command that is not a known read-only one and before every
//! file change, which is what "manual" means in twr. With `builtin_tools: Some([])` (no
//! built-ins, e.g. the orchestrator) the sandbox is `read-only` whatever the mode (also noted in
//! `mode_note`).
//!
//! ### The sandbox's network and writable roots
//!
//! A `workspace-write` sandbox takes its network access and its writable roots from the
//! user's Codex config and the project's (`[sandbox_workspace_write] network_access`,
//! `writable_roots`), whatever the request says, unless the request overrides them (verified
//! with Codex 0.160.0). So every mode with that sandbox gets both as `config` overrides
//! ([`sandbox_overrides`]), always, even when empty: no network and no extra roots, except in
//! `auto` (below). The attach command passes the same overrides as `-c`.
//!
//! ### The `auto` sandbox
//!
//! Claude Code's auto mode lets routine builds, tests and commits run without asking. Codex's
//! plain `workspace-write` sandbox does not: it has no network, which also blocks every socket
//! (tests binding a unix socket in `/tmp` fail with `EPERM`), and a linked worktree's git
//! metadata lives in the main repository's `.git`, outside the worktree, so `git commit` fails
//! (Codex also keeps a worktree's own git dir read-only when a wider root holds it, verified
//! with Codex 0.160.0). An `auto` thread's sandbox therefore gets:
//!
//! - `sandbox_workspace_write.network_access = true`;
//! - `sandbox_workspace_write.writable_roots` = [`auto_writable_roots`]: the worktree's git dir
//!   and the common dir's `objects`, `refs` and `logs` (not its `config` or `hooks`), found
//!   from the repository the thread was made from ([`SpawnConfig::repo`]) and not from the
//!   worktree's own pointers, which the agent can rewrite, and the cargo and npm caches.
//!   These replace any `writable_roots` of the user's Codex config.
//!
//! Network access opens every local socket too, the user's D-Bus included: a command could
//! start another one outside the sandbox (`systemd-run --user`). That is as open as Claude
//! Code's auto mode, which has no OS sandbox; writes elsewhere still ask.
//!
//! `approvalsReviewer: "user"` is always sent: a user config with `approvals_reviewer =
//! "auto_review"` would otherwise send approvals to Codex's reviewer agent instead of twr.
//!
//! What the thread runs with is checked, not assumed: the `thread/start` / `thread/resume`
//! result must report an `approvalPolicy`, a `sandbox.type` and an `approvalsReviewer`, the
//! first two no wider than requested (policies `untrusted` < `on-request`/`on-failure` <
//! `never`; sandboxes `readOnly` < `workspaceWrite` < `dangerFullAccess`; anything else is
//! unknown) and the reviewer exactly `user`; short of `dangerFullAccess`, the sandbox's
//! `networkAccess` may be true only when twr asked for it, and each of its `writableRoots`
//! must be one twr asked for. Otherwise the session ends before any prompt
//! ([`translate::check_policy`]). The sandbox's `networkAccess` and `writableRoots` are
//! also reported as `ext.codex.sandbox_network` and `ext.codex.sandbox_writable_roots`.
//!
//! Process handling matches the Claude Code adapter: own process group, `SpawnConfig::env`
//! added and twr's secrets removed, stdout lines capped at [`crate::process::MAX_LINE`] (a
//! skipped `turn/completed` still ends the turn as an error), stdin written by a dedicated task, the last 4 KB of stderr kept for `Exited`, the
//! group killed on every exit path. `stop` interrupts a running turn (`turn/interrupt`), closes
//! stdin (Codex exits on EOF), waits up to [`STOP_GRACE`], then kills the group.
//!
//! `ext.twr.exe` is only reported when the binary is not a script: the npm `codex` is a Node
//! script, so its process's `argv[0]` is `node`, and the hub's stale-process check would never
//! match it. Without `exe` the hub still matches the start time and the `TWR_THREAD` marker.

pub mod translate;

use std::collections::VecDeque;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterEvent, AdapterHandle, AdapterInfo, AttachApplies,
    SpawnConfig, Tier,
};

pub use translate::{Emitted, Open, Translated, Translator, TranslatorOptions};

use crate::acp::translate::{native_mode, ModeLevel};
use crate::process::{self, BoundedLines, Line};

/// Adapter name (`source = adapter:codex`).
pub const NAME: &str = "codex";

/// Environment variable overriding the `codex` binary (tests point it at a stub).
pub const BIN_ENV: &str = "TWR_CODEX_BIN";

/// How long `stop` waits for the process to exit after closing stdin.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// How long stdout is still read after the process exited.
const DRAIN: Duration = Duration::from_secs(1);

/// Bytes of stderr kept for `Exited::stderr_tail`.
pub const STDERR_TAIL: usize = 4096;

/// The Codex runtime adapter.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    bin: Option<PathBuf>,
    stop_grace: Duration,
    max_line: usize,
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl CodexAdapter {
    /// Uses [`BIN_ENV`] if set, else `codex` from `PATH`.
    pub fn new() -> Self {
        CodexAdapter {
            bin: None,
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
        }
    }

    /// Uses `bin` as the `codex` binary.
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        CodexAdapter {
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
    /// else [`BIN_ENV`], else `codex`; an empty one counts as unset).
    pub fn bin(&self) -> PathBuf {
        crate::resolve::select_program(NAME, self.bin.as_deref(), |k| std::env::var_os(k))
            .unwrap_or_else(|| PathBuf::from("codex"))
    }
}

// ---------------------------------------------------------------------------------------------
// SpawnConfig mapping
// ---------------------------------------------------------------------------------------------

/// What to do when the `codex` program is not there ([`process::start_error`]).
fn missing_fix() -> String {
    format!("install Codex, or set {BIN_ENV}")
}

/// Arguments of the app-server process. Everything else goes over stdin.
pub fn build_args() -> Vec<String> {
    vec!["app-server".into(), "--stdio".into()]
}

/// `(approvalPolicy, sandbox)` for a permission mode: the table in the module doc
/// ("Permission modes"). Never wider than the mode; with `builtin_tools: Some([])` the sandbox
/// is `read-only` whatever the mode. An unknown mode is an error ([`NATIVE_MODES`],
/// [`crate::acp::translate::native_mode`]): the runtime does not start.
pub fn policy(
    mode: &str,
    builtin_tools: Option<&[String]>,
) -> anyhow::Result<(&'static str, &'static str)> {
    let (approval, sandbox, _) = mode_policy(mode)?;
    let no_builtins = builtin_tools.is_some_and(<[String]>::is_empty);
    Ok((approval, if no_builtins { "read-only" } else { sandbox }))
}

/// Why the Codex policy for `mode` is not an exact match of it (None when it is, or when the
/// mode is unknown: [`policy`] refuses it).
pub fn mode_note(mode: &str, builtin_tools: Option<&[String]>) -> Option<String> {
    let (_, sandbox, note) = mode_policy(mode).ok()?;
    let mut parts: Vec<String> = note
        .map(|n| format!("mode {:?}: {n}", mode.trim()))
        .into_iter()
        .collect();
    let no_builtins = builtin_tools.is_some_and(<[String]>::is_empty);
    if no_builtins && sandbox != "read-only" {
        parts.push("no built-in tools: sandbox read-only".into());
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// The reasoning efforts twr passes to Codex (`config.model_reasoning_effort`): every value the
/// models of Codex 0.159.2 advertise (`model/list` `supportedReasoningEfforts`, checked
/// 2026-10-03: `low` to `xhigh` for all, `max` and `ultra` for most) plus `minimal` of older
/// models. Codex itself takes any string at thread start (a made-up value is reported back
/// as is), so the hub checks against this list; whether the thread's model supports the value
/// is Codex's to say.
pub const EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];

/// The mode ids this adapter maps exactly as written (twr's and Codex's own); any other
/// spelling of one of them (`Read_Only`, `dont-ask`) maps like it, and another known mode maps
/// by its level ([`level_mode`]).
pub const NATIVE_MODES: &[&str] = &[
    "manual",
    "default",
    "untrusted",
    "plan",
    "read-only",
    "on-request",
    "auto",
    "acceptEdits",
    "dontAsk",
    "never",
    "bypassPermissions",
    "danger-full-access",
    "yolo",
];

/// The [`NATIVE_MODES`] id a known mode of `level` maps like: restricted modes get the
/// read-only sandbox with every command asking (`plan`).
fn level_mode(level: ModeLevel) -> &'static str {
    match level {
        ModeLevel::Restricted => "plan",
        ModeLevel::Ask => "manual",
        ModeLevel::AcceptEdits => "acceptEdits",
        ModeLevel::Auto => "auto",
        ModeLevel::Bypass => "bypassPermissions",
    }
}

/// The [`NATIVE_MODES`] id `mode` maps like (`""` is `manual`); an unknown mode is an error.
fn native(mode: &str) -> anyhow::Result<&'static str> {
    match mode.trim() {
        "" => Ok("manual"),
        m => native_mode(m, NATIVE_MODES, level_mode).map_err(|e| anyhow::anyhow!("codex: {e}")),
    }
}

/// How much `mode` lets a Codex thread run without asking, by the [`NATIVE_MODES`] id it maps
/// like ([`native`]; `""` is `manual`); None for a mode Codex refuses. Codex's own ids classify
/// by their policy ([`mode_policy`]): `untrusted` and `on-request` ask, `never` is auto (nothing
/// asks; commands run in the workspace-write sandbox, and what it refuses fails), and
/// `danger-full-access` and `yolo` bypass. twr's ids keep their own level
/// ([`crate::acp::translate::mode_level`]): `acceptEdits` is accept-edits, though Codex asks
/// for its edits too.
pub fn mode_level(mode: &str) -> Option<ModeLevel> {
    Some(match native(mode).ok()? {
        "plan" | "read-only" | "dontAsk" => ModeLevel::Restricted,
        "manual" | "default" | "untrusted" | "on-request" => ModeLevel::Ask,
        "acceptEdits" => ModeLevel::AcceptEdits,
        "auto" | "never" => ModeLevel::Auto,
        "bypassPermissions" | "danger-full-access" | "yolo" => ModeLevel::Bypass,
        // Every NATIVE_MODES id is above (a test checks).
        _ => return None,
    })
}

/// What a Codex thread in `mode` may do, as capabilities to compare one by one (each higher:
/// more): its approval policy and its sandbox ([`policy`], ranked as
/// [`translate::check_policy`] ranks them), its network access and where it may write
/// besides the worktree. `auto` has network and extra roots (module doc); the full-access
/// sandbox (`bypassPermissions`, `danger-full-access`, `yolo`) has network and may write
/// anywhere, the top of both. Two modes of one level can differ here: `on-request` runs
/// sandboxed commands without asking where `manual` (`untrusted`) asks, and `auto` has
/// network and roots that `never` lacks. None for a mode Codex refuses.
pub fn mode_capabilities(mode: &str) -> Option<[u8; 4]> {
    let (approval, sandbox) = policy(mode, None).ok()?;
    let sandbox = translate::sandbox_rank(sandbox)?;
    let auto = matches!(native(mode), Ok("auto"));
    let (network, writes) = match (sandbox, auto) {
        (2, _) => (1, 2),
        (_, true) => (1, 1),
        _ => (0, 0),
    };
    Some([
        translate::approval_rank(approval)?,
        sandbox,
        network,
        writes,
    ])
}

/// `(approvalPolicy, sandbox, note when inexact)` for a mode; see [`policy`].
fn mode_policy(mode: &str) -> anyhow::Result<(&'static str, &'static str, Option<&'static str>)> {
    Ok(match native(mode)? {
        "manual" | "default" | "untrusted" => ("untrusted", "workspace-write", None),
        "plan" => (
            "untrusted",
            "read-only",
            Some("Codex has no plan mode; read-only sandbox, every command and edit asks"),
        ),
        "read-only" => ("on-request", "read-only", None),
        "on-request" => ("on-request", "workspace-write", None),
        "auto" => (
            "on-request",
            "workspace-write",
            Some(
                "Codex has no auto mode; approvalPolicy on-request with a workspace-write \
                 sandbox that also has network access and can write the worktree's git \
                 metadata and the cargo and npm caches: commands inside it run without \
                 asking, anything beyond it asks",
            ),
        ),
        "acceptEdits" => (
            "untrusted",
            "workspace-write",
            Some(
                "Codex cannot accept file edits without also widening shell autonomy; \
                 approvalPolicy untrusted: edits and commands still ask",
            ),
        ),
        "dontAsk" => (
            "never",
            "read-only",
            Some(
                "deny by default: approvalPolicy never with a read-only sandbox; nothing asks, \
                 writes and commands needing more fail (MCP tools approved via allowed_tools \
                 still run)",
            ),
        ),
        "never" => ("never", "workspace-write", None),
        "bypassPermissions" | "danger-full-access" | "yolo" => {
            ("never", "danger-full-access", None)
        }
        other => unreachable!("{other} is in NATIVE_MODES and mapped above"),
    })
}

/// The config key of the `workspace-write` sandbox's network access.
pub const NETWORK_KEY: &str = "sandbox_workspace_write.network_access";

/// The config key of the `workspace-write` sandbox's writable roots (besides the cwd and the
/// temp dirs).
pub const WRITABLE_ROOTS_KEY: &str = "sandbox_workspace_write.writable_roots";

/// The sandbox's config overrides for a thread (module doc, "The sandbox's network and
/// writable roots"): with a `workspace-write` sandbox, always both [`NETWORK_KEY`] (true only
/// in `auto`) and [`WRITABLE_ROOTS_KEY`] (in `auto` [`auto_writable_roots`], else none), even
/// when that is `[]`: the user's or a project's Codex config
/// (`[sandbox_workspace_write] network_access`, `writable_roots`) would otherwise widen any
/// mode's sandbox (verified with Codex 0.160.0). Empty for the other sandboxes (`read-only`,
/// also with no built-in tools; `danger-full-access`) and an unknown mode. Without
/// `local_roots` (a command for another box, whose paths twr cannot see from here) `auto`
/// gets no writable roots either.
pub fn sandbox_overrides(cfg: &SpawnConfig, local_roots: bool) -> Map<String, Value> {
    let mut m = Map::new();
    let Ok((_, sandbox)) = policy(&cfg.mode, cfg.builtin_tools.as_deref()) else {
        return m;
    };
    if sandbox != "workspace-write" {
        return m;
    }
    let auto = matches!(native(&cfg.mode), Ok("auto"));
    m.insert(NETWORK_KEY.into(), json!(auto));
    let roots: Vec<String> = if auto && local_roots {
        auto_writable_roots(&cfg.cwd, cfg.repo.as_deref(), |k| spawn_var(cfg, k))
            .iter()
            .map(|r| r.to_string_lossy().into_owned())
            .collect()
    } else {
        Vec::new()
    };
    m.insert(WRITABLE_ROOTS_KEY.into(), json!(roots));
    m
}

/// Variable `k` as the runtime process gets it: `cfg.env`, else twr's own environment.
fn spawn_var(cfg: &SpawnConfig, k: &str) -> Option<std::ffi::OsString> {
    match cfg.env.iter().rev().find(|(name, _)| name == k) {
        Some((_, v)) => Some(v.into()),
        None => std::env::var_os(k),
    }
}

/// The directories an `auto` thread in `cwd` may write besides `cwd` and the temp dirs
/// (canonical, each once, only those that exist as directories the owner of `cwd` can
/// write: Codex's Linux sandbox fails every command when a writable root is one the user
/// cannot write):
///
/// - when `cwd` is in a linked worktree of `repo` (the repository the thread was made from,
///   [`SpawnConfig::repo`]), its git dir (`<common>/worktrees/<name>`: index, `HEAD`, rebase
///   state) and the common dir's `objects`, `refs` and `logs`: enough to stage, commit,
///   stash, tag and fetch. The common `config` and `hooks` stay read-only (git runs what they
///   name, outside the sandbox), and so do the git dirs of a main checkout and a submodule,
///   which hold them. The worktree's `.git` file and its git dir's `commondir` are the
///   agent's to rewrite (both are writable), so neither decides: the common dir is `repo`'s
///   ([`linked_worktree_git_dirs`]), and the worktree's pointers must lead to one of its
///   worktree records and back to it, through no symbolic link. Otherwise (or with no
///   `repo`) no git metadata is writable, and a commit asks;
/// - the cargo caches `$CARGO_HOME/registry` and `$CARGO_HOME/git` (`CARGO_HOME` defaulting
///   to `~/.cargo`; its `bin` and `config.toml` stay read-only);
/// - the npm cache, `$npm_config_cache` (or `NPM_CONFIG_CACHE`) else `~/.npm`.
///
/// `env` looks up variables (empty counts as unset).
pub fn auto_writable_roots(
    cwd: &Path,
    repo: Option<&Path>,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;
    let var = |k: &str| {
        env(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
    };
    let mut roots = Vec::new();
    if let Some((git_dir, common)) = repo.and_then(|r| linked_worktree_git_dirs(cwd, r)) {
        roots.push(git_dir);
        // Each a directory of the common dir itself, not a link out of it.
        roots.extend(
            ["objects", "refs", "logs"]
                .map(|d| common.join(d))
                .into_iter()
                .filter(|p| p.canonicalize().is_ok_and(|c| c == *p)),
        );
    }
    let home = var("HOME");
    if let Some(cargo) = var("CARGO_HOME").or_else(|| home.as_ref().map(|h| h.join(".cargo"))) {
        roots.extend(["registry", "git"].map(|d| cargo.join(d)));
    }
    if let Some(npm) = var("npm_config_cache")
        .or_else(|| var("NPM_CONFIG_CACHE"))
        .or_else(|| home.as_ref().map(|h| h.join(".npm")))
    {
        roots.push(npm);
    }
    let Ok(owner) = std::fs::metadata(cwd).map(|m| m.uid()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let Ok(r) = r.canonicalize() else { continue };
        let writable = std::fs::metadata(&r)
            .is_ok_and(|m| m.is_dir() && m.uid() == owner && m.mode() & 0o200 != 0);
        if writable && !out.contains(&r) {
            out.push(r);
        }
    }
    out
}

/// `(git dir, common dir)` of the linked worktree of `repo` that `cwd` is in (canonical).
///
/// The common dir is `repo`'s ([`common_dir_of`]), found outside `cwd`: the worktree is the
/// agent's to write, and so are its git dir's files. Which of the common dir's worktree
/// records is this worktree's is git's own back-pointer, read from the records: the one whose
/// `gitdir` names this worktree's `.git` (what `git worktree list` reads). An agent can write
/// only its own record (its git dir), never a sibling's, so it cannot make another worktree's
/// record name it; when more than one record names it (a sibling's agent pointed its own
/// record here), none is taken. The worktree's own pointers must then agree: the nearest
/// `.git` at or above `cwd` (as git finds it) is a regular file, not a link, naming that
/// record (through no symbolic link: canonical, it is `<common>/worktrees/<name>`), and the
/// record's `commondir` leads back to the common dir. So a `.git` repointed at a sibling's
/// record, or linked to a sibling's `.git`, grants nothing. None for a main checkout (`.git`
/// is a directory), a submodule, a worktree of another repository, any pointer that
/// disagrees, or no repository.
fn linked_worktree_git_dirs(cwd: &Path, repo: &Path) -> Option<(PathBuf, PathBuf)> {
    let cwd = cwd.canonicalize().ok()?;
    let common = common_dir_of(repo, &cwd)?;
    let dot_git = cwd
        .ancestors()
        .map(|d| d.join(".git"))
        .find(|p| std::fs::symlink_metadata(p).is_ok())?;
    if !std::fs::symlink_metadata(&dot_git).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let record = worktree_record(&common, &dot_git)?;
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    let git_dir = dot_git.parent()?.join(target).canonicalize().ok()?;
    let name = git_dir.file_name()?;
    if git_dir != record || git_dir != common.join("worktrees").join(name) {
        return None;
    }
    let back = std::fs::read_to_string(git_dir.join("commondir")).ok()?;
    let back = git_dir.join(back.trim()).canonicalize().ok()?;
    (back == common).then_some((git_dir, common))
}

/// The one record of `<common>/worktrees` (canonical) whose `gitdir` names `dot_git` (a
/// worktree's `.git`, its parent canonical): its directory compared after resolving links,
/// the file name `.git`. None when no record or more than one does.
fn worktree_record(common: &Path, dot_git: &Path) -> Option<PathBuf> {
    let worktree = dot_git.parent()?;
    let mut found = None;
    for entry in std::fs::read_dir(common.join("worktrees")).ok()?.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let record = entry.path();
        let Ok(text) = std::fs::read_to_string(record.join("gitdir")) else {
            continue;
        };
        let named = record.join(text.trim_end_matches(['\n', '\r']));
        let names_it = named.file_name() == Some(std::ffi::OsStr::new(".git"))
            && named
                .parent()
                .and_then(|d| d.canonicalize().ok())
                .is_some_and(|d| d == worktree);
        if names_it {
            if found.is_some() {
                return None;
            }
            found = Some(record.canonicalize().ok()?);
        }
    }
    found
}

/// The git common dir of repository `repo` (canonical): `repo/.git` when it is a directory,
/// the common dir its `.git` file leads to (a linked worktree's `commondir`, else the git dir
/// itself), or `repo` when it is a bare repository. None when there is none, or when it, or
/// a pointer read on the way, lies inside `cwd` (the agent's to write).
fn common_dir_of(repo: &Path, cwd: &Path) -> Option<PathBuf> {
    let repo = repo.canonicalize().ok()?;
    let inside = |p: &Path| p.starts_with(cwd);
    let dot_git = repo.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git);
    let common = match meta {
        Ok(m) if m.is_dir() => dot_git.canonicalize().ok()?,
        Ok(m) if m.is_file() => {
            if inside(&dot_git) {
                return None;
            }
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let target = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            let git_dir = repo.join(target).canonicalize().ok()?;
            if inside(&git_dir) {
                return None;
            }
            match std::fs::read_to_string(git_dir.join("commondir")) {
                Ok(c) => git_dir.join(c.trim()).canonicalize().ok()?,
                Err(_) => git_dir,
            }
        }
        Ok(_) => return None,
        Err(_) if repo.join("HEAD").is_file() && repo.join("objects").is_dir() => repo,
        Err(_) => return None,
    };
    (!inside(&common)).then_some(common)
}

/// Config overrides turning off the built-in tool groups `builtin_tools` does not name.
///
/// Codex cannot enable built-ins one by one; twr maps names to feature switches:
///
/// | group     | selected by                                 | turned off with                          |
/// |-----------|---------------------------------------------|------------------------------------------|
/// | shell     | `shell`, `Bash`, `exec_command`             | `features.shell_tool`, `features.unified_exec`, `features.code_mode_host` = false |
/// | web       | `web_search`, `WebSearch`, `WebFetch`       | `web_search = "disabled"`                |
/// | agents    | `spawn_agent`, `Task`, `Agent`              | `features.multi_agent`, `features.multi_agent_v2` = false |
/// | apps      | `apps`                                      | `features.apps` = false                  |
/// | browser   | `browser_use`, `computer_use`               | `features.browser_use`, `features.computer_use` = false |
/// | images    | `image_generation`                          | `features.image_generation` = false      |
///
/// `apply_patch`, `view_image` and `request_user_input` cannot be turned off; file changes still
/// need approval under `untrusted` and fail in a `read-only` sandbox. Verified live: with the
/// shell group off, Codex reports no shell tool and cannot run commands.
pub fn builtin_overrides(builtin_tools: Option<&[String]>) -> Map<String, Value> {
    let mut m = Map::new();
    let Some(names) = builtin_tools else {
        return m;
    };
    let has = |any: &[&str]| names.iter().any(|n| any.contains(&n.as_str()));
    let off = |m: &mut Map<String, Value>, keys: &[&str]| {
        for k in keys {
            m.insert(format!("features.{k}"), json!(false));
        }
    };
    if !has(&["shell", "Bash", "exec_command"]) {
        off(&mut m, &["shell_tool", "unified_exec", "code_mode_host"]);
    }
    if !has(&["web_search", "WebSearch", "WebFetch"]) {
        m.insert("web_search".into(), json!("disabled"));
    }
    if !has(&["spawn_agent", "Task", "Agent"]) {
        off(&mut m, &["multi_agent", "multi_agent_v2"]);
    }
    if !has(&["apps"]) {
        off(&mut m, &["apps"]);
    }
    if !has(&["browser_use", "computer_use"]) {
        off(&mut m, &["browser_use", "computer_use"]);
    }
    if !has(&["image_generation"]) {
        off(&mut m, &["image_generation"]);
    }
    m
}

/// The MCP configuration of `cfg`: `mcp_config_file` (read now) wins over `mcp_config`.
fn mcp_source(cfg: &SpawnConfig) -> anyhow::Result<Option<Value>> {
    if let Some(file) = &cfg.mcp_config_file {
        let mut text = String::new();
        std::fs::File::open(file)
            .and_then(|mut f| f.read_to_string(&mut text))
            .with_context(|| format!("reading MCP config {}", file.display()))?;
        let v: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing MCP config {}", file.display()))?;
        return Ok(Some(v));
    }
    Ok(cfg.mcp_config.clone().filter(|v| !v.is_null()))
}

/// Codex `mcp_servers` table for an MCP configuration in Claude's shape
/// (`{"mcpServers": {name: {command, args, env}}}`) or Codex's (`{"mcp_servers": {...}}`).
///
/// Per server: `command`, `args`, `env`, `cwd`, `url` are kept; Claude's `headers` becomes
/// `http_headers`; Codex keys (`http_headers`, `bearer_token_env_var`, `enabled_tools`,
/// `disabled_tools`, `startup_timeout_sec`, `tool_timeout_sec`, `default_tools_approval_mode`,
/// `enabled`) pass through; anything else (e.g. Claude's `type`) is dropped. When
/// `allowed_tools` has `mcp__<name>` (Claude's rule for all of a server's tools), the server's
/// tools run without approval (`default_tools_approval_mode = "approve"`, verified live).
pub fn mcp_servers(config: &Value, allowed_tools: &[String]) -> anyhow::Result<Map<String, Value>> {
    let servers = config
        .get("mcpServers")
        .or_else(|| config.get("mcp_servers"))
        .unwrap_or(config);
    let servers = servers
        .as_object()
        .context("MCP config: expected an object of servers under \"mcpServers\"")?;
    let mut out = Map::new();
    for (name, spec) in servers {
        let spec = spec
            .as_object()
            .with_context(|| format!("MCP config: server {name:?} is not an object"))?;
        let mut s = Map::new();
        for key in [
            "command",
            "args",
            "env",
            "cwd",
            "url",
            "http_headers",
            "bearer_token_env_var",
            "enabled_tools",
            "disabled_tools",
            "startup_timeout_sec",
            "tool_timeout_sec",
            "default_tools_approval_mode",
            "enabled",
        ] {
            if let Some(v) = spec.get(key).filter(|v| !v.is_null()) {
                s.insert(key.into(), v.clone());
            }
        }
        if let Some(h) = spec.get("headers").filter(|v| !v.is_null()) {
            s.entry("http_headers").or_insert_with(|| h.clone());
        }
        let rule = format!("mcp__{name}");
        if allowed_tools
            .iter()
            .any(|t| *t == rule || *t == format!("{rule}__*"))
        {
            s.insert("default_tools_approval_mode".into(), json!("approve"));
        }
        out.insert(name.clone(), Value::Object(s));
    }
    Ok(out)
}

/// The `config` param (Codex config overrides) for `cfg`, if any.
pub fn thread_config(cfg: &SpawnConfig) -> anyhow::Result<Option<Map<String, Value>>> {
    let mut config = builtin_overrides(cfg.builtin_tools.as_deref());
    config.extend(sandbox_overrides(cfg, true));
    // `thread/start` and `thread/resume` have no effort param (only `turn/start` does); the
    // config override sets it for the thread (verified with Codex 0.159.2: the result's
    // `reasoningEffort` is the one given, on a resume in a new process too).
    if let Some(e) = cfg.effort.as_deref().filter(|e| !e.is_empty()) {
        config.insert("model_reasoning_effort".into(), json!(e));
    }
    let mcp = mcp_source(cfg)?;
    let servers = match &mcp {
        Some(v) => Some(mcp_servers(v, &cfg.allowed_tools)?),
        None => None,
    };
    for t in &cfg.allowed_tools {
        let covered = servers.as_ref().is_some_and(|s| {
            s.keys()
                .any(|n| *t == format!("mcp__{n}") || *t == format!("mcp__{n}__*"))
        });
        if !covered {
            tracing::debug!(rule = %t, "codex: allowed_tools rule has no Codex equivalent; ignored");
        }
    }
    if let Some(servers) = servers {
        config.insert("mcp_servers".into(), Value::Object(servers));
        // A strict set: plugins and apps bring MCP servers of their own. The user's own
        // servers are turned off by name once the config is read (`translate`, "MCP
        // allowlist").
        for feature in STRICT_MCP_FEATURES_OFF {
            config.insert(format!("features.{feature}"), json!(false));
        }
    }
    Ok((!config.is_empty()).then_some(config))
}

/// The features turned off for a thread with an MCP allowlist (`mcp_config` /
/// `mcp_config_file`, such as the orchestrator's): plugins and apps (connectors) start MCP
/// servers of their own (`codex_apps` for apps, verified with Codex 0.160.0).
pub const STRICT_MCP_FEATURES_OFF: &[&str] = &["apps", "plugins"];

/// How the thread is opened: `thread/start`, or `thread/resume` when `cfg.resume_session` is
/// set. Every setting is sent both ways (Codex does not keep the permission settings across
/// processes; a model or effort left out keeps the thread's: [`crate::resume_keeps`]).
pub fn thread_params(cfg: &SpawnConfig) -> anyhow::Result<Open> {
    let (approval, sandbox) = policy(&cfg.mode, cfg.builtin_tools.as_deref())?;
    let mode_note = mode_note(&cfg.mode, cfg.builtin_tools.as_deref());
    if let Some(note) = &mode_note {
        tracing::info!(mode = %cfg.mode, approval, sandbox, %note, "codex: permission mode mapped inexactly");
    }
    let mut p = Map::new();
    if let Some(id) = &cfg.resume_session {
        p.insert("threadId".into(), json!(id));
    }
    if !cfg.cwd.as_os_str().is_empty() {
        p.insert("cwd".into(), json!(cfg.cwd.to_string_lossy()));
    }
    p.insert("approvalPolicy".into(), json!(approval));
    p.insert("approvalsReviewer".into(), json!("user"));
    p.insert("sandbox".into(), json!(sandbox));
    if let Some(m) = cfg.model.as_deref().filter(|m| !m.is_empty()) {
        p.insert("model".into(), json!(m));
    }
    if let Some(s) = cfg.system_append.as_deref().filter(|s| !s.is_empty()) {
        p.insert("developerInstructions".into(), json!(s));
    }
    if let Some(c) = thread_config(cfg)? {
        p.insert("config".into(), Value::Object(c));
    }
    let method = if cfg.resume_session.is_some() {
        p.insert("excludeTurns".into(), json!(true));
        "thread/resume"
    } else {
        "thread/start"
    };
    Ok(Open {
        method,
        params: Value::Object(p),
        mode_note,
    })
}

/// The program to record as `ext.twr.exe`: `bin`, unless it is a script (`#!`), whose process
/// shows the interpreter as `argv[0]`. `found` is the file it resolved to
/// ([`crate::resolve::resolve`], on the runtime's `PATH`).
fn exe_of(bin: &Path, found: Option<&Path>) -> Option<String> {
    let path = found?;
    let mut head = [0u8; 2];
    let n = std::fs::File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .ok()?;
    (n < 2 || &head != b"#!").then(|| bin.to_string_lossy().into_owned())
}

impl CodexAdapter {
    /// The attach command ([`Adapter::attach_command`]); `local_roots` as for
    /// [`sandbox_overrides`]. The built-in tool groups `cfg` turns off stay off
    /// ([`builtin_overrides`] as `-c`). None for a session id twr does not put on a command line
    /// ([`crate::valid_session_id`]), and for a thread with an MCP allowlist (`mcp_config` /
    /// `mcp_config_file`): `codex resume` would load the servers of the user's and the
    /// project's Codex config, which a `-c` override can only turn off by name (the hub refuses
    /// to attach the orchestrator).
    fn resume_command(
        &self,
        cfg: &SpawnConfig,
        session_id: &str,
        local_roots: bool,
    ) -> Option<std::process::Command> {
        if !crate::valid_session_id(session_id)
            || cfg.mcp_config_file.is_some()
            || cfg.mcp_config.as_ref().is_some_and(|v| !v.is_null())
        {
            return None;
        }
        let (approval, sandbox) = policy(&cfg.mode, cfg.builtin_tools.as_deref()).ok()?;
        let mut cmd = std::process::Command::new(self.bin());
        cmd.args([
            "resume",
            session_id,
            "--ask-for-approval",
            approval,
            "--sandbox",
            sandbox,
            "-c",
            "approvals_reviewer=\"user\"",
        ]);
        for (k, v) in sandbox_overrides(cfg, local_roots) {
            cmd.args(["-c".to_string(), format!("{k}={v}")]);
        }
        for (k, v) in builtin_overrides(cfg.builtin_tools.as_deref()) {
            cmd.args(["-c".to_string(), format!("{k}={v}")]);
        }
        if let Some(m) = cfg.model.as_deref().filter(|m| !m.is_empty()) {
            cmd.args(["--model", m]);
        }
        if let Some(e) = cfg.effort.as_deref().filter(|e| !e.is_empty()) {
            cmd.args([
                "-c".to_string(),
                format!("model_reasoning_effort={}", json!(e)),
            ]);
        }
        cmd.envs(process::attach_env(cfg));
        process::scrub_std(&mut cmd);
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        Some(cmd)
    }
}

impl Adapter for CodexAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Headless,
            description: "Codex via `codex app-server --stdio` JSON-RPC (approvals over stdio)",
        }
    }

    fn program(&self) -> Option<(PathBuf, String)> {
        Some((self.bin(), missing_fix()))
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let bin = self.bin();
        let open = thread_params(&cfg)?;
        let mut cmd = process::wrapped(&cfg.wrap, &bin);
        cmd.args(build_args())
            .envs(cfg.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        process::scrub(&mut cmd);
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        process::check_wrapped(&cfg.wrap, &bin, &cfg.env, &cfg.cwd, &missing_fix())?;
        let child = cmd
            .spawn()
            .map_err(|e| process::start_error(e, &bin, &cfg.env, &cfg.cwd, &missing_fix()))?;
        tracing::info!(
            thread = %cfg.thread,
            pid = ?child.id(),
            method = open.method,
            session = ?cfg.resume_session,
            "codex: started"
        );
        let translator = Translator::new(
            TranslatorOptions {
                thinking: cfg.thinking,
                pid: child.id(),
                exe: exe_of(
                    &bin,
                    crate::resolve::resolve(&bin, &cfg.env, None, &cfg.cwd)
                        .found
                        .as_deref(),
                ),
                client_version: env!("CARGO_PKG_VERSION").into(),
            },
            open,
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let (handle, end) = AdapterHandle::pair();
        if let Some(pid) = child.id() {
            end.spawned(pid);
        }
        tokio::spawn(drive(
            child,
            translator,
            end,
            first,
            self.stop_grace,
            self.max_line,
        ));
        Ok(handle)
    }

    /// `codex resume <thread id>` in the thread's directory, with the same approval policy,
    /// sandbox (the `auto` sandbox's overrides too), reviewer, model and reasoning effort
    /// (`-c model_reasoning_effort=...`) as the headless session. MCP servers and developer
    /// instructions are not passed again (the orchestrator's token is never put on a command
    /// line).
    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        self.resume_command(cfg, session_id, true)
    }

    /// [`Adapter::attach_command`] without the `auto` sandbox's writable roots: they are
    /// resolved from this box's files, and the command runs on another box. Its network
    /// override is kept; a commit there asks.
    fn remote_attach_command(
        &self,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> Option<std::process::Command> {
        self.resume_command(cfg, session_id, false)
    }

    /// Each setting the thread has (`--model`, `-c model_reasoning_effort=...`). One left to
    /// the runtime's default is not passed, and Codex keeps the session's own on resume
    /// (`crate::resume_keeps`).
    fn attach_applies(&self, cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies {
            model: cfg.model.as_deref().is_some_and(|m| !m.is_empty()),
            effort: cfg.effort.as_deref().is_some_and(|e| !e.is_empty()),
        }
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
            tracing::warn!(error = %e, "codex: writing to stdin failed");
            return;
        }
    }
}

/// Queues one line for the runtime's stdin. Never waits.
fn queue_line(stdin: &mut Stdin, line: String) {
    let Some(tx) = stdin.as_ref() else {
        tracing::warn!("codex: stdin already closed; dropping a line");
        return;
    };
    if tx.send(line).is_err() {
        tracing::warn!("codex: stdin writer is gone; dropping a line");
        *stdin = None;
    }
}

/// Sends `SIGKILL` to process group `pgid` with the `kill` command (no libc in this crate).
fn kill_group(pgid: u32) {
    let status = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
    tracing::debug!(pgid, ?status, "codex: killed process group");
}

/// Helper tasks of one runtime process.
struct Tasks {
    writer: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

/// What the driver does after handling a translation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Continue,
    /// The hub dropped the event stream, or the session cannot go on.
    Stop,
}

async fn drive(
    mut child: Child,
    mut tr: Translator,
    mut end: AdapterEnd,
    first: Option<(String, u64)>,
    grace: Duration,
    max_line: usize,
) {
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

    queue_line(&mut stdin, tr.start());
    if let Some((prompt, turn)) = first {
        for l in tr.send(&prompt, turn) {
            queue_line(&mut stdin, l);
        }
    }

    let mut stop_deadline: Option<tokio::time::Instant> = None;
    let mut stdout_open = true;
    let mut background_tick = tokio::time::interval(Duration::from_secs(5));
    background_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = background_tick.tick(), if stop_deadline.is_none() => {
                for line in tr.poll_background() { queue_line(&mut stdin, line); }
            }
            line = lines.next_line(), if stdout_open => match line {
                Ok(Some(l)) => {
                    let out = translate_line(&mut tr, l);
                    if emit(&mut stdin, &end, &tail, out) == After::Stop && stop_deadline.is_none() {
                        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                    }
                }
                Ok(None) => stdout_open = false,
                Err(e) => {
                    tracing::warn!(error = %e, "codex: reading stdout failed");
                    stdout_open = false;
                }
            },
            status = child.wait() => {
                // Leftovers in the group (e.g. commands Codex left running) die with it,
                // whatever the exit path. Lines already in the pipe stay readable.
                if let Some(p) = pid {
                    kill_group(p);
                }
                if stdout_open {
                    let drain_until = tokio::time::Instant::now() + DRAIN;
                    while let Ok(Ok(Some(l))) =
                        tokio::time::timeout_at(drain_until, lines.next_line()).await
                    {
                        let out = translate_line(&mut tr, l);
                        emit(&mut stdin, &end, &tail, out);
                    }
                }
                return finish(status, pid, &end, &tail, tasks).await;
            }
            cmd = end.cmds.recv(), if stop_deadline.is_none() => match cmd {
                Some(AdapterCmd::Send { text, turn }) => {
                    for l in tr.send(&text, turn) {
                        queue_line(&mut stdin, l);
                    }
                }
                Some(AdapterCmd::Respond { attn_id, answer }) => match tr.respond(&attn_id, &answer) {
                    Some(out) => {
                        if emit(&mut stdin, &end, &tail, out) == After::Stop {
                            stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                        }
                    }
                    None => tracing::warn!(%attn_id, "codex: ignoring respond for an attention that is not pending"),
                },
                Some(AdapterCmd::Stop) | None => {
                    stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace));
                }
                Some(AdapterCmd::Interrupt) => {
                    let out = tr.interrupt_turn();
                    emit(&mut stdin, &end, &tail, out);
                },
            },
            _ = sleep_until_opt(stop_deadline) => {
                tracing::warn!(?pid, "codex: runtime did not exit after stop; killing its process group");
                if let Some(p) = pid {
                    kill_group(p);
                }
                let status = child.wait().await;
                return finish(status, pid, &end, &tail, tasks).await;
            }
        }
    }
}

/// Translates one stdout line. An oversized line is dropped (warned about); a
/// `turn/completed` one still ends the turn ([`Translator::oversized`]).
fn translate_line(tr: &mut Translator, line: Line) -> Translated {
    match line {
        Line::Text(l) => tr.translate(&l),
        Line::Oversized { len, head } => {
            tracing::debug!(len, head = %translate::one_line(&head, 200), "codex: dropping an oversized stdout line");
            tr.oversized(&head, len)
        }
    }
}

/// Queues `out.replies` and emits `out.events`. A fatal translation is recorded in the stderr
/// tail (so `thread.crashed` says why) and stops the session.
fn emit(stdin: &mut Stdin, end: &AdapterEnd, tail: &Arc<Mutex<Tail>>, out: Translated) -> After {
    for reply in out.replies {
        queue_line(stdin, reply);
    }
    let mut after = After::Continue;
    for e in out.events {
        let ev = AdapterEvent::Emit {
            body: e.body,
            raw: e.raw,
            turn: e.turn,
        };
        if end.events.send(ev).is_err() {
            tracing::warn!("codex: hub dropped the event stream; stopping");
            after = After::Stop;
        }
    }
    if let Some(why) = out.fatal {
        tracing::warn!(%why, "codex: session cannot go on; stopping");
        tail.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("twr: {why}\n").as_bytes());
        after = After::Stop;
    }
    after
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Queues an interrupt for a running turn and closes the queue: the writer sends what it has,
/// then closes stdin (Codex exits on EOF). Returns the kill deadline, which starts now whether
/// or not the runtime still reads its stdin.
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

/// Reports the exit. The process group was already killed by the caller.
async fn finish(
    status: std::io::Result<std::process::ExitStatus>,
    pid: Option<u32>,
    end: &AdapterEnd,
    tail: &Arc<Mutex<Tail>>,
    tasks: Tasks,
) {
    if let Some(w) = tasks.writer {
        w.abort();
    }
    if let Some(mut t) = tasks.stderr {
        if tokio::time::timeout(STDERR_DRAIN, &mut t).await.is_err() {
            t.abort();
        }
    }
    let code = match status {
        Ok(s) => s.code(),
        Err(e) => {
            tracing::warn!(error = %e, "codex: waiting for the runtime failed");
            None
        }
    };
    let text = tail.lock().unwrap_or_else(|e| e.into_inner()).text();
    tracing::info!(?pid, ?code, "codex: exited");
    end.exited(code, text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_map_to_policies() {
        let pol = |m: &str, b: Option<&[String]>| policy(m, b).unwrap();
        assert_eq!(pol("manual", None), ("untrusted", "workspace-write"));
        assert_eq!(pol("", None), ("untrusted", "workspace-write"));
        assert_eq!(pol("plan", None), ("untrusted", "read-only"));
        assert_eq!(pol("read-only", None), ("on-request", "read-only"));
        assert_eq!(pol("on-request", None), ("on-request", "workspace-write"));
        assert_eq!(pol("auto", None), ("on-request", "workspace-write"));
        // Never wider than the mode: edits alone cannot be auto-accepted, so everything asks.
        assert_eq!(pol("acceptEdits", None), ("untrusted", "workspace-write"));
        // Deny by default, like Claude's dontAsk: nothing asks and nothing can be written.
        assert_eq!(pol("dontAsk", None), ("never", "read-only"));
        assert_eq!(pol("never", None), ("never", "workspace-write"));
        assert_eq!(
            pol("bypassPermissions", None),
            ("never", "danger-full-access")
        );
        assert_eq!(pol("manual", Some(&[])), ("untrusted", "read-only"));
        assert_eq!(
            pol("manual", Some(&["shell".to_string()])),
            ("untrusted", "workspace-write")
        );
    }

    /// Other spellings of a mode map like it, other known modes map by
    /// their level (restricted → read-only sandbox), and an unknown mode is refused instead of
    /// being treated as `manual`.
    #[test]
    fn modes_map_by_level_and_unknown_modes_are_refused() {
        let pol = |m: &str| policy(m, None).unwrap();
        assert_eq!(pol("dont-ask"), ("never", "read-only"));
        assert_eq!(pol("Read_Only"), ("on-request", "read-only"));
        assert_eq!(pol("PLAN"), ("untrusted", "read-only"));
        assert_eq!(pol("readOnly"), ("on-request", "read-only"));
        assert_eq!(pol("ask"), ("untrusted", "workspace-write"));
        assert_eq!(pol("autoEdit"), ("untrusted", "workspace-write"));
        assert_eq!(pol("always-approve"), ("never", "danger-full-access"));
        for unknown in ["weird", "careful", "full"] {
            let e = policy(unknown, None).unwrap_err().to_string();
            assert!(e.contains("unknown permission mode"), "{e}");
            assert_eq!(mode_note(unknown, None), None);
            let cfg = SpawnConfig {
                mode: unknown.into(),
                ..Default::default()
            };
            assert!(thread_params(&cfg).is_err());
            assert!(CodexAdapter::with_bin("/bin/false")
                .attach_command(&cfg, "s")
                .is_none());
        }
    }

    #[test]
    fn inexact_modes_carry_a_note() {
        for exact in [
            "",
            "manual",
            "default",
            "untrusted",
            "read-only",
            "on-request",
            "never",
            "bypassPermissions",
            "yolo",
        ] {
            assert_eq!(mode_note(exact, None), None, "{exact}");
        }
        for (mode, needle) in [
            ("plan", "no plan mode"),
            ("auto", "on-request"),
            ("acceptEdits", "still ask"),
            ("dontAsk", "deny by default"),
        ] {
            let note = mode_note(mode, None).unwrap_or_else(|| panic!("{mode}: no note"));
            assert!(note.contains(needle), "{mode}: {note}");
            assert!(note.starts_with(&format!("mode {mode:?}")), "{note}");
        }
        assert_eq!(
            mode_note("manual", Some(&[])).as_deref(),
            Some("no built-in tools: sandbox read-only")
        );
        // Already read-only: nothing more to say.
        assert_eq!(mode_note("read-only", Some(&[])), None);
        assert!(mode_note("dontAsk", Some(&[]))
            .unwrap()
            .starts_with("mode \"dontAsk\""));
    }

    /// A main checkout at `<dir>/main` and its linked worktree at `<dir>/wt`, laid out the way
    /// `git worktree add` does (`gitdir` absolute unless `relative`).
    fn worktree(dir: &Path, relative: bool) -> (PathBuf, PathBuf) {
        let common = dir.join("main/.git");
        for d in ["objects", "refs/heads", "logs", "hooks", "worktrees/wt"] {
            std::fs::create_dir_all(common.join(d)).unwrap();
        }
        std::fs::write(common.join("config"), "").unwrap();
        std::fs::write(common.join("worktrees/wt/commondir"), "../..\n").unwrap();
        let wt = dir.join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        // git's back-pointer from the record to the worktree.
        std::fs::write(
            common.join("worktrees/wt/gitdir"),
            format!("{}\n", wt.join(".git").display()),
        )
        .unwrap();
        let target = if relative {
            "../main/.git/worktrees/wt".to_string()
        } else {
            common.join("worktrees/wt").display().to_string()
        };
        std::fs::write(wt.join(".git"), format!("gitdir: {target}\n")).unwrap();
        (wt, common.canonicalize().unwrap())
    }

    fn no_env(_: &str) -> Option<std::ffi::OsString> {
        None
    }

    /// The incident behind the `auto` sandbox: a Codex auto thread asked to `git commit`
    /// because its worktree's git metadata is in the main repository's `.git`. The worktree's
    /// git dir and the common objects, refs and logs become writable; config and hooks do not.
    #[test]
    fn auto_roots_cover_a_linked_worktrees_git_metadata() {
        for relative in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (wt, common) = worktree(dir.path(), relative);
            let main = dir.path().join("main");
            let repo = Some(main.as_path());
            let want: Vec<PathBuf> = ["worktrees/wt", "objects", "refs", "logs"]
                .iter()
                .map(|d| common.join(d))
                .collect();
            assert_eq!(
                auto_writable_roots(&wt, repo, no_env),
                want,
                "relative {relative}"
            );
            // From a subdirectory too, like git.
            assert_eq!(auto_writable_roots(&wt.join("src"), repo, no_env), want);
            // A repository without logs (core.logAllRefUpdates off) just has fewer roots.
            std::fs::remove_dir(common.join("logs")).unwrap();
            assert_eq!(auto_writable_roots(&wt, repo, no_env), want[..3]);
        }
    }

    /// The git dir of a main checkout or a submodule holds `config` and `hooks`: nothing of it
    /// is made writable, and a directory outside any repository gets no git roots.
    #[test]
    fn auto_roots_leave_a_main_checkout_and_a_submodule_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (_, common) = worktree(dir.path(), false);
        let main = dir.path().join("main");
        let repo = Some(main.as_path());
        assert!(auto_writable_roots(&main, repo, no_env).is_empty());
        let sub = dir.path().join("main/sub");
        std::fs::create_dir_all(common.join("modules/sub")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../.git/modules/sub\n").unwrap();
        assert!(auto_writable_roots(&sub, repo, no_env).is_empty());
        let plain = tempfile::tempdir().unwrap();
        assert!(auto_writable_roots(plain.path(), repo, no_env).is_empty());
        assert!(auto_writable_roots(plain.path(), None, no_env).is_empty());
    }

    /// The cargo caches (not `bin` or `config.toml`) and the npm cache, from their variables or
    /// `HOME`; only directories that exist and the owner can write, each once.
    #[test]
    fn auto_roots_cover_the_cargo_and_npm_caches() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();
        let home = d.join("home");
        for p in [
            ".cargo/registry",
            ".cargo/git",
            ".cargo/bin",
            ".npm",
            "cargo2/registry",
        ] {
            std::fs::create_dir_all(home.join(p)).unwrap();
        }
        let cwd = d.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let env = |vars: &'static [(&'static str, &'static str)]| {
            let home = home.clone();
            move |k: &str| match k {
                "HOME" => Some(home.clone().into_os_string()),
                _ => vars
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| home.join(v).into_os_string()),
            }
        };
        assert_eq!(
            auto_writable_roots(&cwd, None, env(&[])),
            [
                home.join(".cargo/registry"),
                home.join(".cargo/git"),
                home.join(".npm")
            ]
        );
        // CARGO_HOME wins (its git cache does not exist yet: skipped); so does the npm cache
        // variable, here pointing at the cargo registry: listed once.
        assert_eq!(
            auto_writable_roots(
                &cwd,
                None,
                env(&[
                    ("CARGO_HOME", "cargo2"),
                    ("npm_config_cache", "cargo2/registry")
                ])
            ),
            [home.join("cargo2/registry")]
        );
        // A cache the owner cannot write would make Codex fail every command: left out.
        let git = home.join(".cargo/git");
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!auto_writable_roots(&cwd, None, env(&[])).contains(&git));
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Empty or relative variables count as unset; a cwd that does not exist gets nothing.
        let blank = |k: &str| match k {
            "HOME" => Some(home.clone().into_os_string()),
            "CARGO_HOME" => Some("".into()),
            "npm_config_cache" => Some("relative/npm".into()),
            _ => None,
        };
        assert_eq!(auto_writable_roots(&cwd, None, blank).len(), 3);
        assert!(auto_writable_roots(&d.join("gone"), None, env(&[])).is_empty());
    }

    /// A worktree's own pointers are the agent's to rewrite (an `auto` thread can write its
    /// worktree and its git dir): repointed, they never make anything outside the
    /// repository's own git metadata writable. The audit's case: `commondir` rewritten to a
    /// directory of the git dir holding an `objects` link to the home directory.
    #[test]
    fn rewritten_worktree_pointers_grant_no_git_roots() {
        let dir = tempfile::tempdir().unwrap();
        let (wt, common) = worktree(dir.path(), false);
        let main = dir.path().join("main");
        let repo = Some(main.as_path());
        let git_dir = common.join("worktrees/wt");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let git_roots = |wt: &Path, repo: Option<&Path>| -> Vec<PathBuf> {
            auto_writable_roots(wt, repo, no_env)
        };
        assert_eq!(git_roots(&wt, repo).len(), 4);
        // Without the repository the thread was made from, nothing of git's.
        assert!(git_roots(&wt, None).is_empty());
        // `commondir` leading to a directory of the git dir with `objects` -> home.
        std::fs::create_dir_all(git_dir.join("control")).unwrap();
        std::os::unix::fs::symlink(&home, git_dir.join("control/objects")).unwrap();
        std::fs::write(git_dir.join("commondir"), "control\n").unwrap();
        assert!(git_roots(&wt, repo).is_empty());
        // ... or to home itself.
        std::fs::write(git_dir.join("commondir"), format!("{}\n", home.display())).unwrap();
        assert!(git_roots(&wt, repo).is_empty());
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        assert_eq!(git_roots(&wt, repo).len(), 4);
        // The worktree's `.git` naming a git dir of its own making, or another repository's
        // worktree record.
        let fake = wt.join("fake");
        std::fs::create_dir_all(fake.join("worktrees/x")).unwrap();
        std::os::unix::fs::symlink(&home, fake.join("objects")).unwrap();
        std::fs::write(fake.join("worktrees/x/commondir"), "../..\n").unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", fake.join("worktrees/x").display()),
        )
        .unwrap();
        assert!(git_roots(&wt, repo).is_empty());
        let other = tempfile::tempdir().unwrap();
        let (_, other_common) = worktree(other.path(), false);
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", other_common.join("worktrees/wt").display()),
        )
        .unwrap();
        assert!(git_roots(&wt, repo).is_empty());
        // A repository whose own `.git` lies in the worktree (the agent's) anchors nothing.
        assert!(git_roots(&wt, Some(&wt)).is_empty());
        // A common `objects` that is a link out of the common dir is left out.
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        std::fs::remove_dir(common.join("objects")).unwrap();
        std::os::unix::fs::symlink(&home, common.join("objects")).unwrap();
        let roots = git_roots(&wt, repo);
        assert_eq!(roots.len(), 3, "{roots:?}");
        assert!(!roots.contains(&home.canonicalize().unwrap()));
    }

    /// Two worktrees of one repository, made by git: a worktree's `.git` repointed at its
    /// sibling's record, or linked to its sibling's `.git`, grants writes to neither record,
    /// and a sibling whose agent points its own record at this worktree makes this one's
    /// record ambiguous (nothing granted) without gaining it.
    #[test]
    fn a_worktree_cannot_pick_its_siblings_record() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().canonicalize().unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "init.defaultBranch=main",
                ])
                .args(args)
                .current_dir(cwd)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        let main = d.join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main, &["worktree", "add", "-q", "-b", "a", "../a"]);
        git(&main, &["worktree", "add", "-q", "-b", "b", "../b"]);
        let common = main.join(".git");
        let (a, b) = (d.join("a"), d.join("b"));
        let repo = Some(main.as_path());
        let roots = |wt: &Path| auto_writable_roots(wt, repo, no_env);
        assert!(roots(&a).contains(&common.join("worktrees/a")));
        assert!(roots(&b).contains(&common.join("worktrees/b")));
        let original = std::fs::read_to_string(a.join(".git")).unwrap();
        // `.git` repointed at the sibling's record.
        std::fs::write(
            a.join(".git"),
            format!("gitdir: {}\n", common.join("worktrees/b").display()),
        )
        .unwrap();
        assert!(roots(&a).is_empty(), "{:?}", roots(&a));
        // `.git` linked to the sibling's `.git`.
        std::fs::remove_file(a.join(".git")).unwrap();
        std::os::unix::fs::symlink(b.join(".git"), a.join(".git")).unwrap();
        assert!(roots(&a).is_empty(), "{:?}", roots(&a));
        std::fs::remove_file(a.join(".git")).unwrap();
        std::fs::write(a.join(".git"), &original).unwrap();
        assert_eq!(roots(&a).len(), 4);
        // The sibling's agent points its own record at this worktree: neither is granted it.
        std::fs::write(
            common.join("worktrees/b/gitdir"),
            format!("{}\n", a.join(".git").display()),
        )
        .unwrap();
        assert!(roots(&a).is_empty(), "{:?}", roots(&a));
        assert!(roots(&b).is_empty(), "{:?}", roots(&b));
    }

    /// Every mode with a `workspace-write` sandbox gets both the network and the writable
    /// roots overrides, always (a user's Codex config would add its own otherwise): only
    /// `auto` with network and roots. No overrides for the other sandboxes, nor with no
    /// built-in tools (read-only then). Another spelling or level of `auto` is `auto`.
    /// `cfg.env` wins over twr's own.
    #[test]
    fn workspace_write_modes_pin_the_sandbox_extras() {
        let dir = tempfile::tempdir().unwrap();
        let (wt, common) = worktree(dir.path(), false);
        let cfg = |mode: &str| SpawnConfig {
            cwd: wt.clone(),
            mode: mode.into(),
            repo: Some(dir.path().join("main")),
            env: vec![
                ("HOME".into(), "/nonexistent-twr".into()),
                ("CARGO_HOME".into(), "".into()),
                ("npm_config_cache".into(), "".into()),
                ("NPM_CONFIG_CACHE".into(), "".into()),
            ],
            ..Default::default()
        };
        let m = sandbox_overrides(&cfg("auto"), true);
        assert_eq!(m[NETWORK_KEY], true);
        assert_eq!(
            m[WRITABLE_ROOTS_KEY],
            json!(["worktrees/wt", "objects", "refs", "logs"]
                .map(|d| common.join(d).display().to_string()))
        );
        assert_eq!(sandbox_overrides(&cfg("AUTO"), true), m);
        let pinned = json!({NETWORK_KEY: false, WRITABLE_ROOTS_KEY: []});
        for other in ["manual", "on-request", "acceptEdits", "never", ""] {
            assert_eq!(
                Value::Object(sandbox_overrides(&cfg(other), true)),
                pinned,
                "{other}"
            );
        }
        for other in [
            "plan",
            "read-only",
            "dontAsk",
            "bypassPermissions",
            "nonsense",
        ] {
            assert!(sandbox_overrides(&cfg(other), true).is_empty(), "{other}");
        }
        let orch = SpawnConfig {
            builtin_tools: Some(vec![]),
            ..cfg("auto")
        };
        assert!(sandbox_overrides(&orch, true).is_empty());
        // No roots at all, or a command for another box: still an empty list, so the user's
        // own roots do not stay.
        let bare = SpawnConfig {
            cwd: dir.path().join("gone"),
            ..cfg("auto")
        };
        let network_only = json!({NETWORK_KEY: true, WRITABLE_ROOTS_KEY: []});
        assert_eq!(Value::Object(sandbox_overrides(&bare, true)), network_only);
        assert_eq!(
            Value::Object(sandbox_overrides(&cfg("auto"), false)),
            network_only
        );
    }

    #[test]
    fn builtin_groups() {
        assert!(builtin_overrides(None).is_empty());
        let none = builtin_overrides(Some(&[]));
        assert_eq!(none["features.shell_tool"], false);
        assert_eq!(none["features.unified_exec"], false);
        assert_eq!(none["web_search"], "disabled");
        assert_eq!(none["features.multi_agent"], false);
        let shell = builtin_overrides(Some(&["Bash".to_string()]));
        assert!(!shell.contains_key("features.shell_tool"));
        assert_eq!(shell["web_search"], "disabled");
    }

    #[test]
    fn exe_is_not_reported_for_scripts() {
        let dir = std::env::temp_dir().join(format!("twr-codex-exe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let binary = dir.join("binary");
        std::fs::write(&binary, [0x7f, b'E', b'L', b'F']).unwrap();
        assert_eq!(exe_of(&script, Some(&script)), None);
        assert_eq!(
            exe_of(&binary, Some(&binary)),
            Some(binary.to_string_lossy().into_owned())
        );
        // A bare name is recorded as named, once it resolved to a binary.
        assert_eq!(
            exe_of(Path::new("codex"), Some(&binary)),
            Some("codex".to_string())
        );
        assert_eq!(exe_of(Path::new("/nonexistent/codex"), None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

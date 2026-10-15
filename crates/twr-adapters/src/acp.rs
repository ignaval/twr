//! Generic Agent Client Protocol (ACP) runtime: any agent CLI that speaks ACP over stdio
//! (Gemini CLI `gemini --experimental-acp`, opencode `opencode acp`, goose `goose acp`,
//! qwen-code, Claude through `claude-code-acp`, Grok's `grok agent stdio`, ...) as a worker
//! thread or as the orchestrator. Tier: headless.
//!
//! Runtime spec: `acp:<command line>`, exactly like `pty:<command line>`: the
//! hub splits the command in-tree ([`crate::pty::split_command_line`]: shell-style quoting, no
//! expansion), records the argv as `thread.created` `ext.twr.acp_command` and passes it to
//! this adapter as [`COMMAND_ENV`] (a JSON array) on every spawn and resume. Named presets in
//! the config (`[runtimes.<name>] acp = "<command>"`, optional `attach = "<command with
//! {session}>"`) resolve to the same thing when the thread is created. A bare `acp` is
//! refused.
//!
//! Protocol ([`translate`] has the full mapping): `initialize`, `session/new{cwd,
//! mcpServers}` (or `session/resume` / `session/load` when the agent advertises it),
//! `session/prompt`, `session/update` (message and thought chunks, tool calls, mode updates),
//! `session/request_permission` (answered with the first `allow_once` / `reject_once` option,
//! never `allow_always` / `reject_always`), `session/cancel`, and the standard stop reasons.
//! No `_meta` is sent and no vendor extension is used ([`Standard`]): Grok's layer on top of
//! the same translator is [`crate::grok_build::acp`].
//!
//! What twr can and cannot enforce on a generic ACP agent:
//! - Relay rule: every `session/request_permission` becomes an attention item the user
//!   answers; twr never selects an `*_always` option. What the agent runs *without* asking is
//!   the agent's own business (its configuration, approval mode or safe-command list): twr
//!   cannot see it. (Verified with grok 1.0.44 through this layer, 2026-09-30: a file write
//!   asks, while `touch x` runs unasked, as it does under the grok-build runtime too.)
//! - Modes: twr passes nothing (the agent's default mode) unless the agent reports modes
//!   (`modes`, or `configOptions` entries of category `mode`). Then every reported mode is
//!   checked against the thread's ([`translate::mode_acceptable`]): exactly the thread's
//!   mode, or a known mode id (explicit tables, whole ids only) no wider than it; an id twr
//!   does not know only when it is exactly the thread's mode. A report twr does not accept is
//!   switched through its own source to the thread's mode when offered, else to the widest
//!   offered known mode that is accepted, and the session fails closed (the running prompt is
//!   cancelled, the session ends, no prompt is sent) when there is none or the switch fails
//!   or does not take. A thread whose mode is the agent's own mode id
//!   (e.g. `--mode autoEdit`) is switched to exactly that mode. Mode updates the agent sends
//!   while the session opens count too (only replayed history is dropped). An agent that
//!   reports no mode at all cannot be held to a restricted thread mode (`plan`, `dontAsk`,
//!   `read-only`): that session fails; with any other thread mode it runs with
//!   `thread.started` `ext.acp.mode_unenforced = true` (the agent's own default applies).
//! - Session ids: an id outside `[A-Za-z0-9._:-]` (1 to 256 characters, not starting with
//!   `-`) fails the session when it opens ([`translate::valid_session_id`]): it would be
//!   inserted unquoted into an attach command line.
//! - Model: `SpawnConfig::model` is set with `session/set_config_option` (the `configOptions`
//!   model option) or `session/set_model` (`models`); an agent that reports neither fails the
//!   session rather than run another model (give the model in the command line instead).
//! - Reasoning effort: `SpawnConfig::effort` is set with `session/set_config_option` (the
//!   `configOptions` entry of category `thought_level`) after the model, as the agent's own
//!   value (the hub keeps it as given); an agent that reports no such option, or rejects the
//!   value, fails the session.
//! - System prompt: ACP has no field for one. `SpawnConfig::system_append` (the
//!   orchestrator's system prompt) is prepended to the FIRST prompt of a new session, between
//!   [`SYSTEM_OPEN`] and [`SYSTEM_CLOSE`] lines; twr's tags inside the prompt itself are
//!   defused ([`defuse_tags`]). A resumed session does not get it again (the agent keeps its
//!   history).
//! - Tools: ACP has no standard way to restrict an agent's built-in tools.
//!   `SpawnConfig::builtin_tools` / `allowed_tools` are NOT enforced (noted in
//!   `thread.started` `ext.acp.tool_restriction`); a generic ACP orchestrator keeps its shell,
//!   file and web tools, and the calls it asks about (twr's MCP tools included) are
//!   permission prompts to the user, so the relay rule still holds for them. Where the agent
//!   has its own restriction (a flag or config of its CLI), put it in the runtime's command
//!   line.
//! - MCP servers: `SpawnConfig::mcp_config(_file)` becomes the standard `mcpServers` of
//!   `session/new|load|resume` ([`acp_mcp_servers`]), sent over stdin so a token never shows
//!   on a command line.
//! - Authentication: twr never calls ACP `authenticate`; sign in with the agent's own CLI
//!   first. An "Authentication required" error fails the session with that hint.
//! - `attach`: none, unless the preset gives `attach = "<command with {session}>"` (passed as
//!   [`ATTACH_ENV`]); twr cannot know an arbitrary agent's interactive CLI. `{mode}` in the
//!   template is replaced by the thread's mode; a thread in a restricted mode cannot attach
//!   through a template without `{mode}` (the CLI would run in its own default mode).
//!   `{model}` and `{effort}` are replaced by the thread's model and effort, and an argument
//!   holding one is left out when the thread has none. Without them the attach cannot apply a
//!   model or effort the session has not run with yet ([`template_applies`]: the hub refuses
//!   that attach). See [`attach_argv`].
//!
//! The process runs the command in `cwd` with `cfg.env` (minus [`COMMAND_ENV`] /
//! [`ATTACH_ENV`]), in its own process group, kill-on-drop, twr's secrets removed; see
//! [`driver`].

pub mod driver;
pub mod translate;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::{json, Map, Value};
use twr_core::adapter::{Adapter, AdapterHandle, AdapterInfo, AttachApplies, SpawnConfig, Tier};

pub use translate::{Dialect, Emitted, Options, Translated, Translator};

use crate::process;

/// Adapter name (`source = adapter:acp`, the runtime of `acp:<command>` threads).
pub const NAME: &str = "acp";

/// Prefix of a runtime spec that names the acp runtime with its command (`acp:<command>`).
pub const RUNTIME_PREFIX: &str = "acp:";

/// Environment entry carrying the agent's argv (a JSON array of strings).
pub const COMMAND_ENV: &str = "TWR_ACP_COMMAND";

/// Environment entry carrying the attach command template (a JSON array of strings, where
/// `{session}` stands for the session id, `{mode}` for the thread's permission mode, and
/// `{model}` / `{effort}` for its model and reasoning effort).
pub const ATTACH_ENV: &str = "TWR_ACP_ATTACH";

/// Placeholder of the session id in an attach command.
pub const SESSION_PLACEHOLDER: &str = "{session}";

/// Placeholder of the thread's permission mode in an attach command.
pub const MODE_PLACEHOLDER: &str = "{mode}";

/// Placeholder of the thread's model in an attach command.
pub const MODEL_PLACEHOLDER: &str = "{model}";

/// Placeholder of the thread's reasoning effort in an attach command.
pub const EFFORT_PLACEHOLDER: &str = "{effort}";

/// Key of this runtime's extras (`data.ext.acp`).
pub const EXT: &str = "acp";

/// How long `stop` waits for the process to exit after closing stdin.
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// First line of the system prompt twr prepends to a new session's first prompt.
pub const SYSTEM_OPEN: &str = "<twr-system-instructions>";

/// Last line of that system prompt.
pub const SYSTEM_CLOSE: &str = "</twr-system-instructions>";

/// Longest command line of an `acp:<command>` runtime (or preset), in bytes.
pub const COMMAND_MAX: usize = 4096;

/// The generic ACP runtime adapter.
#[derive(Debug, Clone)]
pub struct AcpAdapter {
    command: Option<Vec<String>>,
    stop_grace: Duration,
    max_line: usize,
}

impl Default for AcpAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpAdapter {
    /// Runs the command each thread names ([`COMMAND_ENV`]).
    pub fn new() -> Self {
        AcpAdapter {
            command: None,
            stop_grace: STOP_GRACE,
            max_line: process::MAX_LINE,
        }
    }

    /// Always runs `argv` (tests).
    pub fn with_command<S: Into<String>>(argv: impl IntoIterator<Item = S>) -> Self {
        AcpAdapter {
            command: Some(argv.into_iter().map(Into::into).collect()),
            ..Self::new()
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

    /// The argv to run for `cfg`.
    pub fn command_for(&self, cfg: &SpawnConfig) -> anyhow::Result<Vec<String>> {
        if let Some(c) = &self.command {
            if c.first().is_none_or(|p| p.is_empty()) {
                bail!("the acp command is empty");
            }
            return Ok(c.clone());
        }
        let Some(raw) = env_of(cfg, COMMAND_ENV) else {
            bail!(
                "the acp runtime needs the agent's command: set {COMMAND_ENV} (a JSON argv \
                 array), e.g. via `--runtime 'acp:<command line>'` or a `[runtimes.<name>] acp` \
                 preset"
            );
        };
        parse_argv(raw, COMMAND_ENV)
    }
}

/// A JSON argv array from environment entry `key`.
pub fn parse_argv(raw: &str, key: &str) -> anyhow::Result<Vec<String>> {
    let argv: Vec<String> = serde_json::from_str(raw.trim())
        .with_context(|| format!("parsing {key} as a JSON array of strings"))?;
    if argv.first().is_none_or(|p| p.is_empty()) {
        bail!("{key} names no program");
    }
    Ok(argv)
}

fn env_of<'a>(cfg: &'a SpawnConfig, key: &str) -> Option<&'a str> {
    cfg.env
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Environment for the agent: `cfg.env` without twr's `TWR_ACP_*` entries.
pub fn program_env(cfg: &SpawnConfig) -> Vec<(String, String)> {
    cfg.env
        .iter()
        .filter(|(k, _)| k != COMMAND_ENV && k != ATTACH_ENV)
        .cloned()
        .collect()
}

/// The agent's name in messages: the program's file name.
pub fn agent_name(argv: &[String]) -> String {
    argv.first()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
        })
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "the ACP agent".into())
}

/// The attach command for `session_id` of a thread configured as `cfg` from template `argv`:
/// [`SESSION_PLACEHOLDER`], [`MODE_PLACEHOLDER`], [`MODEL_PLACEHOLDER`] and
/// [`EFFORT_PLACEHOLDER`] replaced in every argument (the mode verbatim, `default` when empty;
/// all are inserted unquoted into single arguments, never through a shell). An argument holding
/// `{model}` (`{effort}`) is left out when the thread has no model (effort): the session keeps
/// its own. None (no attach) when:
/// - no argument holds `{session}`;
/// - the session id is not one twr accepts ([`translate::valid_session_id`]);
/// - the mode is a restricted one ([`translate::ModeLevel::Restricted`]) and no argument holds
///   `{mode}`: the interactive CLI would run in its own default mode, wider than the thread's;
/// - `{mode}` is used and the mode is not a plain id (the same characters as a session id);
/// - `{model}` or `{effort}` is a whole argument ([`template_error`]), or its value has
///   whitespace, control characters or braces (it could hold another placeholder).
pub fn attach_argv(
    template: &[String],
    session_id: &str,
    cfg: &SpawnConfig,
) -> Option<Vec<String>> {
    if !template.iter().any(|a| a.contains(SESSION_PLACEHOLDER)) {
        return None;
    }
    if let Some(e) = template_error(template) {
        tracing::warn!("acp: refusing to attach: {e}");
        return None;
    }
    if !translate::valid_session_id(session_id) {
        tracing::warn!(
            session_id,
            "acp: refusing to attach to a session id twr does not accept"
        );
        return None;
    }
    let mode = match cfg.mode.trim() {
        "" => "default",
        m => m,
    };
    let has_mode = template.iter().any(|a| a.contains(MODE_PLACEHOLDER));
    if !has_mode && translate::mode_level(mode) == Some(translate::ModeLevel::Restricted) {
        tracing::warn!(
            mode,
            "acp: refusing to attach: the attach template has no {MODE_PLACEHOLDER} for the \
             thread's restricted mode"
        );
        return None;
    }
    if has_mode && !translate::valid_session_id(mode) {
        tracing::warn!(
            mode,
            "acp: refusing to put this mode into an attach command"
        );
        return None;
    }
    let model = setting(&cfg.model);
    let effort = setting(&cfg.effort);
    for (placeholder, value) in [(MODEL_PLACEHOLDER, model), (EFFORT_PLACEHOLDER, effort)] {
        let used = template.iter().any(|a| a.contains(placeholder));
        let plain = |v: &str| {
            !v.chars()
                .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '{' | '}'))
        };
        if used && value.is_some_and(|v| !plain(v)) {
            tracing::warn!(
                ?value,
                "acp: refusing to put this {placeholder} into an attach command"
            );
            return None;
        }
    }
    Some(
        template
            .iter()
            .filter(|a| model.is_some() || !a.contains(MODEL_PLACEHOLDER))
            .filter(|a| effort.is_some() || !a.contains(EFFORT_PLACEHOLDER))
            .map(|a| {
                a.replace(SESSION_PLACEHOLDER, session_id)
                    .replace(MODE_PLACEHOLDER, mode)
                    .replace(MODEL_PLACEHOLDER, model.unwrap_or_default())
                    .replace(EFFORT_PLACEHOLDER, effort.unwrap_or_default())
            })
            .collect(),
    )
}

/// A model or effort setting, trimmed; `None` when it is the runtime's default.
fn setting(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// What is wrong with attach template `argv` beyond a missing `{session}`: `{model}` or
/// `{effort}` as a whole argument (`-m {model}`), or in one argument with any other
/// placeholder (`--settings=mode:{mode},effort:{effort}`). An argument holding one is left
/// out for a thread without that setting: alone it would leave its flag behind, and with
/// another placeholder it would drop that one too (the permission mode, or the other
/// setting, which [`template_applies`] and the hub's restricted-mode check count as passed).
/// So the flag and the placeholder go in one argument of their own (`--model={model}`).
pub fn template_error(argv: &[String]) -> Option<String> {
    const ALL: [&str; 4] = [
        SESSION_PLACEHOLDER,
        MODE_PLACEHOLDER,
        MODEL_PLACEHOLDER,
        EFFORT_PLACEHOLDER,
    ];
    for p in [MODEL_PLACEHOLDER, EFFORT_PLACEHOLDER] {
        for a in argv.iter().filter(|a| a.contains(p)) {
            if a == p {
                return Some(format!(
                    "the attach template has {p} as a whole argument; put it in one argument \
                     with its flag (`--flag={p}`): twr leaves that argument out for a thread \
                     without the setting"
                ));
            }
            if let Some(other) = ALL.iter().find(|o| **o != p && a.contains(**o)) {
                return Some(format!(
                    "the attach template has {p} and {other} in one argument ({a:?}); give {p} \
                     an argument of its own (`--flag={p}`): twr leaves that argument out for a \
                     thread without the setting, which would drop {other} too"
                ));
            }
        }
    }
    None
}

/// Which of `cfg`'s settings an attach through `template` runs the session with: a setting
/// the thread has whose placeholder ([`MODEL_PLACEHOLDER`], [`EFFORT_PLACEHOLDER`]) the
/// template holds. One left to the runtime's default is left out of the command, and an ACP
/// session keeps its own on resume (`crate::resume_keeps`).
pub fn template_applies(template: &[String], cfg: &SpawnConfig) -> AttachApplies {
    let has = |p: &str| template.iter().any(|a| a.contains(p));
    AttachApplies {
        model: has(MODEL_PLACEHOLDER) && setting(&cfg.model).is_some(),
        effort: has(EFFORT_PLACEHOLDER) && setting(&cfg.effort).is_some(),
    }
}

/// Tags twr wraps text in for an agent (its system prompt, a project's standing context):
/// [`defuse_tags`] neutralizes them in untrusted text.
pub const TWR_TAGS: &[&str] = &["twr-system-instructions", "twr-context"];

/// `text` with every opening or closing [`TWR_TAGS`] tag (`<twr-context`, `</TWR-Context`,
/// any case) defused: the `-` after `twr` becomes `_`, so untrusted text can neither close
/// twr's block early nor open one that looks like twr's.
pub fn defuse_tags(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            let after = if lower[i + 1..].starts_with('/') {
                i + 2
            } else {
                i + 1
            };
            if let Some(tag) = TWR_TAGS.iter().find(|t| lower[after..].starts_with(*t)) {
                out.extend_from_slice(&bytes[i..after + 3]);
                out.push(b'_');
                out.extend_from_slice(&bytes[after + 4..after + tag.len()]);
                i = after + tag.len();
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("only ASCII bytes were replaced")
}

/// The first prompt of a new session with `system` prepended ([`SYSTEM_OPEN`] ...
/// [`SYSTEM_CLOSE`]). The prompt is untrusted text (the user's, or a handoff built from other
/// agents' output): its twr tags are defused ([`defuse_tags`]) so it cannot close the system
/// block or pose as twr's.
pub fn with_system_prompt(system: &str, prompt: &str) -> String {
    format!(
        "{SYSTEM_OPEN}\n{system}\n{SYSTEM_CLOSE}\n\n{}",
        defuse_tags(prompt)
    )
}

/// ACP `mcpServers` from an MCP configuration: Claude's `{"mcpServers": {name: server}}` (a
/// stdio server `{command, args, env: {K: V}}`, or `{type: http|sse, url, headers: {K: V}}`),
/// or already an ACP list (passed through). Entries are sorted by name.
pub fn acp_mcp_servers(config: &Value) -> Vec<Value> {
    if let Some(list) = config.as_array() {
        return list.clone();
    }
    let servers = config.get("mcpServers").unwrap_or(config);
    if let Some(list) = servers.as_array() {
        return list.clone();
    }
    let Some(map) = servers.as_object() else {
        return Vec::new();
    };
    let pairs = |v: Option<&Value>| -> Vec<Value> {
        v.and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| {
                        let value = v.as_str().map_or_else(|| v.to_string(), str::to_string);
                        json!({"name": k, "value": value})
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut names: Vec<&String> = map.keys().collect();
    names.sort();
    names
        .into_iter()
        .map(|name| {
            let s = &map[name];
            match s.get("type").and_then(Value::as_str) {
                Some(t @ ("http" | "sse")) => json!({
                    "type": t,
                    "name": name,
                    "url": s.get("url").and_then(Value::as_str).unwrap_or_default(),
                    "headers": pairs(s.get("headers")),
                }),
                _ => json!({
                    "name": name,
                    "command": s.get("command").and_then(Value::as_str).unwrap_or_default(),
                    "args": s.get("args").cloned().filter(Value::is_array).unwrap_or_else(|| json!([])),
                    "env": pairs(s.get("env")),
                }),
            }
        })
        .collect()
}

/// The ACP `mcpServers` for `cfg` (reads `mcp_config_file` when set).
pub fn mcp_servers_of(cfg: &SpawnConfig) -> anyhow::Result<Vec<Value>> {
    let config = match (&cfg.mcp_config_file, &cfg.mcp_config) {
        (Some(file), _) => {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading MCP config {}", file.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("parsing MCP config {}", file.display()))?
        }
        (None, Some(v)) => v.clone(),
        (None, None) => return Ok(Vec::new()),
    };
    Ok(acp_mcp_servers(&config))
}

/// The standard ACP dialect: no `_meta`, no vendor requests or notifications.
#[derive(Debug, Clone, Default)]
pub struct Standard {
    name: String,
    command: Vec<String>,
    /// System prompt prepended to a new session's first prompt.
    system: Option<String>,
    /// Asked-for tool restrictions twr cannot enforce (`ext.acp.tool_restriction`).
    unenforced: Option<String>,
    /// `initialize`'s `agentInfo` and advertised auth method ids.
    agent_info: Option<Value>,
    auth_methods: Vec<String>,
}

impl Standard {
    /// The dialect for an agent started as `command`, with the system prompt `system` (for
    /// a new session's first prompt) and a note on tool restrictions twr cannot enforce.
    pub fn new(command: Vec<String>, system: Option<String>, unenforced: Option<String>) -> Self {
        Standard {
            name: agent_name(&command),
            command,
            system: system.filter(|s| !s.trim().is_empty()),
            unenforced,
            agent_info: None,
            auth_methods: Vec::new(),
        }
    }
}

impl Dialect for Standard {
    fn ext_key(&self) -> &'static str {
        EXT
    }

    fn log_name(&self) -> &'static str {
        NAME
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn on_initialized(&mut self, r: &Value) {
        self.agent_info = r.get("agentInfo").filter(|i| i.is_object()).cloned();
        self.auth_methods = r
            .get("authMethods")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
    }

    fn ready_ext(&self, ext: &mut Map<String, Value>) {
        ext.insert("command".into(), json!(self.command));
        if let Some(i) = &self.agent_info {
            ext.insert("agent".into(), i.clone());
        }
        if !self.auth_methods.is_empty() {
            ext.insert("auth_methods".into(), json!(self.auth_methods));
        }
        if let Some(u) = &self.unenforced {
            ext.insert("tool_restriction".into(), json!(u));
        }
    }

    fn auth_hint(&self) -> &str {
        " (sign in with the agent's own CLI first: twr does not run ACP `authenticate`)"
    }

    fn first_prompt(&self, text: &str) -> Option<String> {
        self.system.as_deref().map(|s| with_system_prompt(s, text))
    }
}

/// The note on the tool restrictions `cfg` asks for that no ACP agent can be made to honor.
pub fn unenforced_restrictions(cfg: &SpawnConfig) -> Option<String> {
    let mut parts = Vec::new();
    match &cfg.builtin_tools {
        Some(t) if t.is_empty() => parts.push("no built-in tools".to_string()),
        Some(t) => parts.push(format!("only the built-in tools {t:?}")),
        None => {}
    }
    if !cfg.allowed_tools.is_empty() {
        parts.push(format!("allow rules {:?}", cfg.allowed_tools));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "not enforced: twr asked for {}, which ACP cannot express; tool calls ask the user \
             unless the agent's own configuration lets them run",
            parts.join(" and ")
        )
    })
}

/// What to do when an ACP agent's program is not there ([`process::start_error`]).
const MISSING_FIX: &str =
    "install the agent, or fix the runtime's command (`acp:<command>` or its `[runtimes]` preset)";

impl Adapter for AcpAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Headless,
            description:
                "any Agent Client Protocol agent (`acp:<command>` or a `[runtimes]` preset)",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let argv = self.command_for(&cfg)?;
        let mcp_servers = mcp_servers_of(&cfg)?;
        let cwd = driver::absolute_cwd(&cfg.cwd)?;
        let program = PathBuf::from(&argv[0]);
        let child = driver::launch(
            &cfg.wrap,
            &program,
            &argv[1..],
            &program_env(&cfg),
            &cwd,
            MISSING_FIX,
        )?;
        let unenforced = unenforced_restrictions(&cfg);
        if let Some(u) = &unenforced {
            tracing::warn!(thread = %cfg.thread, "acp: tool restriction {u}");
        }
        let dialect = Standard::new(argv.clone(), cfg.system_append.clone(), unenforced);
        let translator = Translator::with_dialect(
            Options {
                thinking: cfg.thinking,
                pid: child.id(),
                exe: Some(argv[0].clone()),
                cwd: cwd.to_string_lossy().into_owned(),
                mcp_servers,
                resume_session: cfg.resume_session.clone(),
                mode: cfg.mode.trim().to_string(),
                model: cfg.model.clone().filter(|m| !m.trim().is_empty()),
                effort: cfg.effort.clone().filter(|e| !e.trim().is_empty()),
            },
            dialect,
        );
        tracing::info!(
            thread = %cfg.thread,
            pid = ?child.id(),
            ?argv,
            resume = ?cfg.resume_session,
            "acp: started"
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

    /// The preset's `attach` command ([`ATTACH_ENV`], its placeholders replaced, see
    /// [`attach_argv`]) in the thread's directory; None without one (twr cannot know an
    /// arbitrary agent's interactive CLI), or when it cannot carry a restricted thread mode.
    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let template = parse_argv(env_of(cfg, ATTACH_ENV)?, ATTACH_ENV).ok()?;
        let argv = attach_argv(&template, session_id, cfg)?;
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).envs(process::attach_env(cfg));
        process::scrub_std(&mut cmd);
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        Some(cmd)
    }

    /// What the preset's attach template carries ([`template_applies`]); neither without one.
    fn attach_applies(&self, cfg: &SpawnConfig) -> AttachApplies {
        env_of(cfg, ATTACH_ENV)
            .and_then(|raw| parse_argv(raw, ATTACH_ENV).ok())
            .map(|t| template_applies(&t, cfg))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_attach_templates() {
        assert_eq!(agent_name(&["/usr/bin/gemini".into()]), "gemini");
        assert_eq!(agent_name(&[]), "the ACP agent");
        let t: Vec<String> = ["gemini", "--resume={session}", "x"]
            .map(String::from)
            .to_vec();
        let manual = SpawnConfig {
            mode: "manual".into(),
            ..Default::default()
        };
        assert_eq!(
            attach_argv(&t, "S1", &manual).unwrap(),
            ["gemini", "--resume=S1", "x"]
        );
        assert!(attach_argv(&["gemini".into()], "S1", &manual).is_none());
    }

    #[test]
    fn restrictions_twr_cannot_enforce_are_named() {
        assert!(unenforced_restrictions(&SpawnConfig::default()).is_none());
        let cfg = SpawnConfig {
            builtin_tools: Some(Vec::new()),
            allowed_tools: vec!["mcp__twr".into()],
            ..Default::default()
        };
        let note = unenforced_restrictions(&cfg).unwrap();
        assert!(
            note.contains("no built-in tools") && note.contains("mcp__twr"),
            "{note}"
        );
    }

    #[test]
    fn the_command_comes_from_the_env() {
        let a = AcpAdapter::new();
        assert!(a.command_for(&SpawnConfig::default()).is_err());
        let cfg = SpawnConfig {
            env: vec![(
                COMMAND_ENV.into(),
                r#"["gemini","--experimental-acp"]"#.into(),
            )],
            ..Default::default()
        };
        assert_eq!(
            a.command_for(&cfg).unwrap(),
            ["gemini", "--experimental-acp"]
        );
        assert!(program_env(&cfg).is_empty());
        let bad = SpawnConfig {
            env: vec![(COMMAND_ENV.into(), "[]".into())],
            ..Default::default()
        };
        assert!(a.command_for(&bad).is_err());
    }
}

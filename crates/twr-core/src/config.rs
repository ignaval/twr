//! `config.toml`, and `boxes.toml` next to it ([`BoxesFile`]: the boxes `twr box add` wrote,
//! merged into `[boxes]`). Every field has a default; a missing file means all defaults.
//!
//! ```toml
//! default_runtime = "claude-code"
//! stall_after_secs = 600  # a running turn silent this long raises `stalled`; 0 = off
//! wake_after_secs = 30    # wake a worker idle this long after its background work; 0 = off
//! # Where `thread.new` looks for a project given by its folder name (`<root>/<name>` or
//! # `<root>/*/<name>`, a git repository); unset = DEFAULT_PROJECT_ROOTS, [] = no lookup.
//! project_roots = ["~/code", "~/src"]
//!
//! [orchestrator]
//! runtime = "claude-code"   # defaults to default_runtime; the first orchestrator's runtime
//!                           # (`twr orchestrator use` switches it; the switch is kept in the log)
//! cwd = "~"
//! autostart = false
//! model = "sonnet"          # unset = threads.model, else the runtime's default
//! account = "work"          # the first orchestrator's account; unset = the policy's default
//!
//! [hub]
//! socket = "/run/user/1000/twr/hub.sock"   # defaults to the resolved path
//! listen = "127.0.0.1:7878"                # runner WebSocket listener; off when unset
//! # A non-loopback `listen` is refused unless one of these is set:
//! tls = { cert = "/etc/twr/hub.pem", key = "/etc/twr/hub.key" }   # serve wss:// (PEM files)
//! allow_insecure_remote = false            # plain ws:// on the network (events in clear text)
//!
//! # A box the hub reaches over ssh: it runs twr there itself (`twr box add gpu me@gpu.lan`
//! # writes this table to boxes.toml).
//! [boxes.gpu]
//! transport = "ssh"             # unset = "token"
//! ssh = "me@gpu.lan"            # the hub's `ssh` destination; `twr attach` runs `ssh -t <ssh> --
//!                               # <command>` for its threads
//! build = "0123456789abcdef"    # the twr build to run there, when the box's OS or CPU is not the
//!                               # hub's (`twr box install` writes it); unset = the hub's own
//!
//! [boxes.gpu.projects.api]      # where project `api` lives on that box
//! repo = "/srv/src/api"
//!
//! # A box the hub reaches through a command instead of ssh (a container): it runs
//! # `<command> sh -c <what ssh would run>` itself, never through a shell on this side.
//! [boxes.sandbox]
//! transport = "command"
//! command = ["docker", "exec", "-i", "twr-sandbox"]
//! # Optional: what `twr box login` and `twr box project --clone` run in your terminal.
//! terminal_command = ["docker", "exec", "-i", "-t", "twr-sandbox"]
//!
//! # A box whose runner connects to the hub's listener (`[hub] listen`) with a token: a
//! # dial-in box. `twr box add lab --dial-in --hub <url>` writes one to boxes.toml, with a new
//! # token, and `twr runner install-token lab` stores the token on the box.
//! [boxes.lab]
//! token = "..."                 # the runner's token (`--token-file` / TWR_RUNNER_TOKEN); never logged
//!                               # or sent (16 characters at least; `twr runner token` prints one)
//! ssh = "me@lab.lan"            # optional: `twr attach` runs `ssh -t <ssh> -- <command>`
//!
//! [projects.api]
//! repo = "/home/me/src/api"
//! worktree = false     # overrides threads.worktree for this project
//! # Added to the environment of this project's runtimes, over threads.env.
//! env = { CARGO_TARGET_DIR = "/home/me/.cache/api-target" }
//! codex_auto = "ask"   # overrides [codex] auto for this project's threads
//! # Its threads' account on every runtime: a name, a glob ("work-*") or a list (a pool).
//! # A runtime with no usable account it matches refuses the thread.
//! account = "work"
//! # accounts = { codex = "team" }       # per runtime; replaces `account` there
//! # exclude_accounts = ["personal"]     # never these, on any runtime
//!
//! [codex]
//! # What a Codex thread in `auto` does with a command its sandbox does not allow: let Codex's
//! # "auto_review" agent decide (the default: it runs what it approves outside the sandbox,
//! # without asking you), "ask" you, or "never" run it.
//! auto = "auto_review"
//!
//! # Accounts: more logins of a runtime (claude-code, codex, grok-build), each a config
//! # directory (`twr account add` records one instead of declaring it here).
//! [accounts.claude-code.work]
//! dir = "~/.claude-work"    # unset = <data dir>/accounts/claude-code/work
//! share = "config"          # none | config (default): settings, skills... linked from the native
//!
//! [accounts.claude-code.personal]
//! native = true             # names the native account (the runtime's own directory, no
//!                           # variable set; "default" until named)
//!
//! [account_policy.claude-code]
//! default = "personal"      # the account of a thread nothing pins; unset = the native account
//!
//! [threads]
//! thinking = false
//! default_mode = "auto"     # the agent's own auto mode decides what asks; twr never answers
//! model = "opus"       # default model for new threads; unset = the runtime's default
//! # Ignored paths parking may delete with a worktree (`dir/` = any directory component,
//! # `name` = any component); other ignored files keep the worktree.
//! park_disposable = ["target/", "node_modules/", "dist/", "build/"]
//! # Of those, what an idle trim (`[projects.<name>] trim_idle_after`) may delete: an entry
//! # must match both lists.
//! idle_trim_disposable = ["target/", "node_modules/"]
//! worktree = true      # a twr-managed worktree per thread; false = work in the repo checkout
//! env = { CARGO_INCREMENTAL = "0" }   # added to every worker thread's runtime environment
//! # A worker's prompt cache expired with a large context: "ask" (default) before a message
//! # goes to it whether to compact it first; "before_expiry" also compacts an idle one just
//! # before an hour-long cache expires; "off": neither.
//! idle_compaction = "ask"
//!
//! [trash]
//! keep_days = 7        # deleted threads stay in the trash this long (`twr trash empty` sooner)
//!
//! [log]
//! retention_days = 30  # agent events of turns older than this move out of twr.db (7 at least)
//! archive = true       # into archive.db beside it; false: they are deleted
//!
//! [tui]
//! mouse = true         # the TUI selects and copies text itself; false = the terminal's own
//! animate = true       # the working icon moves; false = a still `twr_` marker
//! card_order = "stable"  # stable (default): cards keep their place; "attention": most recent first
//! suggest = true       # a small model suggests your next message (ghost text, Tab takes it);
//!                      # false: none (sends nothing to Anthropic for it)
//! # The small model suggestions come from, per runtime (a string: Claude Code's). Only a
//! # Haiku for Claude Code, a Luna (mini, nano) for Codex: a large one is never used.
//! suggest_model = { claude-code = "claude-haiku-5-5", codex = "gpt-6-luna" }
//! summaries = true     # the same small model writes one live line per active thread in the
//!                      # conversation; false: routine updates fold without a model
//! summary_every = "5m" # at most one summary per thread this often (and none when nothing changed)
//! colors = "auto"      # auto (default) | 256 | 16 | none: the colors the TUI draws with
//!
//! [tui.notify]
//! terminal = "bell"    # off (default) | bell | osc9 | osc777: how the TUI alerts its terminal
//! when = "unfocused"   # unfocused (default) | always
//! events = ["permission", "question", "confirm", "error", "review", "report", "reply"]
//!                      # the default; "stalled" too on request
//!
//! # Named runtimes: any Agent Client Protocol agent under a short name (`--runtime gemini`,
//! # `twr orchestrator use gemini`, `default_runtime = "gemini"`), the same as
//! # `--runtime 'acp:gemini --experimental-acp'`. `attach` (optional) is the agent's
//! # interactive command for `twr attach`, `{session}` standing for the session id and
//! # `{mode}` (optional) for the thread's permission mode (a thread in a restricted mode such
//! # as `plan` is attached only through a template with `{mode}`).
//! [runtimes.gemini]
//! acp = "gemini --experimental-acp"
//! attach = "gemini --resume {session}"
//! ```

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::account::{check_name, split_runtime, takes_accounts, Pin, Share};
use crate::adapter::CodexAuto;
use crate::notify::NotifyEvent;
use crate::paths::Paths;

/// How long a read of the config file waits for a writer's lock ([`read_locked`]).
pub const CONFIG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The text of the file at `path`, read under a shared lock (`flock`): the TUI's settings
/// write the config file in place under an exclusive one, so a reader never sees it half
/// written. A writer still holding it after `wait` fails the read (`TimedOut`): half a file is
/// never read. A file system without locks reads as it is.
pub fn read_locked(path: &Path, wait: std::time::Duration) -> std::io::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let until = std::time::Instant::now() + wait;
    loop {
        match f.try_lock_shared() {
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < until => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "another program is writing it (it holds its lock): try again",
                ));
            }
            _ => break,
        }
    }
    let mut text = String::new();
    f.read_to_string(&mut text)?;
    Ok(text)
}

/// Top-level configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Runtime for new threads.
    pub default_runtime: String,
    /// Orchestrator thread settings.
    pub orchestrator: OrchestratorConfig,
    /// Hub settings.
    pub hub: HubConfig,
    /// Remote boxes, by name.
    pub boxes: BTreeMap<String, BoxConfig>,
    /// Projects, by name.
    pub projects: BTreeMap<String, ProjectConfig>,
    /// Thread defaults.
    pub threads: ThreadsConfig,
    /// The trash deleted threads go to.
    pub trash: TrashConfig,
    /// What the hub keeps of its log (`[log]`).
    pub log: LogConfig,
    /// Seconds without progress in a turn or background tasks (not waiting on the user) before the hub
    /// raises `stalled`; 0 turns the stall timer off.
    pub stall_after_secs: u64,
    /// Background-only work with no task liveness signal this long is stopped. Zero = off.
    pub background_timeout_secs: u64,
    /// Seconds the hub waits, after a worker's background work ended with no turn after its
    /// interim report, before it wakes the worker itself with a prompt saying what ended (once
    /// per such report). Zero = never.
    pub wake_after_secs: u64,
    /// Named runtimes (`[runtimes.<name>]`): ACP agent commands under a short name.
    pub runtimes: BTreeMap<String, RuntimePreset>,
    /// Codex settings (`[codex]`).
    pub codex: CodexConfig,
    /// The hub's notification command (`[notify]`): off unless `command` is set.
    pub notify: NotifyConfig,
    /// Terminal UI settings (read by the TUI when it starts, not by the hub).
    pub tui: TuiConfig,
    /// Folders `thread.new` searches for a project given by its repository folder name (a
    /// plain name that is neither a configured project nor an absolute path): `<root>/<name>`
    /// and `<root>/*/<name>`, a git repository. A leading `~` is the home directory. `None`:
    /// [`DEFAULT_PROJECT_ROOTS`]; empty: no lookup.
    pub project_roots: Option<Vec<PathBuf>>,
    /// Accounts declared here (`[accounts.<runtime>.<name>]`), by runtime, then name
    /// ([`crate::account`]; `twr account add` records them in the log instead).
    pub accounts: BTreeMap<String, BTreeMap<String, AccountConfig>>,
    /// Per-runtime account policy (`[account_policy.<runtime>]`).
    pub account_policy: BTreeMap<String, AccountPolicy>,
}

/// Default `project_roots`: common code folders (the ones that do not exist are skipped).
/// Not `~/Documents/...`: reading it makes macOS ask the user for the hub's access to
/// Documents. Not `~/work`, often a folder of documents rather than repositories.
pub const DEFAULT_PROJECT_ROOTS: &[&str] = &[
    "~/code",
    "~/src",
    "~/projects",
    "~/dev",
    "~/Developer",
    "~/repos",
    "~/git",
    "~/workspace",
];

/// `path` with a leading `~` (alone or followed by `/`) replaced by `home`: `None` when it has
/// one and `home` is unknown. Other paths are returned as they are.
pub fn expand_home(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    match path.strip_prefix("~") {
        Ok(rest) => home.map(|h| h.join(rest)),
        Err(_) => Some(path.to_path_buf()),
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_runtime: "claude-code".into(),
            orchestrator: OrchestratorConfig::default(),
            hub: HubConfig::default(),
            boxes: BTreeMap::new(),
            projects: BTreeMap::new(),
            threads: ThreadsConfig::default(),
            trash: TrashConfig::default(),
            log: LogConfig::default(),
            stall_after_secs: 600,
            background_timeout_secs: 86400,
            wake_after_secs: 30,
            runtimes: BTreeMap::new(),
            codex: CodexConfig::default(),
            notify: NotifyConfig::default(),
            tui: TuiConfig::default(),
            project_roots: None,
            accounts: BTreeMap::new(),
            account_policy: BTreeMap::new(),
        }
    }
}

/// `[orchestrator]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestratorConfig {
    /// Runtime; `None` uses `default_runtime`.
    pub runtime: Option<String>,
    /// Working directory; `None` means the home directory.
    pub cwd: Option<PathBuf>,
    /// Start the orchestrator when the hub starts.
    pub autostart: bool,
    /// Model; `None` uses `threads.model`, else the runtime's default.
    pub model: Option<String>,
    /// Reasoning effort for new orchestrators, in the selected runtime's vocabulary.
    pub effort: Option<String>,
    /// Whether the orchestrator may read the committed files of the projects
    /// (`project.files`, `project.read`, `project.search`; default `true`).
    pub project_reads: bool,
    /// More file names the orchestrator may never read, as globs (`*` within a path part,
    /// `**` across parts, `?` one character; a pattern without `/` matches a file name, one
    /// with `/` a path from the repository's top). They add to the built-in secret names,
    /// never remove one.
    pub read_deny: Vec<String>,
    /// Whether the orchestrator keeps notes across sessions in the workspace's memory
    /// (`<data dir>/memory/`, `hub::memory`; default `true`). `false`: its memory tools refuse,
    /// nothing is loaded into its sessions, and the notes stay where they are.
    pub memory: bool,
    /// The first orchestrator's account: a pin of its runtime's accounts, as a project's
    /// `account` takes it (a name, a glob, a list, or a comma-separated string; the first of
    /// them that can be used); `None`: the policy's default, else the native account. `twr
    /// orchestrator use <runtime>:<account>` switches it (the switch is kept in the log).
    pub account: Option<Pin>,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        OrchestratorConfig {
            runtime: None,
            cwd: None,
            autostart: false,
            model: None,
            effort: None,
            project_reads: true,
            read_deny: Vec::new(),
            memory: true,
            account: None,
        }
    }
}

/// `[tui]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Capture the mouse (default `true`): dragging selects text inside one pane, never its
    /// borders, and copies it; the wheel scrolls the pane under the pointer. `false` leaves
    /// the mouse to the terminal (its own selection then spans whole screen rows).
    pub mouse: bool,
    /// Animate the working icon (default `true`) on what is running a turn right now: the
    /// orchestrator's header, worker cards, grid tiles and a dive's header, and the cursor
    /// where the main pane's next text will appear. `false` shows a still `twr_` marker (the
    /// cursor a still `_`) in the same places, and the screen is redrawn only when something
    /// changes.
    pub animate: bool,
    /// How the cards (and grid tiles) are ordered within a group ([`CardOrder`]).
    pub card_order: CardOrder,
    /// Below this many columns (a phone), the TUI shows one screen at a time: the
    /// conversation, what needs you, the threads (default 60; 0: never).
    pub compact_width: u16,
    /// Terminal notifications (`[tui.notify]`): off unless `terminal` picks a way.
    pub notify: TuiNotifyConfig,
    /// Suggest the next message (default `true`): after a turn of the thread on the main
    /// pane ends, ask a small model for a likely next message, shown as ghost text in the
    /// empty input box (Tab or → takes it). It sends that turn's prompt and the end of its
    /// reply to Anthropic through the Claude Code CLI, with your login; without the CLI or a
    /// login, nothing is sent. The TUI says so once, the first time one is due.
    pub suggest: bool,
    /// The small model suggestions come from, per runtime ([`SuggestModels`]): Claude Code's
    /// `claude-haiku-5-5` and Codex's `gpt-6-luna` by default.
    pub suggest_model: SuggestModels,
    /// Summarize what each active worker thread is doing (default `true`): its routine
    /// updates in the orchestrator conversation fold into one live line per thread, written
    /// by the same small model as suggestions ([`TuiConfig::suggest_model`]) under the same
    /// rules. `false`: the line is put together without a model.
    pub summaries: bool,
    /// The shortest time between two summaries of one thread, as a span (`5m`, the default;
    /// [`TuiConfig::summary_every`]). A thread whose updates did not change gets none.
    #[serde(rename = "summary_every")]
    pub summary_every_span: String,
    /// The colors the TUI draws with ([`ColorMode`]).
    pub colors: ColorMode,
}

impl TuiConfig {
    /// `summary_every` as a duration (checked by [`Config::validate`]; 5 minutes if it is not
    /// a span).
    pub fn summary_every(&self) -> std::time::Duration {
        parse_span(&self.summary_every_span).unwrap_or(DEFAULT_SUMMARY_EVERY)
    }
}

/// The shortest time between two summaries of one thread unless `[tui] summary_every` says
/// otherwise.
pub const DEFAULT_SUMMARY_EVERY: std::time::Duration = std::time::Duration::from_secs(300);

/// The shortest `[tui] summary_every` there may be: a summary costs a model call.
pub const MIN_SUMMARY_EVERY: std::time::Duration = std::time::Duration::from_secs(60);

/// `[tui] colors`: the colors the TUI draws with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ColorMode {
    /// From the environment: none when `NO_COLOR` is set (and not empty), else 256 colors
    /// unless `TERM` says the terminal has only 16 (`linux`, `vt100`, `xterm` without
    /// `COLORTERM`, ...).
    #[default]
    #[serde(rename = "auto")]
    Auto,
    /// 256 colors (and the 16 named ones).
    #[serde(rename = "256")]
    Full,
    /// The 16 named colors only: other colors are drawn as the nearest of them, and threads
    /// have no color of their own.
    #[serde(rename = "16")]
    Basic,
    /// No color: bold, dim, reverse video and the border shapes only.
    #[serde(rename = "none")]
    None,
}

/// The model Claude Code's suggestions come from unless `[tui] suggest_model` names another:
/// Haiku 5.5, pinned (the `haiku` alias ran it too with Claude Code 2.1.304).
pub const DEFAULT_SUGGEST_MODEL: &str = "claude-haiku-5-5";
/// The model Codex's suggestions come from unless `[tui] suggest_model` names another:
/// GPT-6-Luna, Codex 0.162's "fast and affordable model for easier tasks" (`model/list`).
pub const DEFAULT_SUGGEST_CODEX_MODEL: &str = "gpt-6-luna";

/// `[tui] suggest_model`: the model next-message suggestions come from, for each runtime that
/// can run them. A string names Claude Code's (`suggest_model = "claude-haiku-5-5"`); a table
/// names either or both (`suggest_model = { claude-code = "claude-haiku-5-5", codex =
/// "gpt-6-luna" }`), the other keeping its default. Only small models: one that looks large
/// ([`suggest_model_too_large`]) is never used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuggestModels {
    /// Claude Code's (`claude -p --model`).
    #[serde(rename = "claude-code")]
    pub claude_code: String,
    /// Codex's (`codex exec -m`).
    pub codex: String,
}

impl Default for SuggestModels {
    fn default() -> Self {
        SuggestModels {
            claude_code: DEFAULT_SUGGEST_MODEL.into(),
            codex: DEFAULT_SUGGEST_CODEX_MODEL.into(),
        }
    }
}

impl SuggestModels {
    /// The model for `runtime` (`claude-code`, `codex`); `None` for any other runtime.
    pub fn of(&self, runtime: &str) -> Option<&str> {
        match runtime {
            "claude-code" => Some(&self.claude_code),
            "codex" => Some(&self.codex),
            _ => None,
        }
    }
}

impl<'de> Deserialize<'de> for SuggestModels {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct PerRuntime {
            #[serde(rename = "claude-code")]
            claude_code: Option<String>,
            codex: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(
            untagged,
            expecting = "a model name, or a table of `claude-code` and `codex`"
        )]
        enum Raw {
            One(String),
            Per(PerRuntime),
        }
        let d0 = SuggestModels::default();
        Ok(match Raw::deserialize(d)? {
            Raw::One(m) => SuggestModels {
                claude_code: m,
                ..d0
            },
            Raw::Per(p) => SuggestModels {
                claude_code: p.claude_code.unwrap_or(d0.claude_code),
                codex: p.codex.unwrap_or(d0.codex),
            },
        })
    }
}

/// Why `model` is no model for `runtime`'s suggestions (`None`: it is a small one): a
/// suggestion runs after every turn, so only a small, cheap model may answer it. For Claude
/// Code, a Haiku (`claude-haiku-5-5`, `haiku`); for Codex, a Luna, mini or nano
/// (`gpt-6-luna`). Anything else (Sonnet, Opus, `gpt-6.1-sol`, `default`) looks large.
pub fn suggest_model_too_large(runtime: &str, model: &str) -> Option<String> {
    let m = model.to_ascii_lowercase();
    let small = match runtime {
        "claude-code" => m.contains("haiku"),
        "codex" => m.contains("luna") || m.contains("mini") || m.contains("nano"),
        _ => false,
    };
    (!small).then(|| {
        let want = match runtime {
            "codex" => DEFAULT_SUGGEST_CODEX_MODEL,
            _ => DEFAULT_SUGGEST_MODEL,
        };
        format!(
            "[tui] suggest_model names {model:?} for {runtime}, which looks like a large model: \
             suggestions run after every turn, so they use only a small one (such as {want:?}), \
             and none come from {runtime} until it names one"
        )
    })
}

/// `[tui] card_order`: how the TUI orders the cards within a group (what needs you first, then
/// working, failed, done, idle and parked threads).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CardOrder {
    /// By when the thread was created, pinned threads first; a card moves on its own only
    /// to the top when it needs you, other moves wait until the cards are left alone.
    #[default]
    Stable,
    /// The most recently active thread first, every move at once.
    Attention,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig {
            mouse: true,
            animate: true,
            card_order: CardOrder::Stable,
            compact_width: 60,
            notify: TuiNotifyConfig::default(),
            suggest: true,
            suggest_model: SuggestModels::default(),
            summaries: true,
            summary_every_span: "5m".to_string(),
            colors: ColorMode::Auto,
        }
    }
}

/// `[notify]`: the hub runs `command` when something needs the user ([`crate::notify`]), with
/// or without a TUI open. Every hub of the home reads it (the config file is shared): the
/// notification names its workspace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// The program and its arguments, run without a shell; empty (the default): no hook.
    /// `{title}`, `{body}` and `{workspace}` in an argument become the notification's.
    pub command: Vec<String>,
    /// What (default [`NotifyEvent::DEFAULTS`]: everything but `stalled`).
    pub events: Vec<NotifyEvent>,
    /// The text of what is asked or reported (a permission's command, a question, a report's
    /// summary) goes into the body and the JSON; off: only who and what kind of thing, as
    /// that text may hold secrets.
    pub detail: bool,
    /// The command is ended after this many seconds (1 to [`NOTIFY_TIMEOUT_MAX`]).
    pub timeout_secs: u64,
}

/// The warning for a config `file` with permission bits `mode` that holds secrets (a box
/// token, or a `[notify]` command, which may carry a webhook's secret) and that group or
/// others can access: `None` when it holds none or is private (`chmod 600`).
pub fn exposed_config_warning(config: &Config, file: &Path, mode: u32) -> Option<String> {
    let secrets =
        config.boxes.values().any(|b| b.token.is_some()) || !config.notify.command.is_empty();
    exposed_file_warning(
        file,
        mode,
        secrets.then_some("a box token or a [notify] command"),
    )
}

/// The warning for `file` with permission bits `mode` when it `holds` a secret (what, in
/// words) and group or others can access it.
fn exposed_file_warning(file: &Path, mode: u32, holds: Option<&str>) -> Option<String> {
    let mode = mode & 0o777;
    let holds = holds.filter(|_| mode & 0o077 != 0)?;
    Some(format!(
        "{} holds {holds} and other users can access it (mode {mode:03o}): run `chmod 600 {}`",
        file.display(),
        crate::paths::sh_word(&file.to_string_lossy())
    ))
}

/// Longest `[notify] timeout_secs`.
pub const NOTIFY_TIMEOUT_MAX: u64 = 600;

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig {
            command: Vec::new(),
            events: NotifyEvent::DEFAULTS.to_vec(),
            detail: false,
            timeout_secs: 30,
        }
    }
}

/// `[tui.notify]`: the TUI tells the terminal it runs in when something needs the user
/// ([`crate::notify`]). It is the TUI's own, read from the config of the machine the TUI runs
/// on; a TUI that is not open notifies nothing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TuiNotifyConfig {
    /// How (default `off`).
    pub terminal: TerminalAlert,
    /// When (default `unfocused`).
    pub when: NotifyWhen,
    /// What (default [`NotifyEvent::DEFAULTS`]: everything but `stalled`).
    pub events: Vec<NotifyEvent>,
}

impl Default for TuiNotifyConfig {
    fn default() -> Self {
        TuiNotifyConfig {
            terminal: TerminalAlert::Off,
            when: NotifyWhen::Unfocused,
            events: NotifyEvent::DEFAULTS.to_vec(),
        }
    }
}

/// How the TUI alerts its terminal (`[tui.notify] terminal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAlert {
    /// No alerts.
    #[default]
    Off,
    /// The bell (BEL): every terminal has one; it says nothing but that something happened.
    Bell,
    /// OSC 9 (iTerm2, WezTerm, Ghostty, kitty): a desktop notification with the text.
    Osc9,
    /// OSC 777 `notify` (urxvt, foot, WezTerm, Ghostty, VTE terminals that support it,
    /// Windows Terminal 1.26 with `compatibility.allowOSC777`): a desktop notification with a
    /// title and the text.
    Osc777,
}

/// When the TUI alerts (`[tui.notify] when`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyWhen {
    /// Only while the terminal is not focused (it reports focus changes). Until it reports
    /// one, a key or click in the last 30 s counts as focused.
    #[default]
    Unfocused,
    /// Whatever has focus.
    Always,
}

/// `[hub]`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HubConfig {
    /// Socket path override.
    pub socket: Option<PathBuf>,
    /// Runner WebSocket listen address (`host:port`); `None` disables the listener. A
    /// non-loopback address needs `tls` or `allow_insecure_remote`.
    pub listen: Option<String>,
    /// Serve the runner listener over TLS (`wss://`) with this certificate chain and key.
    pub tls: Option<TlsConfig>,
    /// Allow a plain `ws://` listener on a non-loopback address (the whole event stream and
    /// the hub's commands cross the network unencrypted; the token itself never does).
    pub allow_insecure_remote: bool,
}

/// `[hub.tls]`: PEM files.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// Certificate chain (PEM), leaf first.
    pub cert: PathBuf,
    /// Private key (PEM: PKCS#8, PKCS#1 or SEC1).
    pub key: PathBuf,
}

/// `[boxes.<name>]`: a remote box, which the hub reaches over ssh or through a command, or
/// whose runner connects to the hub ([`BoxConfig::transport`]).
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoxConfig {
    /// How the hub and the box's runner find each other; `None` is [`BoxTransport::Token`],
    /// what every box was before SSH boxes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transport: Option<BoxTransport>,
    /// Token the box's runner authenticates with (compared in constant time; never logged).
    /// A token box without one cannot connect; an SSH box has none ([`Config::validate`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// `ssh` destination of the box (`user@host`, or a `~/.ssh/config` host; whitespace
    /// separates extra `ssh` arguments placed before it, e.g. `-p 2222 me@host`, none that
    /// forwards anything or changes how ssh runs: [`check_ssh_words`]): `twr attach` on its
    /// threads runs `ssh -t <ssh> -- <command>`, and the hub connects to an SSH box with it
    /// (required there).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ssh: Option<String>,
    /// The program and arguments the hub runs to reach a command box (`["docker", "exec",
    /// "-i", "<container>"]`): it appends `sh -c <what ssh would run>` and speaks the box
    /// protocol over the command's stdin and stdout. Never run through a shell on the hub's
    /// side. Required on a command box, refused elsewhere ([`check_command_words`]). It may
    /// hold a secret (`-e KEY=value`): Debug output shows only its program and length.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    /// A command box's argv for what needs a terminal on the box (`twr box login`, `twr box
    /// project --clone`): `command` with `-t`, run the same way in the user's terminal.
    /// Optional; command boxes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_command: Option<Vec<String>>,
    /// The build id ([`crate::build_id`], 16 lowercase hex digits) of the twr installed on a
    /// box the hub connects to whose OS or CPU is not the hub's, which the hub cannot upload
    /// its own executable to: `twr box install` uploads a build for that target and writes
    /// its id here, and the hub runs that build. `None`: the hub's own build. SSH and command
    /// boxes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Projects on this box: where each repository lives there.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, BoxProjectConfig>,
}

impl BoxConfig {
    /// How the hub and the box's runner find each other: `transport`, else
    /// [`BoxTransport::Token`].
    pub fn transport(&self) -> BoxTransport {
        self.transport.unwrap_or_default()
    }

    /// How the hub reaches this box: all of it but its `build` and its `projects`, which `twr
    /// box install` and `twr box project` change while the hub runs. Two configs of one box
    /// that differ in this are two different boxes to those commands and to the hub.
    pub fn connection(&self) -> BoxConfig {
        BoxConfig {
            build: None,
            projects: BTreeMap::new(),
            ..self.clone()
        }
    }

    /// [`Config::validate`]'s checks of box `name`: an SSH box has an `ssh` destination that
    /// [`check_ssh_words`] accepts and no token; a command box has a `command` that
    /// [`check_command_words`] accepts, and neither a token nor `ssh`; `command` and
    /// `terminal_command` are a command box's only; `build`, on the boxes the hub connects to
    /// only, is a build id. (A token box's `ssh`, which only `twr attach` uses, is the hub's to
    /// check: every twr command loads the config, and one written before the check must not
    /// keep `twr hub stop` from working.)
    fn check(&self, name: &str) -> anyhow::Result<()> {
        let transport = self.transport();
        if transport != BoxTransport::Command {
            for (key, set) in [
                ("command", self.command.is_some()),
                ("terminal_command", self.terminal_command.is_some()),
            ] {
                if set {
                    anyhow::bail!(
                        "boxes.{name}.{key}: only a command box (transport = \"command\") has one"
                    );
                }
            }
        }
        match transport {
            BoxTransport::Command => {
                let Some(argv) = &self.command else {
                    anyhow::bail!(
                        "boxes.{name}: a command box (transport = \"command\") needs `command`, \
                         the program and arguments the hub runs to reach it ([\"docker\", \
                         \"exec\", \"-i\", \"<container>\"])"
                    );
                };
                check_command_words(argv)
                    .map_err(|why| anyhow::anyhow!("boxes.{name}.command: {why}"))?;
                if let Some(argv) = &self.terminal_command {
                    check_command_words(argv)
                        .map_err(|why| anyhow::anyhow!("boxes.{name}.terminal_command: {why}"))?;
                }
                if self.token.is_some() {
                    anyhow::bail!(
                        "boxes.{name}.token: a command box has no token, the hub runs its \
                         command itself (remove the token)"
                    );
                }
                if self.ssh.is_some() {
                    anyhow::bail!(
                        "boxes.{name}.ssh: a command box has no ssh destination, the hub runs \
                         its command instead (remove ssh, or use transport = \"ssh\")"
                    );
                }
            }
            BoxTransport::Ssh => {
                if self.ssh.as_deref().is_none_or(|s| s.trim().is_empty()) {
                    anyhow::bail!(
                        "boxes.{name}: an SSH box (transport = \"ssh\") needs `ssh`, the \
                         destination the hub connects to (`me@host`)"
                    );
                }
                self.check_ssh(name)?;
                if self.token.is_some() {
                    anyhow::bail!(
                        "boxes.{name}.token: an SSH box has no token, the hub connects to it \
                         over ssh (remove the token, or the transport for a box whose runner \
                         connects to the hub)"
                    );
                }
            }
            BoxTransport::Token => {
                if self.build.is_some() {
                    anyhow::bail!(
                        "boxes.{name}.build: only a box the hub connects to (transport ssh or \
                         command) has a build"
                    );
                }
            }
        }
        if let Some(id) = &self.build {
            if !crate::valid_build_id(id) {
                anyhow::bail!(
                    "boxes.{name}.build: {id:?} is not a build id ({} lowercase hex digits; \
                     `twr box install` writes it)",
                    crate::BUILD_ID_LEN
                );
            }
        }
        Ok(())
    }

    /// [`check_ssh_words`] of the box's `ssh` (none, or only spaces: nothing to check), as an
    /// error naming `boxes.<name>.ssh`.
    pub fn check_ssh(&self, name: &str) -> anyhow::Result<()> {
        match self.ssh.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(ssh) => check_ssh_words(ssh)
                .map_err(|why| anyhow::anyhow!("boxes.{name}.ssh = {ssh:?}: {why}")),
            None => Ok(()),
        }
    }
}

/// The `ssh` options that take a value (OpenSSH's `ssh` option letters: a value follows the
/// letter in the same word, or is the next word).
const SSH_VALUE_OPTIONS: &str = "BDEFIJLOPQRSWbceilmopw";

/// The `ssh` options without a value that a box's `ssh` may hold (the rest are refused,
/// [`ssh_flag_refusal`]).
const SSH_PLAIN_OPTIONS_OK: &str = "1246Cagkqvx";

/// Why `ssh` option `-<flag>` (a value one when [`SSH_VALUE_OPTIONS`] has it) has no place in
/// a box's `ssh`; `None` when it may be there.
fn ssh_flag_refusal(flag: char) -> Option<&'static str> {
    Some(match flag {
        'A' => "it forwards your ssh agent to the box",
        'K' => "it forwards your GSSAPI (Kerberos) credentials to the box",
        'X' | 'Y' => "it forwards your X11 display to the box",
        'L' | 'R' | 'D' => "it forwards ports",
        'w' => "it forwards a tunnel device",
        'W' => "it forwards ssh's stdin and stdout to a port instead of running twr",
        'M' | 'S' | 'O' => "it controls a shared (multiplexed) ssh connection",
        'N' | 's' => "ssh would run no command, or a subsystem, instead of twr",
        'f' => "ssh would go to the background",
        't' | 'T' => "twr decides whether ssh gets a terminal (`twr attach` asks for one)",
        'n' => "ssh would not read stdin, which carries the hub's connection",
        'G' | 'Q' | 'V' => "ssh would only print something and exit",
        'E' | 'y' => "ssh's own errors would go to a log instead of the hub",
        _ if SSH_VALUE_OPTIONS.contains(flag) || SSH_PLAIN_OPTIONS_OK.contains(flag) => {
            return None
        }
        _ => "twr does not know that ssh option",
    })
}

/// Why `ssh -o <key>=...` has no place in a box's `ssh` (`key` compared without regard to
/// case); `None` when it may be there.
fn ssh_option_refusal(key: &str) -> Option<&'static str> {
    let key = key.to_ascii_lowercase();
    Some(match key.as_str() {
        "forwardagent" => "it forwards your ssh agent to the box",
        "gssapidelegatecredentials" => "it forwards your GSSAPI (Kerberos) credentials",
        "forwardx11" | "forwardx11trusted" => "it forwards your X11 display to the box",
        "localforward" | "remoteforward" | "dynamicforward" | "clearallforwardings" => {
            "it is about forwarding ports, which twr turns off"
        }
        "tunnel" | "tunneldevice" => "it forwards a tunnel device",
        "permitlocalcommand" | "localcommand" => "it runs a local command",
        "batchmode" => "the hub runs ssh without prompting, and `twr box add` with prompts",
        "stdinnull"
        | "requesttty"
        | "remotecommand"
        | "sessiontype"
        | "forkafterauthentication" => "it changes what ssh runs, or how, which twr decides",
        "controlmaster" | "controlpath" | "controlpersist" => {
            "it controls a shared (multiplexed) ssh connection"
        }
        "include" | "match" | "host" => "it brings in other configuration",
        _ => return None,
    })
}

/// The key of `ssh -o <value>` as OpenSSH reads it: `"`s dropped, then leading `=`s and
/// whitespace skipped, and the key ends at the next `=` or whitespace (`=ControlMaster=yes`
/// sets `ControlMaster`).
fn ssh_option_key(value: &str) -> String {
    let unquoted: String = value.chars().filter(|c| *c != '"').collect();
    unquoted
        .trim_start_matches(|c: char| c == '=' || c.is_whitespace())
        .split(|c: char| c == '=' || c.is_whitespace())
        .next()
        .unwrap_or_default()
        .to_string()
}

/// Checks the words of a box's `ssh` setting (whitespace-separated: `ssh` options, then the
/// destination): exactly one destination, no `--`, and no option that forwards anything (your
/// agent, X11, GSSAPI credentials, ports, a tunnel), runs a local command, shares a connection
/// (`ControlMaster`, `-S`, `-O`), or changes what ssh runs or how (`-N`, `-T`, `-t`, `-n`,
/// `-f`, `RemoteCommand`, `BatchMode`, ...), which the hub's and `twr attach`'s ssh decide.
/// (Both also pass `-a -x -k` after these words, which turns agent, X11 and GSSAPI forwarding
/// off whatever comes before; a forwarding option is refused rather than silently
/// overridden.) Options are parsed as ssh
/// parses them: several letters in one word (`-qA`), a value in the same word (`-p2222`,
/// `-oForwardAgent=yes`) or the next, and `-o` keys (before `=` or alone, `"`s dropped)
/// without regard to case. A word after the destination that is not an option would be run
/// on the box: refused. Why not, else `Ok`.
pub fn check_ssh_words(ssh: &str) -> Result<(), String> {
    let mut words = ssh.split_whitespace();
    let mut destination: Option<&str> = None;
    while let Some(word) = words.next() {
        if word == "--" {
            return Err(
                "`--` would make the rest a command on the box: give options, then the \
                 destination"
                    .to_string(),
            );
        }
        let Some(letters) = word.strip_prefix('-').filter(|l| !l.is_empty()) else {
            if let Some(d) = destination {
                return Err(format!(
                    "`{word}` after the destination `{d}` would run on the box as a command: \
                     give options, then one destination"
                ));
            }
            destination = Some(word);
            continue;
        };
        for (i, flag) in letters.char_indices() {
            if let Some(why) = ssh_flag_refusal(flag) {
                return Err(format!("`-{flag}` (in `{word}`): {why}"));
            }
            if !SSH_VALUE_OPTIONS.contains(flag) {
                continue;
            }
            let rest = &letters[i + flag.len_utf8()..];
            let value = if rest.is_empty() {
                words
                    .next()
                    .ok_or_else(|| format!("`-{flag}` needs a value"))?
            } else {
                rest
            };
            if flag == 'o' {
                let key = ssh_option_key(value);
                let named = key.starts_with(|c: char| c.is_ascii_alphabetic())
                    && key.chars().all(|c| c.is_ascii_alphanumeric());
                if !named {
                    return Err(format!(
                        "`-o {value}`: twr cannot tell which ssh option it sets"
                    ));
                }
                if let Some(why) = ssh_option_refusal(&key) {
                    return Err(format!("`-o {value}`: {why}"));
                }
            }
            break;
        }
    }
    match destination {
        Some(_) => Ok(()),
        None => Err("no destination (`me@host`, after any ssh options)".to_string()),
    }
}

/// Checks a command box's argv (`[boxes.<name>] command` or `terminal_command`): at least one
/// word, none empty and none with a NUL byte, and not an ssh-like program, which hands its
/// arguments to a shell on the far end (that shell would split and expand twr's scripts, the
/// terminal's as much as the hub's): a program named `ssh`, one whose first argument is `ssh`
/// (`tailscale ssh`, `az ssh`), or the words `compute ssh` (`gcloud compute ssh`). A refusal
/// never quotes the words, which may hold a secret (`-e KEY=value`); only the program's file
/// name. Why not, else `Ok`.
pub fn check_command_words(argv: &[String]) -> Result<(), String> {
    let Some(program) = argv.first() else {
        return Err(
            "it is empty: give the program and its arguments ([\"docker\", \"exec\", \"-i\", \
             \"<container>\"])"
                .to_string(),
        );
    };
    for (i, word) in argv.iter().enumerate() {
        if word.is_empty() {
            return Err(format!("word {} is empty", i + 1));
        }
        if word.contains('\0') {
            return Err(format!("word {} contains a NUL byte", i + 1));
        }
    }
    let base = Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.clone());
    let like = if base == "ssh" {
        Some("ssh".to_string())
    } else if argv.get(1).is_some_and(|a| a == "ssh") {
        Some(format!("{base} ssh"))
    } else if argv.windows(2).any(|w| w[0] == "compute" && w[1] == "ssh") {
        Some(format!("{base} compute ssh"))
    } else {
        None
    };
    match like {
        Some(like) => Err(format!(
            "`{like}` passes its arguments to a shell on the box{}: use transport = \"ssh\" (a \
             Tailscale SSH host and a `gcloud compute config-ssh` host work with plain ssh)",
            if like == "ssh" { "" } else { ", as ssh does" }
        )),
        None => Ok(()),
    }
}

/// How Debug shows a command box's argv: its program and how many words it has, never the
/// words (one may hold a secret, and Debug output can reach logs).
fn argv_shown(argv: &Option<Vec<String>>) -> Option<String> {
    argv.as_ref().map(|a| {
        format!(
            "{} ({} words)",
            a.first().map(String::as_str).unwrap_or_default(),
            a.len()
        )
    })
}

impl fmt::Debug for BoxConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxConfig")
            .field("transport", &self.transport)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("ssh", &self.ssh)
            .field("command", &argv_shown(&self.command))
            .field("terminal_command", &argv_shown(&self.terminal_command))
            .field("build", &self.build)
            .field("projects", &self.projects)
            .finish()
    }
}

/// `[boxes.<name>] transport`: how the hub and a box's runner find each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BoxTransport {
    /// The box's runner (`twr runner --hub <url>`) connects to the hub's listener (`[hub]
    /// listen`) and both prove they know the box's `token`. For a box the hub cannot reach by
    /// ssh.
    #[default]
    Token,
    /// The hub runs twr on the box over `ssh` itself (`ssh <ssh> -- twr runner --stdio ...`)
    /// and speaks the runner protocol over that connection, inside a channel only the hub's
    /// key can open ([`crate::sealed`]): no token, listener or tunnel.
    Ssh,
    /// The hub runs the box's `command` itself (`docker exec -i <container>`, say) with `sh
    /// -c <what ssh would run>` appended, and speaks to the box over that command's stdin and
    /// stdout as it does over ssh's: the same probe, install, relay, daemon and sealed
    /// channel. For a container on the hub's machine.
    Command,
}

impl BoxTransport {
    /// Whether the hub connects to the box itself (ssh or a command), rather than the box's
    /// runner connecting to the hub.
    pub fn dialed(self) -> bool {
        matches!(self, BoxTransport::Ssh | BoxTransport::Command)
    }
}

/// `[boxes.<name>.projects.<project>]`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoxProjectConfig {
    /// Repository path on the box.
    pub repo: PathBuf,
    /// Runtime override for this project's threads on the box.
    pub runtime: Option<String>,
    /// The account pin of this project's threads on the box, on every runtime; with
    /// `accounts` and `exclude_accounts`, it replaces the project's ([`Config::account_pin`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Pin>,
    /// Per-runtime account pins, by runtime; one replaces `account` on its runtime.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Pin>,
    /// Accounts this project's threads on the box never use, on any runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_accounts: Option<Pin>,
}

/// `[projects.<name>]`
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfig {
    /// Repository path.
    pub repo: PathBuf,
    /// Runtime override for this project's threads.
    pub runtime: Option<String>,
    /// Default box for this project's threads.
    #[serde(rename = "box")]
    pub box_name: Option<String>,
    /// Whether this project's threads get a twr-managed worktree by default (overrides
    /// `threads.worktree`; `thread.new` `worktree` / `no_worktree` override both).
    pub worktree: Option<bool>,
    /// Environment variables for this project's runtimes, over `threads.env`
    /// ([`Config::thread_env`]).
    pub env: BTreeMap<String, String>,
    /// The branch this project's threads are merged into (`dev`, `origin/main`): their cards
    /// say how far ahead and behind of it they are, and whether a merge would conflict. Unset:
    /// the branch each thread's worktree was made from.
    pub base: Option<String>,
    /// How long a thread of this project stays idle (`24h`, `90m`, `2d`: no turn, background
    /// work or job, nothing recorded by it or for it but the hub's own bookkeeping) before the
    /// hub trims its worktree's build output, as `twr trim` does, in its disk round. Unset
    /// (the default): never by itself. [`ProjectConfig::trim_idle_after`].
    pub trim_idle_after: Option<String>,
    /// What an idle trim of this project's threads may delete, replacing `[threads]
    /// idle_trim_disposable` ([`Config::idle_trim_disposable`]).
    pub idle_trim_disposable: Option<Vec<String>>,
    /// What this project's Codex threads in `auto` do with a command their sandbox does not
    /// allow, over `[codex] auto` ([`Config::codex_auto`]).
    pub codex_auto: Option<CodexAuto>,
    /// The account pin of this project's threads, on every runtime: a name, a glob
    /// (`work-*`) or a list of them, a pool in order ([`Config::account_pin`]). A thread on a
    /// runtime with no usable account it matches is refused (the hub).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<Pin>,
    /// Per-runtime account pins, by runtime; one replaces `account` on its runtime.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub accounts: BTreeMap<String, Pin>,
    /// Accounts this project's threads never use, on any runtime, pin or not (applied after
    /// the pin).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_accounts: Option<Pin>,
}

/// `[codex]`: settings of the Codex runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CodexConfig {
    /// What a Codex thread in `auto` does with a command its sandbox does not allow
    /// ([`CodexAuto`]): let Codex's reviewer agent decide (the default), ask the user, or
    /// never run it.
    pub auto: CodexAuto,
}

impl Default for CodexConfig {
    /// `auto = "auto_review"`: Codex's own reviewer answers what the `auto` sandbox does not
    /// allow, as Claude Code's auto mode decides for itself. Not [`CodexAuto`]'s own default
    /// (`ask`), which is what a thread runs with when nothing names a value.
    fn default() -> Self {
        CodexConfig {
            auto: CodexAuto::AutoReview,
        }
    }
}

impl ProjectConfig {
    /// `trim_idle_after` as a duration (checked by [`Config::validate`]).
    pub fn trim_idle_after(&self) -> Option<std::time::Duration> {
        self.trim_idle_after.as_deref().and_then(parse_span)
    }
}

/// The longest span the config may write ([`parse_span`]): ten years.
pub const MAX_SPAN: std::time::Duration = std::time::Duration::from_secs(10 * 365 * 86400);

/// A span of time as the config writes it: a whole number and its unit, `s`, `m`, `h` or `d`
/// (`90m`, `24h`, `2d`); `None` for anything else, zero, or more than ten years ([`MAX_SPAN`]).
pub fn parse_span(s: &str) -> Option<std::time::Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    let secs = n
        .checked_mul(mult)
        .filter(|v| *v > 0 && *v <= MAX_SPAN.as_secs())?;
    Some(std::time::Duration::from_secs(secs))
}

/// Whether `s` is a size systemd takes for `MemoryHigh`: a whole number of bytes with an
/// optional `K`, `M`, `G` or `T`, or a percentage of the machine's memory, 1 to 100 (`4G`,
/// `512M`, `20%`).
pub fn valid_memory_size(s: &str) -> bool {
    if let Some(p) = s.strip_suffix('%') {
        return p.len() <= 3
            && !p.is_empty()
            && p.bytes().all(|b| b.is_ascii_digit())
            && p.parse::<u64>().is_ok_and(|p| (1..=100).contains(&p));
    }
    let (digits, mult) = match s.as_bytes().last() {
        Some(b'K') => (&s[..s.len() - 1], 1u64 << 10),
        Some(b'M') => (&s[..s.len() - 1], 1 << 20),
        Some(b'G') => (&s[..s.len() - 1], 1 << 30),
        Some(b'T') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    // In bytes, it must fit what systemd parses it into.
    !digits.is_empty()
        && digits.len() <= 20
        && digits.bytes().all(|b| b.is_ascii_digit())
        && digits
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(mult))
            .is_some_and(|n| n > 0 && n < u64::MAX)
}

/// `[runtimes.<name>]`: an Agent Client Protocol agent under a short name. `--runtime
/// <name>` is `--runtime 'acp:<acp>'`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimePreset {
    /// The agent's ACP command line (split like a shell would, without expansion).
    pub acp: String,
    /// The agent's interactive command for `twr attach`, `{session}` standing for the
    /// session id, `{mode}` (optional) for the thread's permission mode (a thread in a
    /// restricted mode is attached only through a template with `{mode}`), and `{model}` /
    /// `{effort}` (optional, each in one argument with its flag: `--model={model}`) for its
    /// model and reasoning effort; `None`: the thread has no `attach`.
    pub attach: Option<String>,
}

/// Runtime names a `[runtimes.<name>]` preset may not take (the built-in runtimes).
pub const RESERVED_RUNTIME_NAMES: &[&str] =
    &["claude-code", "codex", "grok-build", "acp", "pty", "fake"];

/// Placeholder of the session id in a preset's `attach` command.
pub const SESSION_PLACEHOLDER: &str = "{session}";

impl RuntimePreset {
    /// Checks preset `name`: a plain name (letters, digits, `-`, `_`, `.`) that is not a
    /// built-in runtime, a command, and an `attach` command naming `{session}`.
    pub fn validate(&self, name: &str) -> anyhow::Result<()> {
        let plain = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !plain {
            anyhow::bail!(
                "runtimes.{name:?}: a runtime name is 1-64 letters, digits, '-', '_' or '.'"
            );
        }
        if RESERVED_RUNTIME_NAMES.contains(&name) {
            anyhow::bail!("runtimes.{name}: {name:?} is a built-in runtime");
        }
        if self.acp.trim().is_empty() {
            anyhow::bail!("runtimes.{name}: `acp` (the agent's ACP command line) is required");
        }
        if let Some(a) = &self.attach {
            if !a.contains(SESSION_PLACEHOLDER) {
                anyhow::bail!(
                    "runtimes.{name}: `attach` must contain {SESSION_PLACEHOLDER} (where the \
                     session id goes)"
                );
            }
        }
        Ok(())
    }
}

/// `[accounts.<runtime>.<name>]`: an account of a runtime that takes accounts
/// ([`crate::account::ACCOUNT_RUNTIMES`]), declared in config. Read when the hub starts.
/// [`Config::validate`] checks what it can; whether `dir` is the runtime's own default
/// directory (which must be `native = true` instead: setting `CLAUDE_CONFIG_DIR=~/.claude` is
/// not the same as leaving it unset) needs the adapters' knowledge, so the hub checks that.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountConfig {
    /// Its config directory, absolute or under `~` (expanded when the config is read);
    /// `None`: `<data dir>/accounts/<runtime>/<name>` ([`crate::account::default_dir`]).
    /// Never set on the native account.
    pub dir: Option<PathBuf>,
    /// What it shares with the native account.
    pub share: Share,
    /// This names the native account (the runtime's own default directory, no variable
    /// set) instead of declaring another one. At most one per runtime.
    pub native: bool,
}

/// `[account_policy.<runtime>]`: how a runtime's accounts are picked.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountPolicy {
    /// The account of a thread nothing pins (on the hub's own box); `None`: the native
    /// account.
    pub default: Option<String>,
}

/// The account settings the design plans for later versions
/// (`docs/design/multi-account.md`): `[account_policy.<runtime>]` `balance`, `pool`,
/// `failover`, `avoid_above`, `move` and `resume_at_reset`, and `[accounts.codex.<name>]`
/// `sessions`. None is available yet: each is refused as an unknown key, saying so
/// ([`planned_account_key`]), never accepted and ignored.
pub const PLANNED_ACCOUNT_KEYS: [&str; 7] = [
    "balance",
    "pool",
    "failover",
    "avoid_above",
    "move",
    "resume_at_reset",
    "sessions",
];

/// The planned account key ([`PLANNED_ACCOUNT_KEYS`]) a config parse error `text` refuses as
/// unknown in `[account_policy.<runtime>]` (whose only key is `default`) or in an account's
/// table (`dir`, `share`, `native`), if it is one.
fn planned_account_key(text: &str) -> Option<&'static str> {
    let in_account_table = text.contains("expected `default`")
        || text.contains("expected one of `dir`, `share`, `native`");
    PLANNED_ACCOUNT_KEYS
        .into_iter()
        .find(|k| in_account_table && text.contains(&format!("unknown field `{k}`")))
}

/// `[threads]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadsConfig {
    /// Emit `agent.thinking` events.
    pub thinking: bool,
    /// Default runtime permission mode for new threads (default `auto`: the agent's own auto
    /// mode decides which tool calls ask; whatever asks is a permission item only the user
    /// answers). The hub refuses to open with an id that is neither a known mode (`plan`,
    /// `dontAsk`, `read-only`, `manual`, `default`, `acceptEdits`, `auto`,
    /// `bypassPermissions`, ...; case, `-` and `_` do not matter) nor a runtime's own (Codex
    /// `untrusted`, `on-request`, `never`), and uses them in their canonical spelling. The
    /// orchestrator never runs in a mode wider than asking (`manual`), whatever this says.
    pub default_mode: String,
    /// Default model for new threads (`thread.new` `model` overrides it); `None` leaves the
    /// choice to the runtime.
    pub model: Option<String>,
    /// Ignored paths removing a parked thread's worktree may delete: `name/` matches any
    /// directory component, `name` any component. Default [`DEFAULT_PARK_DISPOSABLE`].
    pub park_disposable: Vec<String>,
    /// What an idle trim (`[projects.<name>] trim_idle_after`) may delete, in the same syntax:
    /// only an ignored entry that matches both this and `park_disposable`, so the hub deletes by
    /// itself only what a build always makes again. `twr trim`, a disk card's Trim, parks and
    /// deletions go by `park_disposable` alone. Default [`DEFAULT_IDLE_TRIM_DISPOSABLE`];
    /// `[projects.<name>] idle_trim_disposable` replaces it per project.
    pub idle_trim_disposable: Vec<String>,
    /// Whether new threads get a twr-managed worktree by default (default `true`). Off,
    /// threads work in the repository checkout itself (parallel threads of one repository
    /// then share it). `[projects.<name>] worktree` overrides it per project.
    pub worktree: bool,
    /// Environment variables for every worker thread's runtime (not the orchestrator's), set
    /// when it starts: build settings such as `CARGO_INCREMENTAL` or `CARGO_TARGET_DIR`.
    /// `[projects.<name>] env` adds to it per project ([`Config::thread_env`]). Names are
    /// checked ([`check_env_name`]); `TWR_*` is twr's own.
    pub env: BTreeMap<String, String>,
    /// Free disk, in GB, under which the hub raises a disk card for a box (where its
    /// worktrees are) and the TUI says how much is left (default [`DEFAULT_DISK_WARN_GB`]);
    /// 0 = never.
    pub disk_warn_gb: u64,
    /// How many job slots the machine has (`twr job run`): the jobs of every twr hub of this
    /// home take them from one pool, each as many as it asks for, and wait for them in turn.
    /// 0 (the default) = the machine's CPUs.
    pub job_slots: u64,
    /// The CPU weight of the workers on the hub's machine, together (1 to 10000; systemd's
    /// default for anything else is 100, and less gives way to it sooner when the CPUs are
    /// busy): each local worker's runtime runs in a systemd user scope in
    /// [`WORKERS_SLICE`], whose weight this is. Without systemd (macOS), `nice` instead.
    /// `None` (the default): workers run as the hub's children, unlimited.
    pub cpu_weight: Option<u64>,
    /// The memory over which a local worker's runtime, with everything it starts, is slowed
    /// and reclaimed hard instead of taking the machine down (systemd's `MemoryHigh` on its
    /// scope: `4G`, `512M`, `20%`). systemd only. `None` (the default): no limit.
    pub memory_high: Option<String>,
    /// What the hub does about a worker whose prompt cache expires while its context is large
    /// ([`IdleCompaction`]): ask before a message goes to it (the default), also compact it
    /// itself just before the cache expires, or neither.
    pub idle_compaction: IdleCompaction,
}

/// `[threads] idle_compaction`: what the hub does about a local worker whose runtime caches
/// its prompt for a while (Claude Code: 1 hour, 5 minutes with an API key) when that cache
/// expires with a large context (`twr_hub`'s `hub::idle_compact`). A message sent after the
/// cache expired makes the runtime read all of the context again, uncached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleCompaction {
    /// Before a message of the user's or the orchestrator's goes to such a worker, a card
    /// asks the user whether to compact its context first.
    #[default]
    Ask,
    /// As `Ask`, and the hub compacts an idle worker's context itself shortly before a cache
    /// of an hour or more expires.
    BeforeExpiry,
    /// Neither.
    Off,
}

impl IdleCompaction {
    /// Every value, as `[threads] idle_compaction` spells it.
    pub const ALL: [IdleCompaction; 3] = [
        IdleCompaction::Ask,
        IdleCompaction::BeforeExpiry,
        IdleCompaction::Off,
    ];

    /// The config spelling: `ask`, `before_expiry`, `off`.
    pub fn as_str(self) -> &'static str {
        match self {
            IdleCompaction::Ask => "ask",
            IdleCompaction::BeforeExpiry => "before_expiry",
            IdleCompaction::Off => "off",
        }
    }
}

/// The systemd user slice the workers' scopes go in ([`ThreadsConfig::cpu_weight`]): a
/// top-level one, so its weight counts against the user manager's other units.
pub const WORKERS_SLICE: &str = "twr.slice";

/// Largest `threads.cpu_weight` (systemd's).
pub const CPU_WEIGHT_MAX: u64 = 10000;

/// Default `threads.disk_warn_gb`.
pub const DEFAULT_DISK_WARN_GB: u64 = 10;

/// `[trash]`: deleted threads' worktrees and branches (`thread.delete`) go to the trash of
/// their box (`<data dir>/trash/<item>/`, refs under `refs/twr-trash/<item>/`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TrashConfig {
    /// Days an item stays in the trash before the hub purges it (default 7; checked when the
    /// hub starts and every hour). 0 purges at the next check.
    pub keep_days: u64,
}

/// Default `trash.keep_days`.
pub const DEFAULT_TRASH_KEEP_DAYS: u64 = 7;

impl Default for TrashConfig {
    fn default() -> Self {
        TrashConfig {
            keep_days: DEFAULT_TRASH_KEEP_DAYS,
        }
    }
}

impl TrashConfig {
    /// How long an item stays, in milliseconds.
    pub fn keep_ms(&self) -> u64 {
        self.keep_days.saturating_mul(24 * 3600 * 1000)
    }
}

/// `[log]`: what the hub keeps of its log in `twr.db` (`hub::prune`). The control events (what
/// threads, turns, items, reports and decisions are) always stay. The agent events (`agent.*`:
/// what the agents said, thought and did) of turns older than `retention_days` move to
/// `archive.db` beside it, except what the hub still reads (each turn's final reply, the last
/// turn of each thread, a turn whose reactions are not complete); those of a deleted thread move
/// once its trash item is purged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// Days the agent events of a turn stay in `twr.db` after it ended (default 30, at least
    /// [`MIN_LOG_RETENTION_DAYS`]).
    pub retention_days: u64,
    /// Move them into `archive.db` (default); `false` deletes them.
    pub archive: bool,
}

/// Default `log.retention_days`.
pub const DEFAULT_LOG_RETENTION_DAYS: u64 = 30;

/// Smallest `log.retention_days`: a day more than a remote box's recorded commands are resent
/// ([`crate::runner::RESEND_HORIZON`], 6 days), whose resend reads its turn's agent events.
pub const MIN_LOG_RETENTION_DAYS: u64 = crate::runner::RESEND_HORIZON.as_secs() / 86_400 + 1;

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            retention_days: DEFAULT_LOG_RETENTION_DAYS,
            archive: true,
        }
    }
}

impl LogConfig {
    /// How long a turn's agent events stay.
    pub fn retention(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.retention_days.saturating_mul(86_400))
    }
}

/// Why `name` cannot be the name of a `threads.env` / `[projects.<name>] env` variable: empty,
/// with `=` or a NUL byte, or `TWR_*` (twr's own markers, which the hub sets: an agent's
/// processes are told from the user's by them).
pub fn check_env_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return Err(format!("{name:?} is not an environment variable name"));
    }
    if name.to_ascii_uppercase().starts_with("TWR_") {
        return Err(format!(
            "{name} is twr's own (TWR_* variables cannot be set)"
        ));
    }
    Ok(())
}

/// Default `threads.park_disposable`: regenerated build output and caches. The same list as
/// `twr_runner::local::DEFAULT_PARK_DISPOSABLE` (twr-core cannot depend on twr-runner; a
/// twr-cli test keeps them equal).
pub const DEFAULT_PARK_DISPOSABLE: &[&str] = &[
    "target/",
    "node_modules/",
    "dist/",
    "build/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".next/",
    ".cache/",
    ".gradle/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".tox/",
    "coverage/",
    ".DS_Store",
];

/// Default `threads.idle_trim_disposable`: build output and caches a build or a test run
/// always makes again, unlike `dist/`, `build/`, `.cache/`, `.venv/` or `coverage/`, which
/// can hold what nothing regenerates.
pub const DEFAULT_IDLE_TRIM_DISPOSABLE: &[&str] = &[
    "target/",
    "node_modules/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".tox/",
    ".gradle/",
    ".next/",
];

/// Why `entry` of an `idle_trim_disposable` can match nothing (the syntax of
/// `park_disposable`: a name, or a name and `/` for a directory, matched against one path
/// component): empty, `.`, `..`, with a `/` inside or a NUL byte.
fn check_disposable_entry(entry: &str) -> Result<(), String> {
    let name = entry.strip_suffix('/').unwrap_or(entry);
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(format!(
            "{entry:?} is not a file or directory name (`name` or `name/`)"
        ));
    }
    Ok(())
}

impl Default for ThreadsConfig {
    fn default() -> Self {
        ThreadsConfig {
            thinking: false,
            default_mode: "auto".into(),
            model: None,
            park_disposable: DEFAULT_PARK_DISPOSABLE
                .iter()
                .map(|s| s.to_string())
                .collect(),
            idle_trim_disposable: DEFAULT_IDLE_TRIM_DISPOSABLE
                .iter()
                .map(|s| s.to_string())
                .collect(),
            worktree: true,
            env: BTreeMap::new(),
            disk_warn_gb: DEFAULT_DISK_WARN_GB,
            job_slots: 0,
            cpu_weight: None,
            memory_high: None,
            idle_compaction: IdleCompaction::Ask,
        }
    }
}

impl Config {
    /// Parses TOML (no environment overrides) and checks values serde cannot
    /// ([`Config::validate`]).
    pub fn from_toml_str(s: &str) -> anyhow::Result<Config> {
        let mut cfg: Config = toml::from_str(s).map_err(|e| {
            let why = planned_account_key(&e.to_string()).map_or_else(
                || "parsing twr config".to_string(),
                |k| {
                    format!(
                        "parsing twr config: `{k}` is planned for a later version of account \
                         support and is not available yet (docs/accounts.md)"
                    )
                },
            );
            anyhow::Error::new(e).context(why)
        })?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        cfg.expand_account_dirs(home.as_deref());
        cfg.validate()?;
        Ok(cfg)
    }

    /// Expands a leading `~` of every account `dir` with `home` ([`expand_home`]); one under
    /// `~` stays as it is when `home` is unknown (and [`Config::validate`] refuses it).
    pub fn expand_account_dirs(&mut self, home: Option<&Path>) {
        for a in self.accounts.values_mut().flat_map(|m| m.values_mut()) {
            if let Some(d) = &a.dir {
                if let Some(e) = expand_home(d, home) {
                    a.dir = Some(e);
                }
            }
        }
    }

    /// Checks values the types allow but twr does not: runtime presets
    /// ([`RuntimePreset::validate`]), `project_roots` that are neither absolute nor under `~`,
    /// an SSH box without `ssh` or with a `token` (which it would never use: refused, so a
    /// token is never silently ignored), a command box without a valid `command` or with a
    /// `token` or `ssh`, a `command` or `terminal_command` off a command box, and a box `build`
    /// that is not a build id or not on a box the hub connects to.
    /// Box tokens that are too short are the hub's to refuse ([`Config::check_box_tokens`]):
    /// every twr command loads the config, and one that is too short must not keep `twr hub
    /// stop` from stopping a hub started before the minimum.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.log.retention_days < MIN_LOG_RETENTION_DAYS {
            anyhow::bail!(
                "log.retention_days: {} is less than {MIN_LOG_RETENTION_DAYS} (a remote box's \
                 commands are resent for 6 days, and a resend reads its turn's events)",
                self.log.retention_days
            );
        }
        for (name, preset) in &self.runtimes {
            preset.validate(name)?;
        }
        for (name, b) in &self.boxes {
            b.check(name)?;
        }
        match parse_span(&self.tui.summary_every_span) {
            Some(d) if d >= MIN_SUMMARY_EVERY => {}
            _ => anyhow::bail!(
                "tui.summary_every: {:?} is not a span of time of a minute or more (5m, 1h)",
                self.tui.summary_every_span
            ),
        }
        for (name, p) in &self.projects {
            if let Some(b) = &p.base {
                if b.is_empty() || b.starts_with('-') || b.len() > 256 || b.contains(['\0', '\n']) {
                    anyhow::bail!("projects.{name}.base: {b:?} is not a ref");
                }
            }
            if let Some(t) = &p.trim_idle_after {
                if parse_span(t).is_none() {
                    anyhow::bail!(
                        "projects.{name}.trim_idle_after: {t:?} is not a span of time (90m, 24h, 2d)"
                    );
                }
            }
            for e in p.idle_trim_disposable.iter().flatten() {
                check_disposable_entry(e)
                    .map_err(|e| anyhow::anyhow!("projects.{name}.idle_trim_disposable: {e}"))?;
            }
        }
        for e in &self.threads.idle_trim_disposable {
            check_disposable_entry(e)
                .map_err(|e| anyhow::anyhow!("threads.idle_trim_disposable: {e}"))?;
        }
        if let Some(w) = self.threads.cpu_weight {
            if !(1..=CPU_WEIGHT_MAX).contains(&w) {
                anyhow::bail!("threads.cpu_weight must be 1 to {CPU_WEIGHT_MAX}");
            }
        }
        if let Some(m) = &self.threads.memory_high {
            if !valid_memory_size(m) {
                anyhow::bail!("threads.memory_high: {m:?} is not a size (4G, 512M, 20%)");
            }
        }
        let envs = std::iter::once(("threads".to_string(), &self.threads.env)).chain(
            self.projects
                .iter()
                .map(|(name, p)| (format!("projects.{name}"), &p.env)),
        );
        for (table, env) in envs {
            for (k, v) in env {
                check_env_name(k).map_err(|e| anyhow::anyhow!("{table}.env: {e}"))?;
                if v.contains('\0') {
                    anyhow::bail!("{table}.env: the value of {k} contains a NUL byte");
                }
            }
        }
        if let Some(first) = self.notify.command.first() {
            if first.is_empty() {
                anyhow::bail!("notify.command: the program is empty");
            }
        }
        if self.notify.command.iter().any(|a| a.contains('\0')) {
            anyhow::bail!("notify.command: an argument contains a NUL byte");
        }
        if !(1..=NOTIFY_TIMEOUT_MAX).contains(&self.notify.timeout_secs) {
            anyhow::bail!("notify.timeout_secs must be 1 to {NOTIFY_TIMEOUT_MAX}");
        }
        for root in self.project_roots.iter().flatten() {
            if !(root.is_absolute() || root.starts_with("~")) {
                anyhow::bail!(
                    "project_roots: {} is neither an absolute path nor under ~ (e.g. \"~/code\")",
                    root.display()
                );
            }
        }
        self.validate_accounts()
    }

    /// The account keys of [`Config::validate`]: `[accounts]` runtimes that take accounts,
    /// valid names, at most one native account per runtime and no `dir` on it, absolute
    /// directories (after `~` expansion) that no two accounts share, valid pins and
    /// exclusions, `[account_policy]` runtimes and defaults, and `orchestrator.account`. (A
    /// named account called `default` while nothing names the native account, whose name that
    /// is then, is checked at the hub's start, with twr's records:
    /// [`crate::account::AccountsView::check_config_default`].)
    fn validate_accounts(&self) -> anyhow::Result<()> {
        let check_runtime = |key: &str, rt: &str| -> anyhow::Result<()> {
            if takes_accounts(rt) {
                Ok(())
            } else if self.runtimes.contains_key(rt) {
                anyhow::bail!(
                    "{key}: accounts for [runtimes] presets come in a later version of twr"
                )
            } else {
                anyhow::bail!(
                    "{key}: {rt:?} does not take accounts (claude-code, codex or grok-build do)"
                )
            }
        };
        let mut dirs: BTreeMap<&Path, String> = BTreeMap::new();
        for (rt, names) in &self.accounts {
            check_runtime(&format!("accounts.{rt}"), rt)?;
            let natives: Vec<&String> = names
                .iter()
                .filter(|(_, a)| a.native)
                .map(|(n, _)| n)
                .collect();
            if let [a, b, ..] = natives.as_slice() {
                anyhow::bail!(
                    "accounts.{rt}: {a} and {b} are both `native = true`; one account is the native one"
                );
            }
            for (name, a) in names {
                let key = format!("accounts.{rt}.{name}");
                check_name(name).map_err(|e| anyhow::anyhow!("{key}: {e}"))?;
                let Some(dir) = &a.dir else {
                    continue;
                };
                if a.native {
                    anyhow::bail!(
                        "{key}: a `native = true` account has no `dir` (it is the runtime's own \
                         default directory)"
                    );
                }
                if !dir.is_absolute() {
                    anyhow::bail!(
                        "{key}.dir: {} is not an absolute path (or under ~ with HOME set)",
                        dir.display()
                    );
                }
                if let Some(other) = dirs.insert(dir.as_path(), key.clone()) {
                    anyhow::bail!(
                        "{key}.dir: {} is also {other}'s directory; each account has its own",
                        dir.display()
                    );
                }
            }
        }
        let pin = |key: String, p: &Pin| p.validate().map_err(|e| anyhow::anyhow!("{key}: {e}"));
        let projects = self.projects.iter().map(|(name, p)| {
            (
                format!("projects.{name}"),
                &p.account,
                &p.accounts,
                &p.exclude_accounts,
            )
        });
        let box_projects = self.boxes.iter().flat_map(|(b, bc)| {
            bc.projects.iter().map(move |(name, p)| {
                (
                    format!("boxes.{b}.projects.{name}"),
                    &p.account,
                    &p.accounts,
                    &p.exclude_accounts,
                )
            })
        });
        for (table, account, accounts, exclude) in projects.chain(box_projects) {
            if let Some(p) = account {
                pin(format!("{table}.account"), p)?;
            }
            for (rt, p) in accounts {
                let key = format!("{table}.accounts.{rt}");
                check_runtime(&key, rt)?;
                pin(key, p)?;
            }
            if let Some(p) = exclude {
                pin(format!("{table}.exclude_accounts"), p)?;
            }
        }
        for (rt, policy) in &self.account_policy {
            check_runtime(&format!("account_policy.{rt}"), rt)?;
            if let Some(d) = &policy.default {
                check_name(d).map_err(|e| anyhow::anyhow!("account_policy.{rt}.default: {e}"))?;
            }
        }
        if let Some(p) = &self.orchestrator.account {
            pin("orchestrator.account".into(), p)?;
        }
        self.refuse_runtime_accounts()
    }

    /// Refuses an account part (`claude-code:work`) in `default_runtime`, a project's
    /// `runtime` and a box project's `runtime`: there it would act as a pin of every thread
    /// it reaches, outranking the project's own pins. A project's pin
    /// (`accounts.<runtime>`) or `[account_policy.<runtime>] default` says it. Only
    /// `[orchestrator] runtime` takes one (the orchestrator's account).
    fn refuse_runtime_accounts(&self) -> anyhow::Result<()> {
        let refuse = |key: &str, spec: &str, rt: &str, instead: String| {
            anyhow::anyhow!(
                "{key}: {spec:?} names an account: write the runtime alone ({rt:?}) and the \
                 account {instead}"
            )
        };
        let policy =
            |rt: &str, account: &str| format!("as [account_policy.{rt}] default = {account:?}");
        let spec = self.default_runtime.as_str();
        if let (rt, Some(account)) = split_runtime(spec) {
            return Err(refuse("default_runtime", spec, rt, policy(rt, account)));
        }
        let projects = self
            .projects
            .iter()
            .map(|(name, p)| (format!("projects.{name}"), &p.runtime));
        let box_projects = self.boxes.iter().flat_map(|(b, bc)| {
            bc.projects
                .iter()
                .map(move |(name, p)| (format!("boxes.{b}.projects.{name}"), &p.runtime))
        });
        for (table, runtime) in projects.chain(box_projects) {
            let Some(spec) = runtime else {
                continue;
            };
            if let (rt, Some(account)) = split_runtime(spec) {
                let instead = format!(
                    "as this project's pin ({table}.accounts.{rt} = {account:?}) or {}",
                    policy(rt, account)
                );
                return Err(refuse(&format!("{table}.runtime"), spec, rt, instead));
            }
        }
        Ok(())
    }

    /// Refuses box tokens shorter than [`crate::runner::TOKEN_MIN_LEN`]: the hub does not
    /// start with one (the error names the key and how to make a new token, not the token).
    pub fn check_box_tokens(&self) -> anyhow::Result<()> {
        for (name, b) in &self.boxes {
            if let Some(token) = &b.token {
                crate::runner::check_token_len(token, &format!("boxes.{name}.token"))
                    .map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }

    /// The folders `thread.new` searches for a project given by its folder name, `~` expanded
    /// with `home` (an entry under `~` is dropped when `home` is unknown): `project_roots`,
    /// else [`DEFAULT_PROJECT_ROOTS`]. Whether they exist is not checked here.
    pub fn project_roots(&self, home: Option<&Path>) -> Vec<PathBuf> {
        let roots: Vec<PathBuf> = match &self.project_roots {
            Some(r) => r.clone(),
            None => DEFAULT_PROJECT_ROOTS.iter().map(PathBuf::from).collect(),
        };
        roots.iter().filter_map(|r| expand_home(r, home)).collect()
    }

    /// What the hub warns about when it starts with this config, read from `file` and the
    /// `boxes.toml` next to it ([`Config::load_with_boxes`]; `hub.info` `warnings`, and its
    /// log): box tokens shorter than [`crate::runner::TOKEN_GOOD_LEN`], and either file when it
    /// holds a secret and other users can access it ([`exposed_config_warning`]). Each file is
    /// judged by its own secrets: `file`'s are the tokens of the boxes `boxes.toml` does not
    /// hold, and `[notify]`; `boxes.toml`'s are its tokens. Without a file only the config
    /// itself is checked; a file whose mode cannot be read is not.
    pub fn warnings(&self, file: Option<&Path>) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let mode_of = |f: &Path| Some(std::fs::metadata(f).ok()?.permissions().mode());
        let mut out: Vec<String> = self
            .boxes
            .iter()
            .filter_map(|(name, b)| {
                let token = b.token.as_deref()?;
                crate::runner::check_token_len(token, &format!("boxes.{name}.token"))
                    .ok()
                    .flatten()
            })
            .collect();
        out.extend(self.tui_warnings());
        let Some(file) = file else {
            return out;
        };
        let boxes_file = BoxesFile::path_for(file);
        // (A `boxes.toml` that does not load: the config could not have loaded either.)
        let added = BoxesFile::load(&boxes_file).unwrap_or_default().boxes;
        let own = self
            .boxes
            .iter()
            .any(|(name, b)| b.token.is_some() && !added.contains_key(name))
            || !self.notify.command.is_empty();
        let own = own.then_some("a box token or a [notify] command");
        out.extend(mode_of(file).and_then(|mode| exposed_file_warning(file, mode, own)));
        let theirs = added
            .values()
            .any(|b| b.token.is_some())
            .then_some("a box token");
        out.extend(
            mode_of(&boxes_file).and_then(|mode| exposed_file_warning(&boxes_file, mode, theirs)),
        );
        out
    }

    /// [`Config::warnings`] about `[tui]`, which the file can't hold wrong: a suggestion model
    /// that looks large ([`suggest_model_too_large`]).
    fn tui_warnings(&self) -> Vec<String> {
        let m = &self.tui.suggest_model;
        [("claude-code", &m.claude_code), ("codex", &m.codex)]
            .into_iter()
            .filter_map(|(rt, model)| suggest_model_too_large(rt, model))
            .collect()
    }

    /// Loads `path` (missing file = defaults).
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        // Under a shared lock: never a file the TUI's settings are writing ([`read_locked`]).
        match read_locked(path, CONFIG_LOCK_WAIT) {
            Ok(s) => Self::from_toml_str(&s).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Loads `config_file` ([`Config::load`]) with the boxes of the `boxes.toml` next to it
    /// ([`BoxesFile::path_for`]; missing = none) merged into `boxes`, and checks the result
    /// ([`Config::validate`]). A box named in both files is an error that names both.
    pub fn load_with_boxes(config_file: &Path) -> anyhow::Result<Config> {
        let mut config = Self::load(config_file)?;
        let boxes_file = BoxesFile::path_for(config_file);
        let added = BoxesFile::load(&boxes_file)?;
        if added.boxes.is_empty() {
            return Ok(config);
        }
        for (name, b) in added.boxes {
            if config.boxes.contains_key(&name) {
                anyhow::bail!(
                    "box {name} is in both {} and {}: keep one of them (`twr box remove {name}` \
                     removes it from {}; it works with the files as they are)",
                    config_file.display(),
                    boxes_file.display(),
                    boxes_file.display()
                );
            }
            config.boxes.insert(name, b);
        }
        // `config_file` alone passed: what fails now came from `boxes.toml`.
        config
            .validate()
            .with_context(|| format!("in {}", boxes_file.display()))?;
        Ok(config)
    }

    /// Loads the config file from `paths` with its `boxes.toml` ([`Config::load_with_boxes`]),
    /// as its workspace sees it ([`Config::in_workspace`]).
    pub fn load_from(paths: &Paths) -> anyhow::Result<Config> {
        Ok(Self::load_with_boxes(&paths.config_file)?.in_workspace(paths))
    }

    /// The config as the workspace of `paths` sees it. Every workspace shares the config file;
    /// a named one ignores what belongs to the `default` workspace's hub: `hub.socket` (it
    /// has its own socket), `hub.listen` and `hub.tls` (runners connect to `default`), and
    /// `orchestrator.cwd` (its orchestrator works in the workspace's own directory, so a Claude
    /// Code orchestrator keeps its own memories).
    pub fn in_workspace(mut self, paths: &Paths) -> Config {
        if let Some(dir) = paths.orchestrator_dir() {
            self.hub.socket = None;
            self.hub.listen = None;
            self.hub.tls = None;
            self.orchestrator.cwd = Some(dir);
        }
        self
    }

    /// Socket path: `hub.socket` if set (only in the `default` workspace), else the resolved
    /// default.
    pub fn socket_path(&self, paths: &Paths) -> PathBuf {
        match &self.hub.socket {
            Some(s) if paths.workspace.is_none() => s.clone(),
            _ => paths.socket.clone(),
        }
    }

    /// The environment variables a worker thread of `project` runs with: `threads.env`, each
    /// overridden by `[projects.<project>] env` (which adds its own), in name order.
    pub fn thread_env(&self, project: Option<&str>) -> Vec<(String, String)> {
        let mut env = self.threads.env.clone();
        if let Some(p) = project.and_then(|p| self.projects.get(p)) {
            env.extend(p.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        env.into_iter().collect()
    }

    /// What a Codex thread of `project` in `auto` does with a command its sandbox does not
    /// allow: `[projects.<project>] codex_auto`, else `[codex] auto`.
    pub fn codex_auto(&self, project: Option<&str>) -> CodexAuto {
        project
            .and_then(|p| self.projects.get(p))
            .and_then(|p| p.codex_auto)
            .unwrap_or(self.codex.auto)
    }

    /// What an idle trim of a thread of `project` may delete (of `threads.park_disposable`):
    /// `[projects.<project>] idle_trim_disposable`, else `threads.idle_trim_disposable`.
    pub fn idle_trim_disposable(&self, project: Option<&str>) -> &[String] {
        project
            .and_then(|p| self.projects.get(p))
            .and_then(|p| p.idle_trim_disposable.as_deref())
            .unwrap_or(&self.threads.idle_trim_disposable)
    }

    /// Whether a new thread of `project` gets a twr-managed worktree unless `thread.new` says
    /// otherwise: `[projects.<project>] worktree`, else `threads.worktree`.
    pub fn project_worktree(&self, project: &str) -> bool {
        self.projects
            .get(project)
            .and_then(|p| p.worktree)
            .unwrap_or(self.threads.worktree)
    }

    /// The orchestrator's runtime.
    pub fn orchestrator_runtime(&self) -> &str {
        self.orchestrator
            .runtime
            .as_deref()
            .unwrap_or(&self.default_runtime)
    }

    /// The account pin of a thread of `project` on `box_name` running `runtime`, and the
    /// accounts it must not use: (pin, exclusions). The pin is the box project's
    /// `accounts.<runtime>`, else its `account`, else the project's `accounts.<runtime>`,
    /// else its `account`; the exclusions are the box project's `exclude_accounts`, else the
    /// project's. `(None, None)`: nothing pins the thread.
    pub fn account_pin(
        &self,
        project: Option<&str>,
        box_name: Option<&str>,
        runtime: &str,
    ) -> (Option<Pin>, Option<Pin>) {
        let Some(project) = project else {
            return (None, None);
        };
        let p = self.projects.get(project);
        let bp = box_name
            .and_then(|b| self.boxes.get(b))
            .and_then(|b| b.projects.get(project));
        let pin = bp
            .and_then(|b| b.accounts.get(runtime).or(b.account.as_ref()))
            .or_else(|| p.and_then(|p| p.accounts.get(runtime).or(p.account.as_ref())))
            .cloned();
        let exclude = bp
            .and_then(|b| b.exclude_accounts.as_ref())
            .or_else(|| p.and_then(|p| p.exclude_accounts.as_ref()))
            .cloned();
        (pin, exclude)
    }

    /// The first orchestrator's account pin: `orchestrator.account` (`None`: the policy's
    /// default, else the native account).
    pub fn orchestrator_account(&self) -> Option<&Pin> {
        self.orchestrator.account.as_ref()
    }

    /// The orchestrator's model: `orchestrator.model`, else `threads.model` (`None`: the
    /// runtime's default).
    pub fn orchestrator_model(&self) -> Option<&str> {
        self.orchestrator
            .model
            .as_deref()
            .or(self.threads.model.as_deref())
            .filter(|m| !m.trim().is_empty())
    }
}

/// `boxes.toml`, next to `config.toml`: the boxes twr added itself (`twr box add`, `twr box
/// remove`, `twr box install`, `twr box project`), kept apart so that twr never rewrites the user's own file.
/// It holds `[boxes.<name>]` tables and nothing else; [`Config::load_with_boxes`] merges them
/// into the config's `boxes`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoxesFile {
    /// The boxes, by name.
    pub boxes: BTreeMap<String, BoxConfig>,
}

/// What [`BoxesFile::save`] writes at the top of `boxes.toml` (comments in the file are not
/// kept: twr rewrites it whole).
const BOXES_FILE_HEADER: &str = "\
# Written by twr: `twr box add`, `twr box remove`, `twr box install`, `twr box project` and
# `twr box token --rotate` change this file, and rewrite it whole (comments are not kept). It
# may hold box tokens: keep it private (mode 0600). Configure anything else in config.toml,
# next to it; a box named in both files is an error.
";

impl BoxesFile {
    /// The `boxes.toml` of `config_file`: in the same directory.
    pub fn path_for(config_file: &Path) -> PathBuf {
        config_file.with_file_name("boxes.toml")
    }

    /// Reads `path` (missing = no boxes). Only parsed: [`Config::load_with_boxes`] checks the
    /// boxes together with the config they join.
    pub fn load(path: &Path) -> anyhow::Result<BoxesFile> {
        match std::fs::read_to_string(path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BoxesFile::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Replaces `path` with these boxes, atomically: a reader sees the old file or the new
    /// one, never a part. The new file is written to a temporary file in the same directory
    /// (created exclusively, mode 0600), synced, renamed over `path`, and the directory
    /// synced so the rename survives a crash. The directory is created when missing. Callers
    /// that read, change and save hold [`BoxesFile::lock`] across all three.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let body = toml::to_string(self).context("writing boxes.toml")?;
        Self::save_body(path, &body)
    }

    /// Removes box `name` from `path` without reading the rest as boxes ([`BoxesFile::save`]'s
    /// way, under the caller's [`BoxesFile::lock`]): the file is read as any TOML table, so a
    /// box with settings twr refuses (or that another twr version wrote) can be taken out, and
    /// every other table stays as it was (comments aside). Its table, as found, when it was
    /// there; `None` when not (nothing written). An error when the file is no TOML at all.
    pub fn remove_box(path: &Path, name: &str) -> anyhow::Result<Option<toml::Value>> {
        let text = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let mut table: toml::Table = toml::from_str(&text).with_context(|| {
            format!(
                "parsing {}: it is no TOML file; remove [boxes.{name}] from it by hand",
                path.display()
            )
        })?;
        let removed = table
            .get_mut("boxes")
            .and_then(toml::Value::as_table_mut)
            .and_then(|boxes| boxes.remove(name));
        if removed.is_some() {
            let body = toml::to_string(&table).context("writing boxes.toml")?;
            Self::save_body(path, &body)?;
        }
        Ok(removed)
    }

    /// Whether the TOML file `path` (`config.toml` or `boxes.toml`) has a `[boxes.<name>]`,
    /// read as any TOML table: also when twr cannot load it as its config. `false` when it
    /// cannot be read or is no TOML.
    pub fn names_box(path: &Path, name: &str) -> bool {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| toml::from_str::<toml::Table>(&s).ok())
            .is_some_and(|t| {
                t.get("boxes")
                    .and_then(toml::Value::as_table)
                    .is_some_and(|b| b.contains_key(name))
            })
    }

    /// Replaces `path` with the header and `body`, atomically ([`BoxesFile::save`]).
    fn save_body(path: &Path, body: &str) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d,
            _ => Path::new("."),
        };
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "boxes.toml".into());
        let nonce = crate::runner::new_nonce()?;
        let tmp = dir.join(format!(".{name}.{}.tmp", &nonce[..16]));
        // Removes the temporary file unless it was renamed into place.
        struct Tmp<'a>(&'a Path, bool);
        impl Drop for Tmp<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    let _ = std::fs::remove_file(self.0);
                }
            }
        }
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        let mut guard = Tmp(&tmp, false);
        f.write_all(BOXES_FILE_HEADER.as_bytes())
            .and_then(|()| f.write_all(b"\n"))
            .and_then(|()| f.write_all(body.as_bytes()))
            .and_then(|()| f.sync_all())
            .with_context(|| format!("writing {}", tmp.display()))?;
        drop(f);
        std::fs::rename(&tmp, path)
            .with_context(|| format!("replacing {} with {}", path.display(), tmp.display()))?;
        guard.1 = true;
        std::fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .with_context(|| format!("syncing {}", dir.display()))?;
        Ok(())
    }

    /// Takes the lock of `path` (`<path>.lock`, an exclusive `flock`; the file is created
    /// with mode 0600, and its directory when missing), waiting for whoever holds it: twr
    /// commands that change `boxes.toml` hold it from reading the file to saving it, so two
    /// at once cannot lose one's change. Advisory: only those commands take it. Released when
    /// the [`BoxesLock`] drops.
    pub fn lock(path: &Path) -> anyhow::Result<BoxesLock> {
        BoxesLock::take(path, true)?
            .ok_or_else(|| anyhow::anyhow!("the lock of {} was not taken", path.display()))
    }

    /// [`BoxesFile::lock`] without waiting: `None` when someone else holds it.
    pub fn try_lock(path: &Path) -> anyhow::Result<Option<BoxesLock>> {
        BoxesLock::take(path, false)
    }
}

/// The exclusive lock of a `boxes.toml` ([`BoxesFile::lock`]), held until it drops (closing
/// the file releases the `flock`).
#[derive(Debug)]
pub struct BoxesLock {
    _file: std::fs::File,
}

impl BoxesLock {
    /// Opens (creating, 0600) `<path>.lock` and `flock`s it exclusively, waiting when `wait`;
    /// `None` when not waiting and someone else holds it.
    fn take(path: &Path, wait: bool) -> anyhow::Result<Option<BoxesLock>> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::OpenOptionsExt;
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        let lock = path.with_file_name(name);
        if let Some(dir) = lock.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC)
            .open(&lock)
            .with_context(|| format!("opening {}", lock.display()))?;
        let op = if wait {
            libc::LOCK_EX
        } else {
            libc::LOCK_EX | libc::LOCK_NB
        };
        loop {
            // SAFETY: flock on a descriptor this function owns (`file` stays open across the
            // call); no memory is passed.
            if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
                return Ok(Some(BoxesLock { _file: file }));
            }
            let e = std::io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EWOULDBLOCK) if !wait => return Ok(None),
                _ => return Err(e).with_context(|| format!("locking {}", lock.display())),
            }
        }
    }
}

/// `e`, an error loading the config files (`{e:#}`), without what of their text it quotes, for
/// a message that leaves the hub (an RPC's error, a log line): a TOML parse error shows the
/// offending line, which may be a token's (`token = "..."` with a typo), and names a value it
/// refused (`unknown variant `...``). Of such an error, the source excerpt (its `|` gutter
/// lines) is left out, and each quoted or backticked span of
/// [`crate::runner::TOKEN_MIN_LEN`] characters or more (shorter ones are no token the hub
/// takes) is replaced with `…`; where it is (the file, its line and column) and what is wrong
/// stay. twr's own checks of the loaded config ([`Config::validate`]) quote no token and are
/// kept whole.
pub fn error_without_values(e: &anyhow::Error) -> String {
    let full = format!("{e:#}");
    if !full.contains("TOML parse error") {
        return full;
    }
    let gutter = |l: &str| {
        let t = l
            .trim_start()
            .trim_start_matches(|c: char| c.is_ascii_digit());
        t.trim_start().starts_with('|')
    };
    full.lines()
        .filter(|l| !gutter(l))
        .map(redact_long_spans)
        .collect::<Vec<_>>()
        .join("\n")
}

/// `line` with each span between two `"` or two `` ` `` of [`crate::runner::TOKEN_MIN_LEN`]
/// characters or more replaced with `…` ([`error_without_values`]).
fn redact_long_spans(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(start) = rest.find(['"', '`']) {
        let quote = rest[start..].chars().next().unwrap_or('"');
        out.push_str(&rest[..=start]);
        let after = &rest[start + 1..];
        match after.find(quote) {
            Some(end) => {
                let span = &after[..end];
                if span.chars().count() >= crate::runner::TOKEN_MIN_LEN {
                    out.push('…');
                } else {
                    out.push_str(span);
                }
                out.push(quote);
                rest = &after[end + 1..];
            }
            // An unclosed quote: what follows it may be a value too.
            None => {
                if after.chars().count() >= crate::runner::TOKEN_MIN_LEN {
                    out.push('…');
                } else {
                    out.push_str(after);
                }
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An error loading the files that leaves the hub holds no value of theirs: a token on a
    /// line TOML can't parse, one in a refused value, in either file; where and what stay.
    #[test]
    fn config_errors_leave_values_out() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let boxes = BoxesFile::path_for(&config);
        let secret = "s3cretTOKENvalue0123456789";
        let cases = [
            format!("[boxes.lab]\ntoken = \"{secret}\n"),
            format!("[boxes.lab]\ntransport = \"{secret}\"\n"),
            format!("[boxes.lab]\ntoken = '{secret}\n"),
            format!("[boxes.lab]\nssh = 5 # {secret}\n"),
        ];
        for (file, other) in [(&config, &boxes), (&boxes, &config)] {
            for case in &cases {
                std::fs::write(file, case).unwrap();
                std::fs::write(other, "").unwrap();
                let e = Config::load_with_boxes(&config).unwrap_err();
                assert!(
                    format!("{e:#}").contains("s3cret"),
                    "the raw error quotes it: {e:#}"
                );
                let said = error_without_values(&e);
                assert!(!said.contains("s3cret"), "{said}");
                assert!(said.contains("line 2"), "{said}");
                assert!(said.contains(&file.display().to_string()), "{said}");
            }
        }
        assert_eq!(
            redact_long_spans("unknown variant `0123456789abcdef`, expected one of `token`"),
            "unknown variant `…`, expected one of `token`"
        );
        assert_eq!(
            redact_long_spans("box \"gpu\" and \"x"),
            "box \"gpu\" and \"x"
        );
        // twr's own checks, which quote no token, are kept whole.
        std::fs::write(
            &boxes,
            "[boxes.fwd]\ntransport = \"ssh\"\nssh = \"-A me@fwd.example\"\n",
        )
        .unwrap();
        std::fs::write(&config, "").unwrap();
        let e = Config::load_with_boxes(&config).unwrap_err();
        assert!(format!("{e:#}").contains("-A me@fwd.example"), "{e:#}");
        assert_eq!(error_without_values(&e), format!("{e:#}"));
    }

    /// `[threads] cpu_weight` / `memory_high` and `[projects.<name>] trim_idle_after` are
    /// checked when the config loads.
    #[test]
    fn limits_and_idle_trims_are_checked() {
        let c = Config::from_toml_str(
            "[threads]\ncpu_weight = 20\nmemory_high = \"8G\"\n\
             [projects.api]\nrepo = \"/r\"\ntrim_idle_after = \"24h\"\n",
        )
        .unwrap();
        assert_eq!(c.threads.cpu_weight, Some(20));
        assert_eq!(
            c.projects["api"].trim_idle_after(),
            Some(std::time::Duration::from_secs(86400))
        );
        for bad in [
            "[threads]\ncpu_weight = 0\n",
            "[threads]\ncpu_weight = 10001\n",
            "[threads]\nmemory_high = \"8 G\"\n",
            "[threads]\nmemory_high = \"-p\"\n",
            "[threads]\nmemory_high = \"150%\"\n",
            "[projects.api]\nrepo = \"/r\"\ntrim_idle_after = \"soon\"\n",
            "[projects.api]\nrepo = \"/r\"\ntrim_idle_after = \"0h\"\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
        for ok in ["4G", "512M", "1073741824", "20%", "100%", "1T"] {
            assert!(valid_memory_size(ok), "{ok}");
        }
        for bad in [
            "",
            "G",
            "0",
            "0%",
            "101%",
            "4GB",
            "4g",
            "1.5G",
            "=1G",
            "16777216T",
        ] {
            assert!(!valid_memory_size(bad), "{bad}");
        }
        assert_eq!(
            parse_span("90m"),
            Some(std::time::Duration::from_secs(5400))
        );
        assert_eq!(
            parse_span("2d"),
            Some(std::time::Duration::from_secs(172800))
        );
        assert_eq!(parse_span("5s"), Some(std::time::Duration::from_secs(5)));
        for bad in ["", "24", "h", "1w", "-1h", "99999999999d"] {
            assert_eq!(parse_span(bad), None, "{bad}");
        }
    }

    #[test]
    fn isolated_homes_load_only_their_own_config_and_socket() {
        let dir = tempfile::tempdir().unwrap();
        let a = Paths::with_home(&dir.path().join("personal"));
        let b = Paths::with_home(&dir.path().join("work"));
        std::fs::create_dir_all(a.config_file.parent().unwrap()).unwrap();
        std::fs::write(&a.config_file, "default_runtime = \"codex\"\n").unwrap();
        assert_eq!(Config::load_from(&a).unwrap().default_runtime, "codex");
        assert_eq!(
            Config::load_from(&b).unwrap(),
            Config::default(),
            "missing instance config uses defaults"
        );
        std::fs::create_dir_all(b.config_file.parent().unwrap()).unwrap();
        std::fs::write(&b.config_file, "default_runtime = \"grok-build\"\n").unwrap();
        let b_config = Config::load_from(&b).unwrap();
        assert_eq!(b_config.default_runtime, "grok-build");
        assert_eq!(b_config.socket_path(&b), b.socket);
        assert_ne!(
            b_config.socket_path(&b),
            Config::load_from(&a).unwrap().socket_path(&a)
        );
    }

    #[test]
    fn named_workspaces_ignore_the_default_hubs_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = Paths::with_home(dir.path());
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(
            &root.config_file,
            "default_runtime = \"codex\"\n\
             [hub]\nsocket = \"/tmp/custom.sock\"\nlisten = \"127.0.0.1:7878\"\n\
             [hub.tls]\ncert = \"/c.pem\"\nkey = \"/k.pem\"\n\
             [orchestrator]\ncwd = \"/srv\"\n",
        )
        .unwrap();
        let c = Config::load_from(&root).unwrap();
        assert_eq!(c.socket_path(&root), PathBuf::from("/tmp/custom.sock"));
        assert_eq!(c.hub.listen.as_deref(), Some("127.0.0.1:7878"));
        assert!(c.hub.tls.is_some());
        assert_eq!(c.orchestrator.cwd, Some(PathBuf::from("/srv")));

        let w = root.workspace("client-x").unwrap();
        let c = Config::load_from(&w).unwrap();
        assert_eq!(c.default_runtime, "codex", "the rest of the file is shared");
        assert_eq!(c.socket_path(&w), w.socket);
        assert_eq!(c.hub.socket, None);
        assert_eq!(c.hub.listen, None);
        assert_eq!(c.hub.tls, None);
        assert_eq!(c.orchestrator.cwd, Some(w.data_dir.join("orchestrator")));
        // Even a config loaded for the root ignores `hub.socket` for a named workspace.
        let root_config = Config::load_from(&root).unwrap();
        assert_eq!(root_config.socket_path(&w), w.socket);
    }

    #[test]
    fn empty_is_defaults() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.default_runtime, "claude-code");
        assert_eq!(c.orchestrator_runtime(), "claude-code");
        assert!(c.orchestrator_model().is_none());
        assert!(!c.orchestrator.autostart);
        assert!(c.hub.listen.is_none());
        assert!(!c.threads.thinking);
        assert_eq!(c.threads.default_mode, "auto");
        assert!(c.threads.model.is_none());
        assert_eq!(c.stall_after_secs, 600);
        assert!(c.tui.mouse);
        assert!(c.tui.animate);
    }

    /// `[threads] idle_compaction`: `ask` unless set; `before_expiry` and `off` read as such;
    /// anything else is refused, naming the values it may be.
    #[test]
    fn threads_idle_compaction() {
        assert_eq!(
            Config::default().threads.idle_compaction,
            IdleCompaction::Ask
        );
        let c = Config::from_toml_str("[threads]\nthinking = true\n").unwrap();
        assert_eq!(
            c.threads.idle_compaction,
            IdleCompaction::Ask,
            "the default"
        );
        for v in IdleCompaction::ALL {
            let c = Config::from_toml_str(&format!(
                "[threads]\nidle_compaction = \"{}\"\n",
                v.as_str()
            ))
            .unwrap();
            assert_eq!(c.threads.idle_compaction, v);
        }
        for bad in ["\"never\"", "\"Ask\"", "true", "1"] {
            let e = Config::from_toml_str(&format!("[threads]\nidle_compaction = {bad}\n"))
                .unwrap_err();
            let e = format!("{e:#}");
            assert!(e.contains("idle_compaction"), "{e}");
            if bad.starts_with('"') {
                for v in IdleCompaction::ALL {
                    assert!(e.contains(v.as_str()), "{e}");
                }
            }
        }
    }

    #[test]
    fn tui_section() {
        let c = Config::from_toml_str("[tui]\nmouse = false\n").unwrap();
        assert!(!c.tui.mouse);
        assert!(c.tui.animate, "unset keys keep their defaults");
        let c = Config::from_toml_str("[tui]\nanimate = false\n").unwrap();
        assert!(!c.tui.animate);
        assert_eq!(c.tui.card_order, CardOrder::Stable, "the default");
        let c = Config::from_toml_str("[tui]\ncard_order = \"attention\"\n").unwrap();
        assert_eq!(c.tui.card_order, CardOrder::Attention);
        let c = Config::from_toml_str("[tui]\ncard_order = \"stable\"\n").unwrap();
        assert_eq!(c.tui.card_order, CardOrder::Stable);
        assert!(Config::from_toml_str("[tui]\ncard_order = \"recent\"\n").is_err());
        assert!(c.tui.mouse);
        let e = Config::from_toml_str("[tui]\nmice = false\n").unwrap_err();
        assert!(format!("{e:#}").contains("unknown field"), "{e:#}");
        // Suggestions: on, Haiku 5.5 and GPT-6-Luna, unless asked otherwise (an empty `[tui]`
        // too).
        assert!(c.tui.suggest);
        let models = |claude: &str, codex: &str| SuggestModels {
            claude_code: claude.into(),
            codex: codex.into(),
        };
        assert_eq!(
            c.tui.suggest_model,
            models("claude-haiku-5-5", "gpt-6-luna")
        );
        assert!(Config::from_toml_str("[tui]\n").unwrap().tui.suggest);
        // A string names Claude Code's; a table either or both.
        let c =
            Config::from_toml_str("[tui]\nsuggest = false\nsuggest_model = \"haiku\"\n").unwrap();
        assert!(!c.tui.suggest);
        assert_eq!(c.tui.suggest_model, models("haiku", "gpt-6-luna"));
        let c = Config::from_toml_str(
            "[tui]\nsuggest_model = { claude-code = \"claude-haiku-4-5\", codex = \"gpt-5.6-luna\" }\n",
        )
        .unwrap();
        assert_eq!(
            c.tui.suggest_model,
            models("claude-haiku-4-5", "gpt-5.6-luna")
        );
        // Summaries: on, every 5 minutes at most; colors from the environment.
        let c = Config::from_toml_str("[tui]\n").unwrap();
        assert!(c.tui.summaries);
        assert_eq!(c.tui.summary_every(), DEFAULT_SUMMARY_EVERY);
        assert_eq!(c.tui.colors, ColorMode::Auto);
        let c = Config::from_toml_str(
            "[tui]\nsummaries = false\nsummary_every = \"10m\"\ncolors = \"16\"\n",
        )
        .unwrap();
        assert!(!c.tui.summaries);
        assert_eq!(c.tui.summary_every(), std::time::Duration::from_secs(600));
        assert_eq!(c.tui.colors, ColorMode::Basic);
        for (text, mode) in [
            ("none", ColorMode::None),
            ("256", ColorMode::Full),
            ("auto", ColorMode::Auto),
        ] {
            let c = Config::from_toml_str(&format!("[tui]\ncolors = \"{text}\"\n")).unwrap();
            assert_eq!(c.tui.colors, mode);
        }
        assert!(Config::from_toml_str("[tui]\ncolors = \"8\"\n").is_err());
        assert!(Config::from_toml_str("[tui]\nsummary_every = \"30s\"\n").is_err());
        assert!(Config::from_toml_str("[tui]\nsummary_every = \"soon\"\n").is_err());
        let c = Config::from_toml_str("[tui.suggest_model]\ncodex = \"gpt-6-luna\"\n").unwrap();
        assert_eq!(
            c.tui.suggest_model,
            models("claude-haiku-5-5", "gpt-6-luna")
        );
        for bad in [
            "[tui]\nsuggest_model = { grok = \"x\" }\n",
            "[tui]\nsuggest_model = 3\n",
            "[tui]\nsuggest_model = [\"haiku\"]\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }

    /// Only small models answer suggestions: a Haiku for Claude Code, a Luna (or mini, nano)
    /// for Codex. A large one is a warning of the hub's, and is never used.
    #[test]
    fn suggestion_models_must_be_small() {
        for (rt, m) in [
            ("claude-code", "claude-haiku-5-5"),
            ("claude-code", "haiku"),
            ("claude-code", "Claude-Haiku-4-5"),
            ("codex", "gpt-6-luna"),
            ("codex", "gpt-5.6-luna"),
            ("codex", "gpt-5-mini"),
        ] {
            assert_eq!(suggest_model_too_large(rt, m), None, "{rt} {m}");
        }
        for (rt, m) in [
            ("claude-code", "sonnet"),
            ("claude-code", "claude-opus-5-5"),
            ("claude-code", "claude-fable-5-1"),
            ("claude-code", "default"),
            ("codex", "gpt-6.1-sol"),
            ("codex", "gpt-6-astra"),
            ("codex", "gpt-5.6-terra"),
            ("grok-build", "grok-mini"),
        ] {
            let why = suggest_model_too_large(rt, m).unwrap();
            assert!(why.contains("large model") && why.contains(m), "{why}");
        }
        assert!(Config::default().warnings(None).is_empty());
        let c =
            Config::from_toml_str("[tui]\nsuggest_model = { codex = \"gpt-6.1-sol\" }\n").unwrap();
        let w = c.warnings(None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].contains("gpt-6.1-sol") && w[0].contains("codex"),
            "{w:?}"
        );
    }

    #[test]
    fn full_file_parses() {
        let c = Config::from_toml_str(
            r#"
            default_runtime = "codex"
            stall_after_secs = 60
            [orchestrator]
            runtime = "claude-code"
            cwd = "/home/me"
            autostart = true
            model = "haiku"
            [hub]
            socket = "/tmp/x.sock"
            listen = "127.0.0.1:7878"
            tls = { cert = "/etc/c.pem", key = "/etc/k.pem" }
            allow_insecure_remote = true
            [boxes.gpu]
            token = "secret-token-4f7a9c2e"
            ssh = "me@gpu"
            [boxes.gpu.projects.api]
            repo = "/srv/api"
            [projects.api]
            repo = "/src/api"
            box = "gpu"
            [threads]
            thinking = true
            model = "sonnet"
            "#,
        )
        .unwrap();
        assert_eq!(c.default_runtime, "codex");
        assert_eq!(c.orchestrator_runtime(), "claude-code");
        assert_eq!(c.orchestrator.cwd, Some(PathBuf::from("/home/me")));
        assert!(c.orchestrator.autostart);
        assert_eq!(c.orchestrator.model.as_deref(), Some("haiku"));
        assert_eq!(c.orchestrator_model(), Some("haiku"));
        let mut c2 = c.clone();
        c2.orchestrator.model = None;
        assert_eq!(c2.orchestrator_model(), Some("sonnet"));
        assert_eq!(c.hub.listen.as_deref(), Some("127.0.0.1:7878"));
        assert_eq!(
            c.boxes["gpu"].token.as_deref(),
            Some("secret-token-4f7a9c2e")
        );
        assert_eq!(c.boxes["gpu"].ssh.as_deref(), Some("me@gpu"));
        assert_eq!(
            c.boxes["gpu"].projects["api"].repo,
            PathBuf::from("/srv/api")
        );
        let tls = c.hub.tls.clone().unwrap();
        assert_eq!(
            (tls.cert, tls.key),
            (PathBuf::from("/etc/c.pem"), PathBuf::from("/etc/k.pem"))
        );
        assert!(c.hub.allow_insecure_remote);
        assert!(!format!("{:?}", c.boxes["gpu"]).contains("secret-token-4f7a9c2e"));
        assert_eq!(c.projects["api"].repo, PathBuf::from("/src/api"));
        assert_eq!(c.projects["api"].box_name.as_deref(), Some("gpu"));
        assert!(c.threads.thinking);
        assert_eq!(c.threads.default_mode, "auto");
        assert_eq!(c.threads.model.as_deref(), Some("sonnet"));
        assert_eq!(c.stall_after_secs, 60);
        let paths = Paths::with_home(Path::new("/th"));
        assert_eq!(c.socket_path(&paths), PathBuf::from("/tmp/x.sock"));
    }

    #[test]
    fn worktree_default_and_project_override() {
        let c = Config::from_toml_str("").unwrap();
        assert!(c.threads.worktree);
        assert!(c.project_worktree("anything"));
        let c = Config::from_toml_str(
            r#"
            [threads]
            worktree = false
            [projects.api]
            repo = "/src/api"
            worktree = true
            [projects.web]
            repo = "/src/web"
            "#,
        )
        .unwrap();
        assert!(!c.threads.worktree);
        assert!(c.project_worktree("api"));
        assert!(!c.project_worktree("web"));
        assert!(!c.project_worktree("/some/path"));
        let c = Config::from_toml_str("[projects.api]\nrepo = \"/a\"\nworktree = false\n").unwrap();
        assert!(!c.project_worktree("api"));
        assert!(c.project_worktree("other"));
    }

    /// `[codex] auto` (who answers what a Codex `auto` thread's sandbox does not allow):
    /// `auto_review` by default (an empty `[codex]` too), `ask` and `never` on request, a
    /// project's `codex_auto` wins for its threads (`ask` included, under the default), and
    /// anything but the three values (or another key under `[codex]`) is refused.
    #[test]
    fn codex_auto_defaults_project_override_and_checks() {
        for text in ["", "[codex]\n"] {
            let c = Config::from_toml_str(text).unwrap();
            assert_eq!(c.codex.auto, CodexAuto::AutoReview);
            assert_eq!(c.codex_auto(None), CodexAuto::AutoReview);
            assert_eq!(c.codex_auto(Some("api")), CodexAuto::AutoReview);
        }
        assert_eq!(Config::default().codex.auto, CodexAuto::AutoReview);
        // What a thread runs with when nothing names a value stays `ask`.
        assert_eq!(CodexAuto::default(), CodexAuto::Ask);
        let c = Config::from_toml_str(
            r#"
            [projects.api]
            repo = "/src/api"
            codex_auto = "ask"
            [projects.web]
            repo = "/src/web"
            "#,
        )
        .unwrap();
        assert_eq!(c.codex_auto(Some("api")), CodexAuto::Ask);
        assert_eq!(c.codex_auto(Some("web")), CodexAuto::AutoReview);
        let c = Config::from_toml_str("[codex]\nauto = \"ask\"\n").unwrap();
        assert_eq!(c.codex_auto(None), CodexAuto::Ask);
        let c = Config::from_toml_str(
            r#"
            [codex]
            auto = "never"
            [projects.api]
            repo = "/src/api"
            codex_auto = "auto_review"
            [projects.web]
            repo = "/src/web"
            "#,
        )
        .unwrap();
        assert_eq!(c.codex.auto, CodexAuto::Never);
        assert_eq!(c.codex_auto(Some("api")), CodexAuto::AutoReview);
        assert_eq!(c.codex_auto(Some("web")), CodexAuto::Never);
        assert_eq!(c.codex_auto(Some("/some/path")), CodexAuto::Never);
        assert_eq!(c.codex_auto(None), CodexAuto::Never);
        for v in CodexAuto::ALL {
            let c =
                Config::from_toml_str(&format!("[codex]\nauto = \"{}\"\n", v.as_str())).unwrap();
            assert_eq!(c.codex.auto, v);
            assert_eq!(CodexAuto::parse(v.as_str()), Some(v));
        }
        for bad in [
            "[codex]\nauto = \"yes\"\n",
            "[codex]\nauto = \"auto-review\"\n",
            "[codex]\nauto = \"Never\"\n",
            "[codex]\nauto = true\n",
            "[codex]\nmode = \"never\"\n",
            "[projects.api]\nrepo = \"/a\"\ncodex_auto = \"always\"\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
        assert_eq!(CodexAuto::parse("yes"), None);
    }

    #[test]
    fn trash_keep_days_defaults_and_overrides() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.trash.keep_days, DEFAULT_TRASH_KEEP_DAYS);
        let c = Config::from_toml_str("[trash]\nkeep_days = 2").unwrap();
        assert_eq!((c.trash.keep_days, c.trash.keep_ms()), (2, 2 * 86_400_000));
        assert!(Config::from_toml_str("[trash]\nkeep = 2").is_err());
    }

    /// `[log]`: 30 days and an archive by default; a retention under a week is refused (a
    /// remote box's commands are resent for 6 days, reading their turn's events).
    #[test]
    fn log_retention_defaults_floor_and_overrides() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!((c.log.retention_days, c.log.archive), (30, true));
        assert_eq!(c.log.retention().as_secs(), 30 * 86_400);
        c.validate().unwrap();
        let c = Config::from_toml_str("[log]\nretention_days = 7\narchive = false").unwrap();
        assert_eq!((c.log.retention_days, c.log.archive), (7, false));
        c.validate().unwrap();
        assert_eq!(MIN_LOG_RETENTION_DAYS, 7);
        let e = Config::from_toml_str("[log]\nretention_days = 6")
            .unwrap_err()
            .to_string();
        assert!(e.contains("log.retention_days"), "{e}");
        assert!(Config::from_toml_str("[log]\nkeep = 2").is_err());
    }

    #[test]
    fn park_disposable_defaults_and_overrides() {
        let c = Config::default();
        assert_eq!(c.threads.park_disposable, DEFAULT_PARK_DISPOSABLE);
        let c = Config::from_toml_str("[threads]\nmodel = \"opus\"").unwrap();
        assert_eq!(c.threads.park_disposable, DEFAULT_PARK_DISPOSABLE);
        let c = Config::from_toml_str("[threads]\npark_disposable = [\"out/\"]").unwrap();
        assert_eq!(c.threads.park_disposable, ["out/"]);
        let c = Config::from_toml_str("[threads]\npark_disposable = []").unwrap();
        assert!(c.threads.park_disposable.is_empty());
    }

    /// `[threads] idle_trim_disposable` defaults to what a build always makes again, a
    /// project's replaces it for that project, and an entry that can match no name is refused.
    #[test]
    fn idle_trim_disposable_defaults_overrides_and_checks() {
        let c = Config::default();
        assert_eq!(c.threads.idle_trim_disposable, DEFAULT_IDLE_TRIM_DISPOSABLE);
        assert_eq!(c.idle_trim_disposable(None), DEFAULT_IDLE_TRIM_DISPOSABLE);
        // Nothing a build may not make again.
        for kept in [
            "dist/",
            "build/",
            ".cache/",
            ".venv/",
            "venv/",
            "coverage/",
            ".DS_Store",
        ] {
            assert!(
                !c.threads.idle_trim_disposable.iter().any(|e| e == kept),
                "{kept}"
            );
        }
        // Each default entry is build output to a park too.
        for e in DEFAULT_IDLE_TRIM_DISPOSABLE {
            assert!(DEFAULT_PARK_DISPOSABLE.contains(e), "{e}");
        }
        let c = Config::from_toml_str(
            "[threads]\nidle_trim_disposable = [\"target/\", \"out\"]\n\
             [projects.api]\nrepo = \"/r\"\nidle_trim_disposable = []\n\
             [projects.web]\nrepo = \"/w\"\n",
        )
        .unwrap();
        assert_eq!(c.threads.idle_trim_disposable, ["target/", "out"]);
        assert!(c.idle_trim_disposable(Some("api")).is_empty());
        assert_eq!(c.idle_trim_disposable(Some("web")), ["target/", "out"]);
        assert_eq!(c.idle_trim_disposable(Some("other")), ["target/", "out"]);
        assert_eq!(c.idle_trim_disposable(None), ["target/", "out"]);
        for bad in [
            "[threads]\nidle_trim_disposable = [\"\"]\n",
            "[threads]\nidle_trim_disposable = [\"/\"]\n",
            "[threads]\nidle_trim_disposable = [\"a/b/\"]\n",
            "[threads]\nidle_trim_disposable = [\"..\"]\n",
            "[threads]\nidle_trim_disposable = [\"./\"]\n",
            "[threads]\nidle_trim_disposable = \"target/\"\n",
            "[projects.api]\nrepo = \"/r\"\nidle_trim_disposable = [\"x//\"]\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }

    /// `threads.env` for every worker thread, `[projects.<name>] env` over it for that
    /// project's; twr's own `TWR_*` and malformed names are refused.
    #[test]
    fn thread_env_layers_projects_over_threads() {
        let c = Config::from_toml_str(
            "[threads]\nenv = { CARGO_INCREMENTAL = \"0\", A = \"threads\" }\n\
             [projects.api]\nrepo = \"/r\"\nenv = { A = \"api\", CARGO_TARGET_DIR = \"/t\" }\n",
        )
        .unwrap();
        let pairs = |v: &[(&str, &str)]| -> Vec<(String, String)> {
            v.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            c.thread_env(Some("api")),
            pairs(&[
                ("A", "api"),
                ("CARGO_INCREMENTAL", "0"),
                ("CARGO_TARGET_DIR", "/t")
            ])
        );
        let threads = pairs(&[("A", "threads"), ("CARGO_INCREMENTAL", "0")]);
        assert_eq!(c.thread_env(Some("web")), threads);
        assert_eq!(c.thread_env(None), threads);
        assert!(Config::default().thread_env(Some("api")).is_empty());
        for bad in [
            "[threads]\nenv = { TWR_AGENT = \"x\" }",
            "[threads]\nenv = { twr_thread = \"x\" }",
            "[threads]\nenv = { \"A=B\" = \"x\" }",
            "[threads]\nenv = { \"\" = \"x\" }",
            "[projects.api]\nrepo = \"/r\"\nenv = { TWR_RUNTIME_GEN = \"1\" }",
        ] {
            let e = Config::from_toml_str(bad).unwrap_err();
            assert!(format!("{e:#}").contains(".env: "), "{bad}: {e:#}");
        }
    }

    /// The module doc's example is a config that loads.
    #[test]
    fn the_doc_example_loads() {
        let src = include_str!("config.rs");
        let start = src.find("//! ```toml\n").unwrap() + "//! ```toml\n".len();
        let end = start + src[start..].find("//! ```\n").unwrap();
        let toml: String = src[start..end]
            .lines()
            .map(|l| l.strip_prefix("//!").unwrap_or(l))
            .map(|l| l.strip_prefix(' ').unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        let c = Config::from_toml_str(&toml).unwrap();
        assert!(c.accounts["claude-code"]["personal"].native);
        assert_eq!(
            c.account_policy["claude-code"].default.as_deref(),
            Some("personal")
        );
        assert_eq!(c.orchestrator_account(), Some(&Pin::parse("work")));
    }

    #[test]
    fn accounts_pins_and_policy_parse() {
        let c = Config::from_toml_str(
            r#"
[accounts.claude-code.work]
dir = "/home/me/.claude-work"
share = "none"

[accounts.claude-code.personal]
native = true

[accounts.codex.team]

[account_policy.codex]
default = "team"

[orchestrator]
account = "personal"

[projects.api]
repo = "/r/api"
account = "work"
accounts = { codex = ["team", "max-*"] }
exclude_accounts = "personal,old-*"

[projects.blog]
repo = "/r/blog"
exclude_accounts = ["work*"]

[boxes.gpu]
ssh = "me@gpu"
transport = "ssh"

[boxes.gpu.projects.api]
repo = "/srv/api"
account = "gpu-work"
"#,
        )
        .unwrap();
        let work = &c.accounts["claude-code"]["work"];
        assert_eq!(
            (work.dir.as_deref(), work.share, work.native),
            (Some(Path::new("/home/me/.claude-work")), Share::None, false)
        );
        assert_eq!(c.accounts["claude-code"]["personal"].dir, None);
        assert_eq!(c.accounts["codex"]["team"], AccountConfig::default());
        assert_eq!(c.accounts["codex"]["team"].share, Share::Config);
        assert_eq!(c.account_policy["codex"].default.as_deref(), Some("team"));
        assert_eq!(c.orchestrator_account(), Some(&Pin::parse("personal")));
        let pin = |p: &[&str]| Some(Pin(p.iter().map(|s| s.to_string()).collect()));
        // Project: `accounts.<rt>` over `account`; exclusions on every runtime.
        assert_eq!(
            c.account_pin(Some("api"), None, "claude-code"),
            (pin(&["work"]), pin(&["personal", "old-*"]))
        );
        assert_eq!(
            c.account_pin(Some("api"), Some("local"), "codex"),
            (pin(&["team", "max-*"]), pin(&["personal", "old-*"]))
        );
        // The box project's keys replace the project's, its `account` over the project's
        // `accounts.<rt>`; without exclusions of its own, the project's apply.
        assert_eq!(
            c.account_pin(Some("api"), Some("gpu"), "codex"),
            (pin(&["gpu-work"]), pin(&["personal", "old-*"]))
        );
        assert_eq!(
            c.account_pin(Some("blog"), None, "codex"),
            (None, pin(&["work*"]))
        );
        assert_eq!(c.account_pin(Some("nope"), None, "codex"), (None, None));
        assert_eq!(c.account_pin(None, Some("gpu"), "codex"), (None, None));
        // Box projects keep no empty account keys when boxes.toml is written.
        let text = toml::to_string(&c.boxes["gpu"]).unwrap();
        assert!(
            !text.contains("accounts") && !text.contains("exclude"),
            "{text}"
        );
        // Nothing set: no accounts, no pins.
        let d = Config::from_toml_str("").unwrap();
        assert!(d.accounts.is_empty() && d.account_policy.is_empty());
        assert_eq!(d.orchestrator_account(), None);
    }

    /// `[orchestrator] account` takes a pin as a project's `account` does: a name, a glob, a
    /// list, a comma-separated string.
    #[test]
    fn the_orchestrators_account_is_a_pin() {
        for (toml, want) in [
            ("account = \"work\"", vec!["work"]),
            ("account = \"work-*\"", vec!["work-*"]),
            ("account = \"work-a, work-b\"", vec!["work-a", "work-b"]),
            ("account = [\"max-*\", \"work\"]", vec!["max-*", "work"]),
        ] {
            let c = Config::from_toml_str(&format!("[orchestrator]\n{toml}")).unwrap();
            let want = Pin(want.into_iter().map(str::to_string).collect());
            assert_eq!(c.orchestrator_account(), Some(&want), "{toml}");
        }
    }

    #[test]
    fn account_validation_errors() {
        let cases = [
            ("[accounts.gemini.a]\ndir = \"/a\"", "accounts.gemini: \"gemini\" does not take accounts"),
            (
                "[runtimes.gemini]\nacp = \"gemini --acp\"\n[accounts.gemini.a]\ndir = \"/a\"",
                "accounts for [runtimes] presets come in a later version",
            ),
            ("[accounts.codex.\"..\"]\ndir = \"/a\"", "accounts.codex...: \"..\" is not an account name"),
            ("[accounts.codex.\"-x\"]\ndir = \"/a\"", "is not an account name"),
            ("[accounts.codex.a]\nnative = true\ndir = \"/a\"", "accounts.codex.a: a `native = true` account has no `dir`"),
            (
                "[accounts.codex.a]\nnative = true\n[accounts.codex.b]\nnative = true",
                "accounts.codex: a and b are both `native = true`",
            ),
            ("[accounts.codex.a]\ndir = \"rel/dir\"", "accounts.codex.a.dir: rel/dir is not an absolute path"),
            (
                "[accounts.codex.a]\ndir = \"/same\"\n[accounts.claude-code.b]\ndir = \"/same/\"",
                "is also accounts.claude-code.b's directory",
            ),
            ("[accounts.codex.a]\nshare = \"all\"", "unknown variant `all`"),
            ("[accounts.codex.a]\nlogin = \"x\"", "unknown field `login`"),
            ("[account_policy.codex]\nbalance = \"pinned\"", "unknown field `balance`"),
            // The later phases' keys: refused, saying they are not available yet.
            (
                "[account_policy.codex]\nfailover = \"auto\"",
                "`failover` is planned for a later version of account support and is not available yet",
            ),
            (
                "[account_policy.claude-code]\nresume_at_reset = true",
                "`resume_at_reset` is planned for a later version",
            ),
            ("[account_policy.codex]\nmove = \"resume\"", "`move` is planned for a later version"),
            (
                "[accounts.codex.a]\nsessions = \"shared\"",
                "`sessions` is planned for a later version",
            ),
            ("[account_policy.gemini]\ndefault = \"a\"", "account_policy.gemini: \"gemini\" does not take accounts"),
            ("[account_policy.codex]\ndefault = \"a b\"", "account_policy.codex.default: \"a b\" is not an account name"),
            ("[orchestrator]\naccount = \"a/b\"", "orchestrator.account: \"a/b\" is not an account name"),
            ("[orchestrator]\naccount = []", "orchestrator.account: a pin names at least one account"),
            ("[orchestrator]\naccount = \" , \"", "orchestrator.account: a pin names at least one account"),
            ("[orchestrator]\naccount = [\"ok\", \"..\"]", "orchestrator.account: \"..\""),
            ("[projects.p]\nrepo = \"/r\"\naccount = \"a/b\"", "projects.p.account: \"a/b\" is not an account name or glob"),
            ("[projects.p]\nrepo = \"/r\"\naccount = []", "projects.p.account: a pin names at least one account"),
            ("[projects.p]\nrepo = \"/r\"\naccounts = { pty = \"a\" }", "projects.p.accounts.pty: \"pty\" does not take accounts"),
            ("[projects.p]\nrepo = \"/r\"\naccounts = { codex = \"..\" }", "projects.p.accounts.codex: \"..\""),
            ("[projects.p]\nrepo = \"/r\"\nexclude_accounts = [\"-x\"]", "projects.p.exclude_accounts: \"-x\""),
            (
                "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\n[boxes.gpu.projects.p]\nrepo = \"/r\"\naccount = \"a:b\"",
                "boxes.gpu.projects.p.account: \"a:b\"",
            ),
            // An account part where it would pin every thread it reaches.
            (
                "default_runtime = \"claude-code:work\"",
                "default_runtime: \"claude-code:work\" names an account: write the runtime \
                 alone (\"claude-code\") and the account as [account_policy.claude-code] \
                 default = \"work\"",
            ),
            (
                "[projects.p]\nrepo = \"/r\"\nruntime = \"codex:team\"",
                "projects.p.runtime: \"codex:team\" names an account: write the runtime alone \
                 (\"codex\") and the account as this project's pin (projects.p.accounts.codex \
                 = \"team\") or as [account_policy.codex] default = \"team\"",
            ),
            (
                "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\n[boxes.gpu.projects.p]\nrepo = \"/r\"\nruntime = \"codex:team\"",
                "boxes.gpu.projects.p.runtime: \"codex:team\" names an account",
            ),
        ];
        for (toml, want) in cases {
            let e = format!("{:#}", Config::from_toml_str(toml).unwrap_err());
            assert!(e.contains(want), "{toml}\n=> {e}\nwanted {want}");
        }
        // Allowed: `default` named while another account is the native one, globs, a
        // native account without a dir, a named one without a dir (twr's default).
        // `default` named with no native account in config too: a record may name the native
        // one (`twr account name-native`), so that is the hub's start's check, with records
        // (`AccountsView::check_config_default`).
        Config::from_toml_str("[accounts.codex.default]\ndir = \"/a\"").unwrap();
        Config::from_toml_str(
            "[accounts.codex.default]\ndir = \"/d\"\n[accounts.codex.me]\nnative = true\n\
             [accounts.codex.team]\n[projects.p]\nrepo = \"/r\"\naccount = [\"*\", \"w?rk\"]",
        )
        .unwrap();
        // The orchestrator's runtime takes an account part; ACP and pty specs are no accounts.
        let c = Config::from_toml_str(
            "default_runtime = \"acp:gemini --acp\"\n[orchestrator]\nruntime = \
             \"claude-code:work\"\n[projects.p]\nrepo = \"/r\"\nruntime = \"pty:bash -l\"",
        )
        .unwrap();
        assert_eq!(c.orchestrator.runtime.as_deref(), Some("claude-code:work"));
    }

    #[test]
    fn account_dirs_expand_home() {
        let mut c: Config =
            toml::from_str("[accounts.codex.a]\ndir = \"~/.codex-a\"\n[accounts.codex.b]\ndir = \"/b\"\n[accounts.codex.c]\n")
                .unwrap();
        let unexpanded = c.clone();
        c.expand_account_dirs(Some(Path::new("/home/me")));
        assert_eq!(
            c.accounts["codex"]["a"].dir.as_deref(),
            Some(Path::new("/home/me/.codex-a"))
        );
        assert_eq!(
            c.accounts["codex"]["b"].dir.as_deref(),
            Some(Path::new("/b"))
        );
        assert_eq!(c.accounts["codex"]["c"].dir, None);
        c.validate().unwrap();
        // Without a home, `~` stays and is refused.
        let mut u = unexpanded;
        u.expand_account_dirs(None);
        assert_eq!(
            u.accounts["codex"]["a"].dir.as_deref(),
            Some(Path::new("~/.codex-a"))
        );
        let e = format!("{:#}", u.validate().unwrap_err());
        assert!(
            e.contains("accounts.codex.a.dir: ~/.codex-a is not an absolute path"),
            "{e}"
        );
        // Loading expands with $HOME.
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_absolute()) {
            let c = Config::from_toml_str("[accounts.codex.a]\ndir = \"~/x\"\n").unwrap();
            assert_eq!(
                c.accounts["codex"]["a"].dir.as_deref(),
                Some(Path::new(&home).join("x").as_path())
            );
        }
    }

    #[test]
    fn project_roots_default_explicit_and_off() {
        let home = Path::new("/home/me");
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.project_roots, None);
        let roots = c.project_roots(Some(home));
        assert_eq!(roots.len(), DEFAULT_PROJECT_ROOTS.len());
        assert_eq!(roots[0], PathBuf::from("/home/me/code"));
        assert!(roots.iter().all(|r| r.starts_with(home)), "{roots:?}");
        // No home: nothing under ~ can be searched.
        assert!(c.project_roots(None).is_empty());

        let c =
            Config::from_toml_str("project_roots = [\"~\", \"~/w/src\", \"/srv/git\"]\n").unwrap();
        assert_eq!(
            c.project_roots(Some(home)),
            [
                PathBuf::from("/home/me"),
                PathBuf::from("/home/me/w/src"),
                PathBuf::from("/srv/git")
            ]
        );
        assert_eq!(c.project_roots(None), [PathBuf::from("/srv/git")]);

        // An empty list turns the lookup off.
        let c = Config::from_toml_str("project_roots = []\n").unwrap();
        assert_eq!(c.project_roots, Some(vec![]));
        assert!(c.project_roots(Some(home)).is_empty());

        for bad in [
            "project_roots = [\"code\"]",
            "project_roots = [\"~me/code\"]",
            "project_roots = \"~/code\"",
            "[threads]\nproject_roots = [\"~/code\"]",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
        let e = Config::from_toml_str("project_roots = [\"./code\"]").unwrap_err();
        assert!(format!("{e:#}").contains("project_roots"), "{e:#}");
    }

    #[test]
    fn expand_home_only_touches_a_leading_tilde() {
        let h = Some(Path::new("/h"));
        assert_eq!(expand_home(Path::new("~"), h), Some(PathBuf::from("/h")));
        assert_eq!(
            expand_home(Path::new("~/a"), h),
            Some(PathBuf::from("/h/a"))
        );
        assert_eq!(
            expand_home(Path::new("/a/~"), h),
            Some(PathBuf::from("/a/~"))
        );
        assert_eq!(expand_home(Path::new("~x"), h), Some(PathBuf::from("~x")));
        assert_eq!(expand_home(Path::new("~/a"), None), None);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(Config::from_toml_str("defualt_runtime = \"x\"").is_err());
        assert!(Config::from_toml_str("[nope]\nkey = \"x\"").is_err());
    }

    #[test]
    fn missing_file_is_defaults_and_bad_file_errors() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let c = Config::load_from(&paths).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.socket_path(&paths), dir.path().join("hub.sock"));

        std::fs::write(&paths.config_file, "stall_after_secs = 5\n").unwrap();
        assert_eq!(Config::load_from(&paths).unwrap().stall_after_secs, 5);

        std::fs::write(&paths.config_file, "stall_after_secs = \"soon\"\n").unwrap();
        assert!(Config::load_from(&paths).is_err());
    }

    #[test]
    fn runtime_presets() {
        let c = Config::from_toml_str(
            "default_runtime = \"gemini\"\n[runtimes.gemini]\nacp = \"gemini --experimental-acp\"\nattach = \"gemini --resume {session}\"\n[runtimes.oc]\nacp = \"opencode acp\"",
        )
        .unwrap();
        assert_eq!(c.runtimes["gemini"].acp, "gemini --experimental-acp");
        assert_eq!(
            c.runtimes["gemini"].attach.as_deref(),
            Some("gemini --resume {session}")
        );
        assert_eq!(c.runtimes["oc"].attach, None);
        for bad in [
            "[runtimes.codex]\nacp = \"x\"",
            "[runtimes.\"a b\"]\nacp = \"x\"",
            "[runtimes.g]\nacp = \"  \"",
            "[runtimes.g]\nacp = \"g\"\nattach = \"g --resume\"",
            "[runtimes.g]\nacp = \"g\"\nmodel = \"x\"",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn secrets_are_redacted_in_debug() {
        let c = Config::from_toml_str("[boxes.b]\ntoken = \"tok-9-abcdef012345\"").unwrap();
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("tok-9-abcdef012345"));
        assert!(dbg.contains("<redacted>"));
    }

    #[test]
    fn box_tokens_have_a_minimum_length() {
        let toml = |t: &str| format!("[boxes.gpu]\ntoken = \"{t}\"\n");
        // A token that is too short still loads (every twr command, `twr hub stop` among
        // them, reads the config); the hub refuses to start with it.
        let c = Config::from_toml_str(&toml("abcdefghijklmno")).unwrap();
        let e = format!("{:#}", c.check_box_tokens().unwrap_err());
        assert!(
            e.contains("boxes.gpu.token is too short (15 characters"),
            "{e}"
        );
        assert!(!e.contains("abcdefghijklmno"), "{e}");
        // A box without a token is fine (it cannot connect).
        let c = Config::from_toml_str("[boxes.gpu]\nssh = \"me@gpu\"\n").unwrap();
        assert!(c.warnings(None).is_empty());
        // 16 to 31 characters: loaded, with a warning; 32 and more: no warning.
        let c = Config::from_toml_str(&toml(&"k".repeat(16))).unwrap();
        c.check_box_tokens().unwrap();
        let w = c.warnings(None);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(
            w[0].starts_with("boxes.gpu.token is short (16 characters"),
            "{w:?}"
        );
        let c = Config::from_toml_str(&toml(&"k".repeat(32))).unwrap();
        assert!(c.warnings(None).is_empty());
    }

    #[test]
    fn box_transports_and_builds_are_checked() {
        let c = Config::from_toml_str(
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\nbuild = \"0123456789abcdef\"\n\
             [boxes.lab]\ntoken = \"0123456789abcdef0123\"\n\
             [boxes.cpu]\ntransport = \"token\"\n",
        )
        .unwrap();
        assert_eq!(c.boxes["gpu"].transport(), BoxTransport::Ssh);
        assert_eq!(c.boxes["gpu"].build.as_deref(), Some("0123456789abcdef"));
        assert_eq!(c.boxes["lab"].transport, None);
        assert_eq!(
            c.boxes["lab"].transport(),
            BoxTransport::Token,
            "the default"
        );
        assert_eq!(c.boxes["cpu"].transport(), BoxTransport::Token);
        let dbg = format!("{:?}", c.boxes["gpu"]);
        assert!(
            dbg.contains("Ssh") && dbg.contains("0123456789abcdef"),
            "{dbg}"
        );

        let ssh = "[boxes.gpu]\ntransport = \"ssh\"\n";
        for (bad, says) in [
            ("[boxes.gpu]\ntransport = \"tcp\"\n", "unknown variant"),
            (
                "[boxes.gpu]\ntransport = \"SSH\"\nssh = \"me@gpu\"\n",
                "unknown variant",
            ),
            (
                ssh,
                "boxes.gpu: an SSH box (transport = \"ssh\") needs `ssh`",
            ),
            (&format!("{ssh}ssh = \" \"\n"), "needs `ssh`"),
            (
                &format!("{ssh}ssh = \"me@gpu\"\ntoken = \"0123456789abcdef0123\"\n"),
                "boxes.gpu.token: an SSH box has no token, the hub connects to it over ssh",
            ),
            (
                "[boxes.gpu]\ntoken = \"0123456789abcdef0123\"\nbuild = \"0123456789abcdef\"\n",
                "boxes.gpu.build: only a box the hub connects to (transport ssh or command)",
            ),
            (
                &format!("{ssh}ssh = \"me@gpu\"\nbuild = \"0123456789ABCDEF\"\n"),
                "boxes.gpu.build: \"0123456789ABCDEF\" is not a build id",
            ),
            (
                &format!("{ssh}ssh = \"me@gpu\"\nbuild = \"0123\"\n"),
                "is not a build id",
            ),
        ] {
            let e = format!("{:#}", Config::from_toml_str(bad).unwrap_err());
            assert!(e.contains(says), "{bad}: {e}");
        }
    }

    /// A command box: its `command` (and optional `terminal_command`) an argv of non-empty
    /// words without NULs, no ssh-like program as `command`, no token or `ssh`; neither key
    /// anywhere else; a `build` like an SSH box's. Both argvs are part of how the hub reaches
    /// it, Debug shows neither, and boxes.toml keeps them as arrays.
    #[test]
    fn command_boxes_are_checked() {
        let cmd = "[boxes.sb]\ntransport = \"command\"\n";
        let c = Config::from_toml_str(&format!(
            "{cmd}command = [\"docker\", \"exec\", \"-i\", \"-e\", \"KEY=s3cret\", \"ctr\"]\n\
             terminal_command = [\"docker\", \"exec\", \"-i\", \"-t\", \"ctr\"]\n\
             build = \"0123456789abcdef\"\n"
        ))
        .unwrap();
        let b = &c.boxes["sb"];
        assert_eq!(b.transport(), BoxTransport::Command);
        assert!(BoxTransport::Command.dialed() && BoxTransport::Ssh.dialed());
        assert!(!BoxTransport::Token.dialed());
        assert_eq!(b.command.as_ref().unwrap()[4], "KEY=s3cret");
        // How the hub reaches it: both argvs, not the build.
        let conn = b.connection();
        assert_eq!(conn.command, b.command);
        assert_eq!(conn.terminal_command, b.terminal_command);
        assert_eq!(conn.build, None);
        let dbg = format!("{b:?}");
        assert!(!dbg.contains("s3cret") && !dbg.contains("ctr"), "{dbg}");
        assert!(
            dbg.contains("docker (6 words)") && dbg.contains("docker (5 words)"),
            "{dbg}"
        );

        for (bad, says) in [
            (
                cmd.to_string(),
                "boxes.sb: a command box (transport = \"command\") needs `command`",
            ),
            (
                format!("{cmd}command = []\n"),
                "boxes.sb.command: it is empty",
            ),
            (
                format!("{cmd}command = [\"docker\", \"\"]\n"),
                "boxes.sb.command: word 2 is empty",
            ),
            (
                format!("{cmd}command = [\"docker\", \"a\\u0000b\"]\n"),
                "boxes.sb.command: word 2 contains a NUL byte",
            ),
            (
                format!("{cmd}command = [\"docker\"]\nterminal_command = [\"\"]\n"),
                "boxes.sb.terminal_command: word 1 is empty",
            ),
            (
                format!("{cmd}command = [\"docker\"]\ntoken = \"0123456789abcdef0123\"\n"),
                "boxes.sb.token: a command box has no token",
            ),
            (
                format!("{cmd}command = [\"docker\"]\nssh = \"me@sb\"\n"),
                "boxes.sb.ssh: a command box has no ssh destination",
            ),
            (
                "[boxes.sb]\ntransport = \"ssh\"\nssh = \"me@sb\"\nterminal_command = [\"x\"]\n"
                    .to_string(),
                "boxes.sb.terminal_command: only a command box (transport = \"command\") has one",
            ),
            (
                "[boxes.sb]\ntoken = \"0123456789abcdef0123\"\nterminal_command = [\"x\"]\n"
                    .to_string(),
                "boxes.sb.terminal_command: only a command box",
            ),
            (
                "[boxes.sb]\ntransport = \"ssh\"\nssh = \"me@sb\"\ncommand = [\"x\"]\n".to_string(),
                "boxes.sb.command: only a command box",
            ),
            (
                format!("{cmd}command = [\"/usr/bin/ssh\", \"me@sb\"]\n"),
                "boxes.sb.command: `ssh` passes its arguments to a shell on the box: use \
                 transport = \"ssh\"",
            ),
            (
                format!("{cmd}command = [\"tailscale\", \"ssh\", \"me@sb\"]\n"),
                "boxes.sb.command: `tailscale ssh` passes its arguments to a shell on the box, \
                 as ssh does: use transport = \"ssh\" (a Tailscale SSH host and a `gcloud \
                 compute config-ssh` host work with plain ssh)",
            ),
            (
                format!("{cmd}command = [\"gcloud\", \"--quiet\", \"compute\", \"ssh\", \"vm\"]\n"),
                "boxes.sb.command: `gcloud compute ssh` passes its arguments",
            ),
            (
                "[boxes.sb]\ntoken = \"0123456789abcdef0123\"\nbuild = \"0123456789abcdef\"\n"
                    .to_string(),
                "boxes.sb.build: only a box the hub connects to",
            ),
        ] {
            let e = format!("{:#}", Config::from_toml_str(&bad).unwrap_err());
            assert!(e.contains(says), "{bad}: {e}");
            assert!(!e.contains("s3cret"), "{e}");
        }
        // An ssh-like terminal command is refused too: its remote shell would split the line.
        let e = format!(
            "{:#}",
            Config::from_toml_str(&format!(
                "{cmd}command = [\"docker\", \"exec\", \"-i\", \"c\"]\n\
                 terminal_command = [\"tailscale\", \"ssh\", \"me@sb\"]\n"
            ))
            .unwrap_err()
        );
        assert!(
            e.contains(
                "boxes.sb.terminal_command: `tailscale ssh` passes its arguments to a shell"
            ),
            "{e}"
        );
        assert!(check_command_words(&["ssh".into(), "-t".into()]).is_err());

        // boxes.toml keeps both argvs as arrays, word for word.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.toml");
        let mut file = BoxesFile::default();
        file.boxes.insert("sb".into(), b.clone());
        file.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains(
                "command = [\"docker\", \"exec\", \"-i\", \"-e\", \"KEY=s3cret\", \"ctr\"]"
            ),
            "{text}"
        );
        assert_eq!(BoxesFile::load(&path).unwrap(), file);
        let c = Config::load_with_boxes(&path.with_file_name("config.toml")).unwrap();
        assert_eq!(c.boxes, file.boxes);
    }

    /// A box's `ssh` words: options ssh parses as ssh does (letters together, values in the
    /// same word or the next, `-o` keys in any case or quoted), one destination; forwarding,
    /// local commands, shared connections and what decides how ssh runs are refused, in an
    /// SSH box's config and by [`BoxConfig::check_ssh`].
    #[test]
    fn box_ssh_words_cannot_forward_or_change_what_ssh_runs() {
        for ok in [
            "me@gpu",
            "-p 2222 me@gpu",
            "-p2222 -i ~/.ssh/id_gpu -l me gpu",
            "-4 -C -q -v -g -e none -J jump -F ~/.ssh/other me@gpu",
            "-o Port=2222 -oIdentitiesOnly=yes -o ProxyCommand=nc me@gpu",
            "me@gpu -p 2222",
            "-a -x -k me@gpu",
            "-qp 2222 me@gpu",
            "ssh://me@gpu:2222",
        ] {
            assert_eq!(check_ssh_words(ok), Ok(()), "{ok}");
        }
        for (bad, says) in [
            ("-A me@gpu", "`-A` (in `-A`): it forwards your ssh agent"),
            ("-X me@gpu", "X11"),
            ("-Y me@gpu", "X11"),
            ("-qA me@gpu", "`-A` (in `-qA`)"),
            ("-K me@gpu", "GSSAPI"),
            ("-L 80:x:80 me@gpu", "forwards ports"),
            ("-R80:x:80 me@gpu", "forwards ports"),
            ("-D 1080 me@gpu", "forwards ports"),
            ("-W x:22 me@gpu", "instead of running twr"),
            ("-w 0 me@gpu", "tunnel"),
            ("-M me@gpu", "shared"),
            ("-S /tmp/s me@gpu", "shared"),
            ("-O exit me@gpu", "shared"),
            ("-N me@gpu", "no command"),
            ("-s me@gpu", "subsystem"),
            ("-f me@gpu", "background"),
            ("-t me@gpu", "terminal"),
            ("-T me@gpu", "terminal"),
            ("-n me@gpu", "stdin"),
            ("-G me@gpu", "exit"),
            ("-V me@gpu", "exit"),
            ("-E /tmp/log me@gpu", "log"),
            ("-Z me@gpu", "does not know"),
            (
                "-o ForwardAgent=yes me@gpu",
                "`-o ForwardAgent=yes`: it forwards",
            ),
            ("-oforwardagent=yes me@gpu", "ssh agent"),
            ("-q -oFORWARDAGENT=/tmp/sock me@gpu", "ssh agent"),
            ("-o \"ForwardAgent\"=yes me@gpu", "ssh agent"),
            ("-o ForwardAgent yes me@gpu", "ssh agent"),
            ("-o=ControlMaster=yes me@gpu", "shared"),
            ("-o =RemoteCommand=id me@gpu", "what ssh runs"),
            ("-o =SessionType=none me@gpu", "what ssh runs"),
            ("-o==SessionType=none me@gpu", "what ssh runs"),
            ("-o = me@gpu", "cannot tell which ssh option"),
            ("-o 1Port=22 me@gpu", "cannot tell which ssh option"),
            ("-o Port-x=22 me@gpu", "cannot tell which ssh option"),
            ("-o ForwardX11=yes me@gpu", "X11"),
            ("-o ForwardX11Trusted=yes me@gpu", "X11"),
            ("-o GSSAPIDelegateCredentials=yes me@gpu", "GSSAPI"),
            ("-o LocalForward=80:x:80 me@gpu", "forwarding"),
            ("-o RemoteForward=80:x:80 me@gpu", "forwarding"),
            ("-o DynamicForward=1080 me@gpu", "forwarding"),
            ("-o ClearAllForwardings=no me@gpu", "forwarding"),
            ("-o PermitLocalCommand=yes me@gpu", "local command"),
            ("-o LocalCommand=x me@gpu", "local command"),
            ("-o BatchMode=no me@gpu", "prompting"),
            ("-o StdinNull=yes me@gpu", "what ssh runs"),
            ("-o RequestTTY=force me@gpu", "what ssh runs"),
            ("-o RemoteCommand=x me@gpu", "what ssh runs"),
            ("-o SessionType=none me@gpu", "what ssh runs"),
            ("-o ForkAfterAuthentication=yes me@gpu", "what ssh runs"),
            ("-o ControlMaster=auto me@gpu", "shared"),
            ("-o ControlPath=/tmp/c me@gpu", "shared"),
            ("-o ControlPersist=1h me@gpu", "shared"),
            ("-o Tunnel=yes me@gpu", "tunnel"),
            ("-o TunnelDevice=0:0 me@gpu", "tunnel"),
            ("-o Include=/tmp/x me@gpu", "other configuration"),
            ("me@gpu -A", "ssh agent"),
            ("me@gpu rm -rf", "`rm` after the destination `me@gpu`"),
            ("-- me@gpu", "`--`"),
            ("me@gpu --", "`--`"),
            ("-p 2222", "no destination"),
            ("-p", "`-p` needs a value"),
        ] {
            let e = check_ssh_words(bad).unwrap_err();
            assert!(e.contains(says), "{bad}: {e}");
        }
        // In an SSH box's config (and `BoxConfig::check_ssh`), named.
        let e = Config::from_toml_str("[boxes.gpu]\ntransport = \"ssh\"\nssh = \"-A me@gpu\"\n")
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("boxes.gpu.ssh = \"-A me@gpu\": `-A`"),
            "{e:#}"
        );
        // A token box's `ssh` (attach only) is the hub's to refuse.
        let c = Config::from_toml_str("[boxes.lab]\nssh = \"-A me@lab\"\n").unwrap();
        let e = c.boxes["lab"].check_ssh("lab").unwrap_err();
        assert!(format!("{e:#}").contains("boxes.lab.ssh"), "{e:#}");
        assert!(BoxConfig::default().check_ssh("x").is_ok());
    }

    /// A box comes out of boxes.toml whatever its settings, the other tables as they were;
    /// a missing file or box is `None`, a file that is no TOML an error.
    #[test]
    fn a_box_is_removed_from_boxes_toml_loosely() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("boxes.toml");
        assert_eq!(BoxesFile::remove_box(&file, "gpu").unwrap(), None);
        std::fs::write(
            &file,
            "[boxes.bad]\ntransport = \"tcp\"\nnot_a_key = 1\n\n[boxes.gpu]\ntransport = \
             \"ssh\"\nssh = \"me@gpu\"\n",
        )
        .unwrap();
        assert!(BoxesFile::load(&file).is_err());
        assert!(BoxesFile::names_box(&file, "bad"));
        let bad = BoxesFile::remove_box(&file, "bad").unwrap().unwrap();
        assert_eq!(
            bad.get("not_a_key").and_then(toml::Value::as_integer),
            Some(1)
        );
        assert!(!BoxesFile::names_box(&file, "bad"));
        let left = BoxesFile::load(&file).unwrap();
        assert_eq!(left.boxes["gpu"].ssh.as_deref(), Some("me@gpu"));
        assert_eq!(BoxesFile::remove_box(&file, "nope").unwrap(), None);
        std::fs::write(&file, "[boxes.gpu\n").unwrap();
        assert!(BoxesFile::remove_box(&file, "gpu").is_err());
        assert!(!BoxesFile::names_box(&file, "gpu"));
    }

    /// `boxes.toml` next to `config.toml`: merged into `[boxes]`, a box in both is an error
    /// naming both files, and the merged config is checked.
    #[test]
    fn boxes_toml_is_merged_into_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        assert_eq!(paths.boxes_file(), dir.path().join("boxes.toml"));
        assert_eq!(paths.boxes_file(), BoxesFile::path_for(&paths.config_file));
        let w = paths.workspace("client-x").unwrap();
        assert_eq!(
            w.boxes_file(),
            paths.boxes_file(),
            "shared like the config file"
        );

        // boxes.toml alone (no config.toml).
        let gpu = "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\n";
        std::fs::write(paths.boxes_file(), gpu).unwrap();
        let c = Config::load_from(&paths).unwrap();
        assert_eq!(c.boxes["gpu"].transport(), BoxTransport::Ssh);
        // Both files.
        std::fs::write(
            &paths.config_file,
            "default_runtime = \"codex\"\n[boxes.lab]\ntoken = \"0123456789abcdef0123\"\n",
        )
        .unwrap();
        let c = Config::load_from(&paths).unwrap();
        assert_eq!(c.default_runtime, "codex");
        assert_eq!(c.boxes.keys().collect::<Vec<_>>(), ["gpu", "lab"]);
        // A named workspace sees them too.
        assert_eq!(Config::load_from(&w).unwrap().boxes.len(), 2);

        // In both files.
        std::fs::write(
            &paths.config_file,
            "[boxes.gpu]\ntoken = \"0123456789abcdef0123\"\n",
        )
        .unwrap();
        let e = format!("{:#}", Config::load_from(&paths).unwrap_err());
        assert!(
            e.contains("box gpu is in both")
                && e.contains(&paths.config_file.display().to_string())
                && e.contains(&paths.boxes_file().display().to_string()),
            "{e}"
        );
        std::fs::remove_file(&paths.config_file).unwrap();

        // A box boxes.toml holds is checked, and the error names the file.
        std::fs::write(paths.boxes_file(), "[boxes.gpu]\ntransport = \"ssh\"\n").unwrap();
        let e = format!("{:#}", Config::load_from(&paths).unwrap_err());
        assert!(e.contains("boxes.toml") && e.contains("needs `ssh`"), "{e}");
        // Anything but boxes, or what does not parse, is refused naming the file.
        for bad in [
            "default_runtime = \"codex\"\n",
            "[boxes.gpu]\nsh = \"me@gpu\"\n",
            "[boxes.gpu\n",
        ] {
            std::fs::write(paths.boxes_file(), bad).unwrap();
            let e = format!("{:#}", Config::load_from(&paths).unwrap_err());
            assert!(e.contains("boxes.toml"), "{bad}: {e}");
        }
        std::fs::remove_file(paths.boxes_file()).unwrap();
        assert_eq!(Config::load_from(&paths).unwrap(), Config::default());
    }

    #[test]
    fn boxes_toml_is_saved_whole_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        // The directory is made when missing.
        let path = dir.path().join("cfg").join("boxes.toml");
        let mut file = BoxesFile::default();
        file.boxes.insert(
            "gpu".into(),
            BoxConfig {
                transport: Some(BoxTransport::Ssh),
                ssh: Some("-p 2222 me@gpu".into()),
                build: Some("0123456789abcdef".into()),
                ..Default::default()
            },
        );
        let mut far = BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some("far".into()),
            ..Default::default()
        };
        far.projects.insert(
            "api".into(),
            BoxProjectConfig {
                repo: "/srv/api".into(),
                runtime: None,
                account: None,
                accounts: BTreeMap::new(),
                exclude_accounts: None,
            },
        );
        file.boxes.insert("far".into(), far);
        file.save(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(BoxesFile::load(&path).unwrap(), file);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# Written by twr"), "{text}");
        assert!(text.contains("config.toml"), "{text}");
        assert!(
            !text.contains("\ntoken") && !text.contains("projects]\n\n"),
            "{text}"
        );
        // It loads as part of the config next to it.
        let c = Config::load_with_boxes(&path.with_file_name("config.toml")).unwrap();
        assert_eq!(c.boxes, file.boxes);

        // A save replaces the file (whatever its mode was) and leaves no temporary file.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        file.boxes.remove("far");
        file.save(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(BoxesFile::load(&path).unwrap(), file);
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["boxes.toml"]);
        // Empty.
        BoxesFile::default().save(&path).unwrap();
        assert_eq!(BoxesFile::load(&path).unwrap(), BoxesFile::default());
        assert_eq!(
            BoxesFile::load(&dir.path().join("missing.toml")).unwrap(),
            BoxesFile::default()
        );
    }

    #[test]
    fn the_boxes_toml_lock_is_exclusive() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boxes.toml");
        let held = BoxesFile::lock(&path).unwrap();
        let lock_file = dir.path().join("boxes.toml.lock");
        let mode = std::fs::metadata(&lock_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Another descriptor, even in this process, does not get it.
        assert!(BoxesFile::try_lock(&path).unwrap().is_none());
        // A waiter gets it once the holder lets go.
        let (tx, rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn({
            let path = path.clone();
            move || {
                let lock = BoxesFile::lock(&path).unwrap();
                tx.send(()).unwrap();
                lock
            }
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the waiter waits"
        );
        drop(held);
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap();
        let theirs = waiter.join().unwrap();
        assert!(BoxesFile::try_lock(&path).unwrap().is_none());
        drop(theirs);
        assert!(BoxesFile::try_lock(&path).unwrap().is_some());
    }

    #[test]
    fn a_config_holding_secrets_must_be_private() {
        let file = Path::new("/home/me/.config/twr/config.toml");
        let token =
            Config::from_toml_str(&format!("[boxes.b]\ntoken = \"{}\"\n", "k".repeat(32))).unwrap();
        let notify =
            Config::from_toml_str("[notify]\ncommand = [\"notify-send\", \"{title}\"]\n").unwrap();
        for c in [&token, &notify] {
            for mode in [0o100644, 0o640, 0o604, 0o620, 0o602, 0o660, 0o777] {
                let w = exposed_config_warning(c, file, mode).unwrap();
                assert!(
                    w.contains("chmod 600 /home/me/.config/twr/config.toml"),
                    "{mode:o}: {w}"
                );
                assert!(w.contains(&format!("(mode {:03o})", mode & 0o777)), "{w}");
            }
            for mode in [0o100600, 0o400, 0o700] {
                assert_eq!(exposed_config_warning(c, file, mode), None, "{mode:o}");
            }
        }
        // Nothing secret: any mode will do.
        let plain = Config::from_toml_str("[boxes.b]\nssh = \"me@b\"\n").unwrap();
        assert_eq!(exposed_config_warning(&plain, file, 0o644), None);
        // The real file's mode.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let w = notify.warnings(Some(&p));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("chmod 600"), "{w:?}");
        // The suggested command works pasted as shown, whatever the path holds.
        let odd = dir.path().join("my twr's $HOME");
        std::fs::create_dir(&odd).unwrap();
        let p = odd.join("config.toml");
        std::fs::write(&p, "").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let w = notify.warnings(Some(&p)).remove(0);
        let cmd = w.split('`').nth(1).unwrap();
        let ran = std::process::Command::new("sh")
            .args(["-c", cmd])
            .status()
            .unwrap();
        assert!(ran.success(), "{cmd}");
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{cmd}");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(notify.warnings(Some(&p)).is_empty());
    }

    /// Each file is judged by its own secrets: a token only in `boxes.toml` does not make a
    /// readable `config.toml` a warning, and a readable `boxes.toml` holding one is, naming
    /// it. A token in `config.toml` still names `config.toml`.
    #[test]
    fn each_config_file_is_judged_by_its_own_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let boxes = BoxesFile::path_for(&config);
        let token = "k".repeat(32);
        let chmod = |p: &Path, mode: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        std::fs::write(&config, "default_runtime = \"fake\"\n").unwrap();
        std::fs::write(&boxes, format!("[boxes.lab]\ntoken = \"{token}\"\n")).unwrap();
        chmod(&config, 0o644);
        chmod(&boxes, 0o600);
        let merged = Config::load_with_boxes(&config).unwrap();
        assert_eq!(merged.warnings(Some(&config)), Vec::<String>::new());

        chmod(&boxes, 0o644);
        let w = merged.warnings(Some(&config));
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].starts_with(&boxes.display().to_string()), "{w:?}");
        assert!(w[0].contains("holds a box token and"), "{w:?}");
        assert!(w[0].contains("(mode 644)"), "{w:?}");

        // A token in config.toml: that file's warning, and boxes.toml's own.
        std::fs::write(&config, format!("[boxes.gpu]\ntoken = \"{token}\"\n")).unwrap();
        let merged = Config::load_with_boxes(&config).unwrap();
        let w = merged.warnings(Some(&config));
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].starts_with(&config.display().to_string()), "{w:?}");
        assert!(w[1].starts_with(&boxes.display().to_string()), "{w:?}");
        chmod(&boxes, 0o600);
        chmod(&config, 0o600);
        assert_eq!(merged.warnings(Some(&config)), Vec::<String>::new());
    }

    #[test]
    fn the_notify_hook_is_off_by_default_and_checked() {
        let c = Config::default();
        assert!(c.notify.command.is_empty());
        assert_eq!(c.notify.events, NotifyEvent::DEFAULTS);
        assert!(!c.notify.detail);
        assert_eq!(c.notify.timeout_secs, 30);
        let c = Config::from_toml_str(
            "[notify]\ncommand = [\"notify-send\", \"{title}\", \"{body}\"]\nevents = [\"permission\"]\n",
        )
        .unwrap();
        assert_eq!(c.notify.command, ["notify-send", "{title}", "{body}"]);
        assert_eq!(c.notify.events, [NotifyEvent::Permission]);
        for bad in [
            "[notify]\ncommand = [\"\"]\n",
            "[notify]\ntimeout_secs = 0\n",
            "[notify]\ntimeout_secs = 601\n",
            "[notify]\nevents = [\"everything\"]\n",
            "[notify]\nurl = \"https://ntfy.sh/x\"\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tui_notify_is_off_by_default_and_checked() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.tui.notify.terminal, TerminalAlert::Off);
        assert_eq!(c.tui.notify.when, NotifyWhen::Unfocused);
        assert_eq!(c.tui.notify.events, NotifyEvent::DEFAULTS);
        let c = Config::from_toml_str(
            "[tui.notify]\nterminal = \"osc777\"\nwhen = \"always\"\nevents = [\"permission\", \"stalled\"]\n",
        )
        .unwrap();
        assert_eq!(c.tui.notify.terminal, TerminalAlert::Osc777);
        assert_eq!(c.tui.notify.when, NotifyWhen::Always);
        assert_eq!(
            c.tui.notify.events,
            [NotifyEvent::Permission, NotifyEvent::Stalled]
        );
        // Keeps `[tui]`'s other keys.
        assert!(c.tui.mouse);
        for bad in [
            "[tui.notify]\nterminal = \"beep\"\n",
            "[tui.notify]\nevents = [\"everything\"]\n",
            "[tui.notify]\nquiet = true\n",
        ] {
            assert!(Config::from_toml_str(bad).is_err(), "{bad}");
        }
    }
}

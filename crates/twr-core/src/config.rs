//! `config.toml`. Every field has a default; a missing file means all defaults.
//!
//! ```toml
//! default_runtime = "claude-code"
//! stall_after_secs = 600  # a running turn silent this long raises `stalled`; 0 = off
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
//!
//! [hub]
//! socket = "/run/user/1000/twr/hub.sock"   # defaults to the resolved path
//! listen = "127.0.0.1:7878"                # runner WebSocket listener; off when unset
//! # A non-loopback `listen` is refused unless one of these is set:
//! tls = { cert = "/etc/twr/hub.pem", key = "/etc/twr/hub.key" }   # serve wss:// (PEM files)
//! allow_insecure_remote = false            # plain ws:// on the network (events in clear text)
//!
//! [boxes.gpu]
//! token = "..."                 # the runner's token (`--token-file` / TWR_RUNNER_TOKEN); never logged
//!                               # or sent (16 characters at least; `twr runner token` prints one)
//! ssh = "me@gpu.lan"            # `twr attach` runs `ssh -t <ssh> -- <command>` for its threads
//!
//! [boxes.gpu.projects.api]      # where project `api` lives on that box
//! repo = "/srv/src/api"
//!
//! [projects.api]
//! repo = "/home/me/src/api"
//! worktree = false     # overrides threads.worktree for this project
//! # Added to the environment of this project's runtimes, over threads.env.
//! env = { CARGO_TARGET_DIR = "/home/me/.cache/api-target" }
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
//!
//! [trash]
//! keep_days = 7        # deleted threads stay in the trash this long (`twr trash empty` sooner)
//!
//! [tui]
//! mouse = true         # the TUI selects and copies text itself; false = the terminal's own
//! animate = true       # the working icon moves; false = a still `twr_` marker
//! card_order = "stable"  # stable (default): cards keep their place; "attention": most recent first
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

use crate::notify::NotifyEvent;
use crate::paths::Paths;

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
    /// Seconds without progress in a turn or background tasks (not waiting on the user) before the hub
    /// raises `stalled`; 0 turns the stall timer off.
    pub stall_after_secs: u64,
    /// Background-only work with no task liveness signal this long is stopped. Zero = off.
    pub background_timeout_secs: u64,
    /// Named runtimes (`[runtimes.<name>]`): ACP agent commands under a short name.
    pub runtimes: BTreeMap<String, RuntimePreset>,
    /// The hub's notification command (`[notify]`): off unless `command` is set.
    pub notify: NotifyConfig,
    /// Terminal UI settings (read by the TUI when it starts, not by the hub).
    pub tui: TuiConfig,
    /// Folders `thread.new` searches for a project given by its repository folder name (a
    /// plain name that is neither a configured project nor an absolute path): `<root>/<name>`
    /// and `<root>/*/<name>`, a git repository. A leading `~` is the home directory. `None`:
    /// [`DEFAULT_PROJECT_ROOTS`]; empty: no lookup.
    pub project_roots: Option<Vec<PathBuf>>,
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
            stall_after_secs: 600,
            background_timeout_secs: 86400,
            runtimes: BTreeMap::new(),
            notify: NotifyConfig::default(),
            tui: TuiConfig::default(),
            project_roots: None,
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
    let mode = mode & 0o777;
    (secrets && mode & 0o077 != 0).then(|| {
        format!(
            "{} holds a box token or a [notify] command and other users can access it (mode \
             {mode:03o}): run `chmod 600 {}`",
            file.display(),
            crate::paths::sh_word(&file.to_string_lossy())
        )
    })
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

/// `[boxes.<name>]`: a remote box whose runner connects to the hub.
#[derive(Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BoxConfig {
    /// Token the box's runner authenticates with (compared in constant time; never logged).
    /// A box without one cannot connect.
    pub token: Option<String>,
    /// `ssh` destination of the box (`user@host`, or a `~/.ssh/config` host; whitespace
    /// separates extra `ssh` arguments placed before it, e.g. `-p 2222 me@host`): `twr attach`
    /// on its threads runs `ssh -t <ssh> -- <command>`.
    pub ssh: Option<String>,
    /// Projects on this box: where each repository lives there.
    pub projects: BTreeMap<String, BoxProjectConfig>,
}

impl fmt::Debug for BoxConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxConfig")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("ssh", &self.ssh)
            .field("projects", &self.projects)
            .finish()
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
}

impl ProjectConfig {
    /// `trim_idle_after` as a duration (checked by [`Config::validate`]).
    pub fn trim_idle_after(&self) -> Option<std::time::Duration> {
        self.trim_idle_after.as_deref().and_then(parse_span)
    }
}

/// A span of time as the config writes it: a whole number and its unit, `s`, `m`, `h` or `d`
/// (`90m`, `24h`, `2d`); `None` for anything else, zero, or more than ten years.
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
        .filter(|v| *v > 0 && *v <= 10 * 365 * 86400)?;
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
        }
    }
}

impl Config {
    /// Parses TOML (no environment overrides) and checks values serde cannot
    /// ([`Config::validate`]).
    pub fn from_toml_str(s: &str) -> anyhow::Result<Config> {
        let cfg: Config = toml::from_str(s).context("parsing twr config")?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks values the types allow but twr does not: runtime presets
    /// ([`RuntimePreset::validate`]), `project_roots` that are neither absolute nor under `~`.
    /// Box tokens that are too short are the hub's to refuse ([`Config::check_box_tokens`]):
    /// every twr command loads the config, and one that is too short must not keep `twr hub
    /// stop` from stopping a hub started before the minimum.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (name, preset) in &self.runtimes {
            preset.validate(name)?;
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

    /// What the hub warns about when it starts with this config, read from `file` (`hub.info`
    /// `warnings`, and its log): box tokens shorter than [`crate::runner::TOKEN_GOOD_LEN`],
    /// and a file holding a box token or a `[notify]` command that other users can access
    /// ([`exposed_config_warning`]). Without a file, or one whose mode cannot be read, only
    /// the config itself is checked.
    pub fn warnings(&self, file: Option<&Path>) -> Vec<String> {
        use std::os::unix::fs::PermissionsExt;
        let file_mode = file.and_then(|f| {
            let mode = std::fs::metadata(f).ok()?.permissions().mode();
            Some((f, mode))
        });
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
        out.extend(file_mode.and_then(|(file, mode)| exposed_config_warning(self, file, mode)));
        out
    }

    /// Loads `path` (missing file = defaults).
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::from_toml_str(&s).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Loads the config file from `paths`, as its workspace sees it ([`Config::in_workspace`]).
    pub fn load_from(paths: &Paths) -> anyhow::Result<Config> {
        Ok(Self::load(&paths.config_file)?.in_workspace(paths))
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn trash_keep_days_defaults_and_overrides() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.trash.keep_days, DEFAULT_TRASH_KEEP_DAYS);
        let c = Config::from_toml_str("[trash]\nkeep_days = 2").unwrap();
        assert_eq!((c.trash.keep_days, c.trash.keep_ms()), (2, 2 * 86_400_000));
        assert!(Config::from_toml_str("[trash]\nkeep = 2").is_err());
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

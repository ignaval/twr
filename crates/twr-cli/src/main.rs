//! The `twr` binary: hub, client subcommands and the TUI launcher.

#[macro_use]
mod term;
mod accounts;
mod boxes;
mod dial_in;
mod pretty;
mod speech;
#[cfg(test)]
mod test_support;

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use twr_core::blob::hydrate_deep;
use twr_core::client::Client;
use twr_core::config::{BoxesFile, Config};
use twr_core::event::{Event, DELETE_KEY, HUB_THREAD};
use twr_core::git::human_bytes;
use twr_core::paths::{check_socket_path, sh_word, Paths, DEFAULT_WORKSPACE, WORKSPACE_ENV};
use twr_core::proto::{codes, RpcError, StreamItem};
use twr_hub::HubOptions;

/// How long bare `twr` waits for a hub it spawned to start listening.
const HUB_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long bare `twr` waits for a hub that accepts connections but does not answer yet (e.g.
/// replaying a large log, or finishing a shutdown).
const HUB_BUSY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long `twr hub stop` waits for the hub to exit by default: well over its shutdown (each
/// runtime's stop grace, the hub's own stop timeout, then draining the connections).
const HUB_STOP_TIMEOUT: Duration = Duration::from_secs(30);

/// One final process-table read at the deadline, so an exit in the last polling interval
/// is observed without letting an unreadable or stalled process check wait indefinitely.
const HUB_STOP_FINAL_CHECK: Duration = Duration::from_millis(100);

/// The host of twr's releases, where `twr update` gets one. It picks the release once
/// (`--version`, else the tag `<releases>/latest` redirects to), downloads [`RELEASE_FILES`]
/// from `<releases>/download/<tag>/`, checks the two signatures against [`RELEASE_KEY`], checks
/// that sha256sums.txt is that tag's and lists that install.sh ([`release_of`]) and runs that
/// install.sh for that tag with that sha256sums.txt.
const GITHUB: &str = "https://github.com";

/// The path of twr's releases (`<releases>`) on [`GITHUB`].
const RELEASES_PATH: &str = "/ignaval/twr/releases";

/// What `twr update` downloads from a release: the installer, the checksums of the archives, and
/// the Ed25519 signature of each (base64, made by the release workflow): `<file>.sig`.
const RELEASE_FILES: [&str; 4] = [
    "install.sh",
    "install.sh.sig",
    "sha256sums.txt",
    "sha256sums.txt.sig",
];

/// The variable that names another installer for `twr update`: a mirror, or a `file:///` path
/// for tests. Its signature is `<url>.sig`; sha256sums.txt, its signature and the archive come
/// from the installer's directory (install.sh gets it as `TWR_DOWNLOAD_URL`), all checked the
/// same way, so whatever the mirror holds is one release. With `--version`, its checksums must
/// name that release.
const INSTALL_SCRIPT_URL_ENV: &str = "TWR_INSTALL_SCRIPT_URL";

/// The public half of the key the releases are signed with (Ed25519, raw): the key in
/// `release/signing-key.pub`, which `docs/tools/make-signing-key.sh` made. A test checks the two
/// agree.
const RELEASE_KEY: [u8; 32] = [
    0xa0, 0x41, 0x42, 0xb0, 0x80, 0xc3, 0x68, 0x94, 0xd2, 0x96, 0x97, 0x2d, 0x78, 0x7d, 0xa9, 0xa7,
    0x76, 0x99, 0xa4, 0xb4, 0x3e, 0x48, 0x1f, 0xb5, 0xf2, 0xc5, 0x1d, 0x64, 0xda, 0x9e, 0xf5, 0xd8,
];

/// A PEM public key `twr update` verifies with instead of [`RELEASE_KEY`]: the tests' key,
/// honoured by debug builds on a test `TWR_HOME` only ([`test_token_trusted`]), never by an
/// installed twr.
const TEST_RELEASE_KEY_ENV: &str = "TWR_TEST_RELEASE_KEY_FILE";

/// Largest file `twr update` downloads (install.sh is about 16 KB, the rest is smaller).
const RELEASE_FILE_MAX: usize = 1 << 20;

/// How long `twr update` waits for each download.
const RELEASE_FILE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one probe waits for `hub.info`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Page size for non-following `events` / `control`.
const PAGE: u64 = 1000;

/// twr: a control tower for parallel coding agents.
#[derive(Debug, Parser)]
#[command(name = "twr", version = twr_core::version(), about)]
struct Cli {
    /// Hub socket (default: `hub.socket` from the config, else the standard location). Not for
    /// `twr runner`, which connects to the hub at `--hub`.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Workspace: a hub of its own on this machine, with its own orchestrator, threads,
    /// worktrees and trash (`twr ws` lists them; `twr ws new <name>` makes one). Default:
    /// `$TWR_WORKSPACE`, else `default`. `--socket` still picks the socket.
    #[arg(long, short = 'w', global = true, value_name = "NAME")]
    workspace: Option<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

// Parsed once per process: `twr runner`'s many hidden flags make its variant the largest.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum Cmd {
    /// Run the hub in the foreground (logs to stderr). `twr hub stop` stops a running hub.
    Hub {
        #[command(subcommand)]
        cmd: Option<HubCmd>,
    },
    /// Update twr in place: download the release's installer and checksums from GitHub (or
    /// from the directory of `$TWR_INSTALL_SCRIPT_URL`, a mirror, with the archive), check
    /// their signatures with the release key built into twr, and run the installer for the
    /// directory this `twr` is in. Nothing runs unless both signatures verify and the checksums
    /// name the release asked for and list the installer. A running hub keeps its version
    /// until you restart it (`twr hub stop`, then `twr`). A `twr` installed by cargo is not
    /// touched: update it with cargo.
    Update {
        /// The release to install, e.g. v0.2.0 (default: the latest).
        #[arg(long, value_name = "VERSION")]
        version: Option<String>,
    },
    /// Show what the running hub reports about itself (`hub.info`): version, boxes,
    /// orchestrator, and warnings about the config it started with (a short box token, a
    /// config file holding secrets that other users can access).
    Info {
        /// Print the raw `hub.info` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show how much is left in each subscription's usage windows (`usage.get`): Claude's
    /// 5-hour and weekly limits, Codex's windows, Grok Build's credit period, as their last
    /// sessions reported them. With --threads, what the threads' turns spent instead
    /// (`usage.threads`): tokens, and the cost where the runtime reports one.
    Usage {
        /// Print the raw JSON (`usage.get`, or `usage.threads` with --threads).
        #[arg(long)]
        json: bool,
        /// What each thread's turns spent: input and output tokens, and the cost where the
        /// runtime reports one (Claude Code: the API price of the tokens, on any plan).
        #[arg(long)]
        threads: bool,
        /// With --threads: only turns that ended in this window, such as 24h, 7d or 30m, or
        /// since a date (2026-10-01) or an RFC 3339 time.
        #[arg(long, requires = "threads")]
        since: Option<String>,
        /// With --threads: group by thread (the default), project or runtime.
        #[arg(long, requires = "threads", value_parser = ["thread", "project", "runtime"])]
        by: Option<String>,
    },
    /// Create and list threads.
    Thread {
        #[command(subcommand)]
        cmd: ThreadCmd,
    },
    /// Show, switch or reset the orchestrator.
    Orchestrator {
        #[command(subcommand)]
        cmd: OrchestratorCmd,
    },
    /// Say something: the hub routes it (a thread named with `@thread` / `on thread,`, the
    /// only thread, `new <project>: ...`, else the orchestrator, else it asks you which
    /// thread). It never answers a permission or question (it cannot tell which ones you had
    /// seen): a message that looks like an answer while one is pending is not sent, and the
    /// pending items are listed; answer those with `twr respond`.
    Say {
        /// The message (words are joined with spaces).
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        message: Vec<String>,
    },
    /// The Claude Code orchestrator's file guard, a `PreToolUse` hook the hub configures: reads
    /// the hook's JSON on stdin and denies Read, Write and Edit outside the session's memory
    /// directory, and whatever it cannot decide within its deadline (a deny decision on stdout
    /// and exit 2).
    #[command(name = "claude-memory-guard", hide = true)]
    ClaudeMemoryGuard {
        /// The one memory directory to allow (the `autoMemoryDirectory` the hub set), instead
        /// of the ones derived from the session's transcript and working directory.
        #[arg(long)]
        memory_dir: Option<PathBuf>,
    },
    /// Serve the twr MCP tools on stdin/stdout for the orchestrator's runtime (logs go to
    /// stderr; stdout is the MCP channel). The hub starts it for the orchestrator with
    /// `TWR_ORCH_TOKEN`; it refuses to start without that token.
    Mcp,
    /// Send a prompt to a thread (queued if a turn is running).
    Send {
        /// Thread id, name, alias or unique prefix.
        #[arg(required_unless_present = "cancel")]
        thread: Option<String>,
        /// The prompt (words are joined with spaces). `{report:<thread>}` in it becomes that
        /// thread's report, for a thread it waits for (`--after`).
        #[arg(
            required_unless_present = "cancel",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        prompt: Vec<String>,
        /// Send it once each of these threads (comma-separated) has finished its next turn,
        /// without an orchestrator turn. If one doesn't finish well, a card asks you.
        #[arg(long, value_delimiter = ',', conflicts_with = "cancel")]
        after: Vec<String>,
        /// Drop a prompt queued with `--after` (its id, or a unique prefix of 4 or more).
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        cancel: Option<String>,
    },
    /// Wait until threads finish or need you, and print how each ended (its report when it
    /// finished). Exit code: 0 finished, 2 waits on you, 3 failed, stopped or parked, 124 timed
    /// out. An agent can run it as a job (`twr job run -- twr wait web`).
    Wait {
        /// Thread ids, names, aliases or unique prefixes.
        #[arg(required = true)]
        threads: Vec<String>,
        /// Wait for every one of them, not just the first.
        #[arg(long)]
        all: bool,
        /// Give up after this long: 90s, 30m, 2h.
        #[arg(long, value_parser = parse_duration_secs)]
        timeout: Option<u64>,
        /// One JSON object per thread.
        #[arg(long)]
        json: bool,
    },
    /// Open the thread's runtime in its own interactive CLI, in this terminal. The thread's
    /// headless runtime is stopped first; prompts sent meanwhile wait until you exit.
    Attach {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Interrupt a running turn, or stop background tasks, instead of refusing.
        #[arg(long)]
        force: bool,
    },
    /// Answer an attention item (a thread's top item when given a thread).
    Respond {
        /// `<attn_id>@<revision>` as `twr attention` lists it, or a thread (id, name, alias,
        /// prefix) to answer its top item: that item is shown first and answered only after
        /// you confirm it (y/N on a terminal; without one, only with --yes). A bare attn_id
        /// answers only an item that was never raised again (it may ask something else now).
        target: String,
        /// An option label (e.g. allow, deny) or its 1-based number; free text for questions
        /// without options.
        answer: String,
        /// Message passed along with the answer.
        #[arg(long, short)]
        message: Option<String>,
        /// With a thread: answer the top item shown without asking (exactly the revision
        /// printed; if it changed meanwhile the answer is refused).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List open attention items (by default the ones blocking a thread: permission, question).
    Attention {
        /// Also list error, review and stalled items.
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        cmd: Option<AttentionCmd>,
    },
    /// Set a thread's permission mode (`auto`, `plan`, `manual`, `acceptEdits`, `read-only`,
    /// `dontAsk`, `bypassPermissions`, or the runtime's own). A running runtime finishes its
    /// turn first, then restarts on the same session in the new mode; an idle one restarts
    /// now; a parked or stopped one starts in it next time. Not for the orchestrator (switch
    /// it with `twr orchestrator use`).
    Mode {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// The permission mode.
        mode: String,
    },
    /// Show or set a thread's runtime, model and reasoning effort. With only the thread, shows
    /// them. The model is the runtime's own id or alias (Claude Code: `fable`, `opus`,
    /// `sonnet`, or a full id such as `claude-fable-5-1`); `default` returns the model or the
    /// effort to the runtime's default (once the thread has a session, only where a resumed
    /// session takes it: Claude Code's effort; the hub refuses the rest, except with
    /// `--runtime`). A running runtime finishes its turn first, then restarts on the same
    /// session with them; an idle one restarts now; a parked or stopped one starts with them
    /// next time. With `--runtime`, the thread moves to that runtime: its current runtime stops
    /// (after the turn it runs), and its next turn starts a new session of the new one, given a
    /// handoff of what the previous session did; a model or effort left out is the new
    /// runtime's default. Moving back to a runtime it left a session of resumes that session
    /// instead, told what the thread did since, with the settings it had (`default` starts a new
    /// one). Not for the orchestrator (switch it with `twr orchestrator use`); a pty thread can
    /// only move to another runtime.
    Model {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// The model (omit to keep it, or with `--runtime` to take the new runtime's default).
        model: Option<String>,
        /// The reasoning effort: Claude Code `low`, `medium`, `high`, `xhigh`, `max`; Codex
        /// `minimal` to `ultra`; Grok Build `low` to `xhigh`; an ACP agent its own.
        #[arg(long)]
        effort: Option<String>,
        /// Move the thread to this runtime (`claude-code`, `codex`, `grok-build`, a configured
        /// `[runtimes.<name>]` preset, or `acp:<command>`; not `pty`): the same thread,
        /// worktree and branch, on a new session (or, back on a runtime and account it left, on
        /// the session it left there). `<runtime>:<account>` (`codex:team`) picks the account too.
        #[arg(long)]
        runtime: Option<String>,
        /// Move the thread to this account of its runtime (`twr account list`; a glob or a
        /// comma-separated list takes the first usable one): a new session there, given a
        /// handoff, keeping the model and effort.
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// Move it although the thread's or its project's pin keeps it off that account
        /// (`--account`, or `--runtime <runtime>:<account>`; on a terminal, a refused move asks
        /// instead).
        #[arg(long)]
        outside_pin: bool,
    },
    /// Rename a worker thread. The name must be free and work as a git branch name, like
    /// `--name`. Its worktree path and branch keep the name they were made with, and its
    /// earlier names stay its own: they still reach it, and no other thread can take them.
    /// Not for the orchestrator.
    Rename {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// The new name.
        name: String,
    },
    /// Stop a thread's runtime and drop its queued prompts.
    Stop {
        /// Thread id, name, alias or unique prefix.
        thread: String,
    },
    /// Park a thread: stop its runtime and remove its worktree if it is clean (its branch is
    /// kept: the one recorded when its worktree was made, `twr/<its name then>`, which a rename
    /// does not change). Prompts sent while parked wait for `twr resume`.
    Park {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Keep the worktree on disk.
        #[arg(long)]
        keep: bool,
        /// Interrupt a running turn, or stop background tasks, instead of refusing.
        #[arg(long)]
        force: bool,
    },
    /// Delete the build output of threads' worktrees (`target/`, `node_modules/` and the rest of
    /// `[threads] park_disposable`, ignored by git and holding no repository), the way deleting
    /// a thread deletes it. Each thread must be idle: no turn or background work, not attached,
    /// parked or resumed. Its next build makes the output again. Linux and macOS only.
    Trim {
        /// Thread ids, names, aliases or unique prefixes.
        #[arg(required = true)]
        threads: Vec<String>,
    },
    /// Resume a parked thread: recreate its worktree if needed (at the same path, on its
    /// recorded branch) and run its queued prompts on the same session.
    Resume {
        /// Thread id, name, alias or unique prefix.
        thread: String,
    },
    /// Compact a thread's context, as its agent's own /compact does: a turn of its own, now or
    /// after its running turn and the messages queued before. Claude Code, Codex and Grok
    /// Build threads on this machine; refused for other runtimes and remote boxes.
    Compact {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Instead of compacting: turn the hub's offers to compact this thread's context once
        /// its prompt cache expired on or off (`[threads] idle_compaction`; a card's "Don't
        /// ask for <thread>" turns them off).
        #[arg(long, value_parser = ["on", "off"])]
        offers: Option<String>,
    },
    /// Delete threads: stop each one's runtime and move its worktree (uncommitted changes too,
    /// or only its record when a park removed it) and its recorded branch (`twr/<its name when
    /// the worktree was made>`: a rename does not change it) to the trash of its box (`twr trash`: purged after `[trash] keep_days`, 7 by default; `twr trash
    /// restore` brings a thread back); parked and stopped threads too. The hub raises one
    /// delete card for them saying what goes, and nothing is deleted until you confirm it: on
    /// a terminal this asks y/N (n answers Cancel; an empty reply leaves the card open);
    /// without one the card waits for `twr respond` or the TUI unless --yes. Once confirmed,
    /// waits until each thread is gone or failed.
    Delete {
        /// Thread ids, names, aliases or unique prefixes (one or more).
        #[arg(required = true, num_args = 1..)]
        threads: Vec<String>,
        /// Confirm the delete card without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List, purge or restore deleted threads in the trash.
    Trash {
        #[command(subcommand)]
        cmd: TrashCmd,
    },
    /// The hub's log: its size, and moving old agent events out of it (`[log]`).
    Log {
        #[command(subcommand)]
        cmd: LogCmd,
    },
    /// Run a command for a thread as a job of the hub: it waits for the machine's job slots,
    /// shows as the thread's background work, and its end (exit code and the end of its
    /// output) comes back to the thread as a prompt. A thread's agent runs jobs for its own
    /// thread.
    Job {
        #[command(subcommand)]
        cmd: JobCmd,
    },
    /// Reading aloud, the hub machine's end (docs/read-aloud.md).
    Speech {
        #[command(subcommand)]
        cmd: SpeechCmd,
    },
    /// A speaker on this machine: reads aloud the orchestrator's messages that this machine's
    /// hubs send, or with `--ssh HOST` those of HOST's hubs, over `ssh HOST twr speech feed`:
    /// every workspace unless `-w` names one. Keys in its terminal: `s` stop, `p` pause or
    /// resume, `n` skip, `q` quit. What it reads is turned on with `/read on` in the TUI
    /// (docs/read-aloud.md).
    Speak {
        /// Read aloud what HOST's hubs send, over ssh (your ssh config and keys).
        #[arg(long, value_name = "HOST")]
        ssh: Option<String>,
        /// Every workspace (of this machine, or of HOST): the default; refused with `-w`.
        #[arg(long)]
        all_workspaces: bool,
        /// The speaker's name in `/read status` and for `/read N --on` (default: this
        /// machine's name).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The engine, over `[speech] engine`: say, avspeech, spd or command.
        #[arg(long, value_name = "ENGINE", value_parser = ["say", "avspeech", "spd", "command"])]
        engine: Option<String>,
        /// The engine's voice, over the one kept by `/read voice` and `[speech] voice`.
        #[arg(long, value_name = "VOICE")]
        voice: Option<String>,
        /// Words per minute (80 to 500), over the rate kept by `/read rate` and `[speech]
        /// rate`.
        #[arg(long, value_name = "WPM", value_parser = clap::value_parser!(u32).range(80..=500))]
        rate: Option<u32>,
        /// Print a LaunchAgent (macOS) or systemd user unit (Linux) that runs this `twr speak`,
        /// and the commands to install it; installs nothing.
        #[arg(long)]
        print_service: bool,
    },
    /// `mosh HOST -- twr ARGS...` (with `--ssh`: `ssh -t HOST -- twr ARGS...`) with `twr speak
    /// --ssh HOST` running beside it, in the background (its output in `speak.log` in twr's
    /// data directory), stopped when the session ends.
    Remote {
        /// The host, as mosh and ssh take it (`me@box`, a `Host` of your ssh config).
        host: String,
        /// Run the session over `ssh -t` rather than mosh.
        #[arg(long)]
        ssh: bool,
        /// The speaker's name (`twr speak --name`).
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
        /// The arguments of the remote `twr`, after `--`.
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
    /// Print a thread's events as NDJSON.
    Events {
        /// Thread id, name, alias or unique prefix.
        thread: String,
        /// Keep streaming new events.
        #[arg(long, short)]
        follow: bool,
        /// Only events with seq greater than N.
        #[arg(long, default_value_t = 0, value_name = "N")]
        since: u64,
        /// Restore payload fields that were moved to the blob store.
        #[arg(long)]
        hydrate: bool,
        /// One human-readable line per event.
        #[arg(long)]
        pretty: bool,
    },
    /// Show or edit a project's standing context (`<repo>/.twr/context.md`), which the hub
    /// prepends to every prompt sent to that project's worker threads.
    Context {
        #[command(subcommand)]
        cmd: ContextCmd,
    },
    /// Show, edit or forget the orchestrator's memory: the notes it keeps across sessions,
    /// restarts and runtimes, one memory per workspace.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Print the hub control stream as NDJSON.
    Control {
        /// Keep streaming new items.
        #[arg(long, short)]
        follow: bool,
        /// Only items with gseq greater than N.
        #[arg(long, default_value_t = 0, value_name = "N")]
        since: u64,
        /// One human-readable line per item.
        #[arg(long)]
        pretty: bool,
    },
    /// Make your terminal send Shift-Enter as Alt-Enter (ESC CR), which the TUI reads as a new
    /// line everywhere, over mosh and SSH too, as Claude Code's /terminal-setup does. It edits
    /// the terminal's settings file when it can do so safely (backing it up), leaves a
    /// Shift-Enter binding already there alone, and otherwise prints the steps. Over SSH or
    /// mosh it changes nothing: it prints what to change on the machine the terminal runs on.
    TerminalSetup {
        /// The terminal: terminal.app, iterm2, ghostty, kitty, wezterm, alacritty, vscode,
        /// vscode-insiders, cursor, windsurf, vscodium or zed. Default: the one this runs in.
        #[arg(long, value_name = "NAME")]
        terminal: Option<String>,
        /// Print the steps and change nothing (every terminal's when none is named or found).
        #[arg(long)]
        print: bool,
    },
    /// List, create or remove workspaces: separate hubs on this machine, each with its own
    /// orchestrator, threads, worktrees and trash, sharing the config file. `twr -w <name>
    /// <command>` (or `TWR_WORKSPACE=<name>`) runs a command in one; `default` is the usual
    /// one. Without a subcommand, lists them.
    #[command(alias = "workspace")]
    Ws {
        #[command(subcommand)]
        cmd: Option<WsCmd>,
    },
    /// Add, list, inspect, install twr on and remove remote boxes. `twr box add <name>
    /// <ssh destination>` sets up a box the hub reaches over SSH (no token, listener or
    /// tunnel), and `twr box add <name> --command docker exec -i <container>` one it reaches
    /// through that command (a container): the hub installs twr there, keeps it connected,
    /// and runs the box's threads in a daemon on the box that outlives the connection. `twr box doctor <name>` checks a box,
    /// and `twr box login <name> <agent>` logs an agent CLI in there, in this terminal.
    #[command(name = "box")]
    Boxes {
        #[command(subcommand)]
        cmd: boxes::BoxCmd,
    },
    /// List, add, log in, check and remove a runtime's accounts: several logins of Claude
    /// Code, Codex or Grok Build, each a config directory of its own. `twr account add
    /// claude-code work` makes one and runs the runtime's login in this terminal; a thread
    /// picks one with `twr thread new --account` and moves with `twr model --account`.
    Account {
        #[command(subcommand)]
        cmd: accounts::AccountCmd,
    },
    /// Run this box as a runner: connect to a hub over WebSocket and run its threads here.
    /// Stops the runtimes gracefully on SIGINT / SIGTERM. `twr runner install-token <box>`
    /// stores a dial-in box's token here; `twr runner token` prints a new token for
    /// `[boxes.<name>] token`.
    #[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
    Runner {
        #[command(subcommand)]
        cmd: Option<RunnerCmd>,
        /// Hub URL: `wss://host:port`, or `ws://` to a loopback host (`localhost`, an address
        /// in 127.0.0.0/8, `[::1]` or an IPv4-mapped one such as `[::ffff:127.0.0.1]`: an SSH
        /// tunnel's end). Plain `ws://` to any other host is refused unless --allow-insecure is
        /// given.
        #[arg(
            long,
            value_name = "URL",
            required_unless_present_any = [
                "stdio", "daemon", "agents", "login", "exec", "find_repo", "clone_repo"
            ]
        )]
        hub: Option<String>,
        /// File holding the box's token (`[boxes.<name>] token` on the hub, at least 16
        /// characters); it must not be accessible by other users (`chmod 600`). Default:
        /// `$TWR_RUNNER_TOKEN`, else the file `twr runner install-token` writes
        /// (`~/.config/twr/runner-tokens/<box>.token`). A token from a file is read again
        /// after the hub refuses the runner, so a new one there is used without a restart.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        /// Box name (as configured on the hub).
        #[arg(
            long = "box",
            value_name = "NAME",
            required_unless_present_any = ["agents", "login", "exec", "find_repo", "clone_repo"]
        )]
        box_name: Option<String>,
        /// Data directory for the spool and worktrees (default: the twr data directory).
        #[arg(long, value_name = "DIR")]
        data_dir: Option<PathBuf>,
        /// For `wss://`: trust only this CA certificate (PEM) instead of the public roots
        /// (a hub with a private or self-signed CA).
        #[arg(long, value_name = "PEM")]
        ca: Option<PathBuf>,
        /// Allow plain `ws://` to a host that is not loopback: every event and command crosses
        /// the network in clear text (the token never does). Only on a network you trust.
        #[arg(long)]
        allow_insecure: bool,
        /// An SSH box's relay, which the hub's ssh runs: starts the box daemon of --hub-key
        /// and --box when none runs, then relays stdin/stdout to it.
        #[arg(
            long,
            hide = true,
            conflicts_with_all = ["daemon", "hub", "token_file", "ca", "allow_insecure"],
            requires_all = ["box_name", "hub_key"]
        )]
        stdio: bool,
        /// An SSH box's daemon, which the relay starts: serves the hub's sealed connections on
        /// --daemon-socket, logging to --log.
        #[arg(
            long,
            hide = true,
            conflicts_with_all = [
                "hub", "token_file", "ca", "allow_insecure", "replace", "stop", "restart"
            ],
            requires_all = ["box_name", "hub_key", "daemon_socket", "data_dir", "lock", "log"]
        )]
        daemon: bool,
        /// The hub's box key (64 hex digits: its Ed25519 public key).
        #[arg(long, hide = true, value_name = "HEX")]
        hub_key: Option<String>,
        /// With --stdio: replace a box daemon of another build with this one.
        #[arg(long, hide = true, requires = "stdio")]
        replace: bool,
        /// With --stdio: stop the box daemon of --hub-key and --box instead of relaying to it,
        /// and wait until it has stopped (at most 60 s). Its data and worktrees stay. Nothing
        /// to do when none runs.
        #[arg(long, hide = true, requires = "stdio", conflicts_with = "replace")]
        stop: bool,
        /// With --stdio: stop the box daemon of --hub-key and --box, whatever its build, start a
        /// new one, and relay to it (`twr box restart`; a Mac's daemon then starts in the GUI
        /// session when there is one). One relay at a time starts a daemon.
        #[arg(long, hide = true, requires = "stdio", conflicts_with = "stop")]
        restart: bool,
        /// With --stdio: the build id the hub launched (this executable's, as the hub knows
        /// it), taken as this relay's own when it cannot read its executable.
        #[arg(long, hide = true, value_name = "ID", requires = "stdio")]
        build: Option<String>,
        /// With --stdio: print the status line of the box daemon of --hub-key and --box, or
        /// `no daemon`, and nothing else (`twr box doctor`).
        #[arg(
            long,
            hide = true,
            requires = "stdio",
            conflicts_with_all = ["replace", "stop", "build", "restart"]
        )]
        status: bool,
        /// Print this box's agent CLIs as its runner finds them, as JSON: the login shell's
        /// PATH, then each agent's path, version and login state (`twr box doctor`).
        #[arg(
            long,
            hide = true,
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login"
            ]
        )]
        agents: bool,
        /// Start AGENT's login in this terminal, on the program this box's runner would run
        /// (its TWR_*_BIN, else on the login shell's PATH): `claude auth login`, `codex login
        /// --device-auth`, `grok login --device-auth` (`twr box login`). Needs a terminal.
        #[arg(
            long,
            hide = true,
            value_name = "AGENT",
            value_parser = ["claude", "claude-code", "codex", "grok", "grok-build"],
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key"
            ]
        )]
        login: Option<String>,
        /// Run RUNTIME's CLI with the arguments after `--`, its program resolved as a spawn
        /// of a thread on this box resolves it (its TWR_*_BIN, else on --path, else on the
        /// login shell's PATH before this one): what `twr attach` runs on an SSH box, with
        /// the arguments the hub built.
        #[arg(
            long,
            hide = true,
            value_name = "RUNTIME",
            value_parser = ["claude-code", "codex", "grok-build"],
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents"
            ]
        )]
        exec: Option<String>,
        /// With --exec: the thread's PATH, when it configures one (it wins over the login
        /// shell's, as for a spawn). (The conflicts repeat --exec's: clap does not check
        /// `requires` against an argument that conflicts with one given.)
        #[arg(
            long = "path",
            hide = true,
            value_name = "PATH",
            requires = "exec",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents"
            ]
        )]
        exec_path: Option<String>,
        /// With --exec: a variable of the thread's environment (NAME=VALUE, repeated), set on
        /// the runtime's command only: the program is resolved, and the login shell asked,
        /// with this process's environment, as a spawn's are.
        #[arg(
            long = "env",
            hide = true,
            value_name = "NAME=VALUE",
            requires = "exec",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents"
            ]
        )]
        exec_env: Vec<String>,
        /// With --exec: a variable the runtime's command does not get (repeated).
        #[arg(
            long = "unset",
            hide = true,
            value_name = "NAME",
            requires = "exec",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents"
            ]
        )]
        exec_unset: Vec<String>,
        /// With --exec: the runtime's arguments.
        #[arg(
            last = true,
            hide = true,
            value_name = "ARGS",
            requires = "exec",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents"
            ]
        )]
        exec_args: Vec<String>,
        /// Print, as JSON, the git repositories on this box that could be the hub's repository
        /// of folder name --name (`twr box project`): the --hint path, then those under the
        /// default project roots in this home, each with its `origin` and whether that names
        /// --origin's repository ([`twr_runner::repos::find_repo`]). Reads only, no network.
        #[arg(
            long,
            hide = true,
            requires = "repo_name",
            group = "repo_op",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec"
            ]
        )]
        find_repo: bool,
        /// With --find-repo: the hub repository's `origin` URL (without credentials).
        #[arg(
            long,
            hide = true,
            value_name = "URL",
            requires = "find_repo",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec", "clone_repo"
            ]
        )]
        origin: Option<String>,
        /// With --find-repo: the path on this box to look at first.
        #[arg(
            long,
            hide = true,
            value_name = "PATH",
            requires = "find_repo",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec", "clone_repo"
            ]
        )]
        hint: Option<PathBuf>,
        /// Clone --url into a new directory --parent/--name in this terminal, then print
        /// `twr-clone-path <path>` (`twr box project --clone`; checks:
        /// [`twr_runner::repos::clone_repo`]). Needs a terminal.
        #[arg(
            long,
            hide = true,
            requires_all = ["repo_name", "clone_url", "clone_parent"],
            group = "repo_op",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec", "find_repo"
            ]
        )]
        clone_repo: bool,
        /// With --clone-repo: the URL (https://, ssh:// or user@host:path, no credentials).
        #[arg(
            long = "url",
            hide = true,
            value_name = "URL",
            requires = "clone_repo",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec", "find_repo"
            ]
        )]
        clone_url: Option<String>,
        /// With --clone-repo: the directory the clone goes in.
        #[arg(
            long = "parent",
            hide = true,
            value_name = "DIR",
            requires = "clone_repo",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec", "find_repo"
            ]
        )]
        clone_parent: Option<PathBuf>,
        /// With --find-repo or --clone-repo: the repository's folder name.
        #[arg(
            long = "name",
            hide = true,
            value_name = "NAME",
            requires = "repo_op",
            conflicts_with_all = [
                "hub", "token_file", "box_name", "data_dir", "ca", "allow_insecure", "stdio",
                "daemon", "hub_key", "login", "agents", "exec"
            ]
        )]
        repo_name: Option<String>,
        /// With --daemon: the socket to listen on (`--socket` is the hub socket's).
        #[arg(long, hide = true, value_name = "PATH", requires = "daemon")]
        daemon_socket: Option<PathBuf>,
        /// With --daemon: its machine lock.
        #[arg(long, hide = true, value_name = "PATH", requires = "daemon")]
        lock: Option<PathBuf>,
        /// With --daemon: the file it logs to.
        #[arg(long, hide = true, value_name = "PATH", requires = "daemon")]
        log: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum RunnerCmd {
    /// Print a new random token (32 bytes from the OS, hex): put it in the hub's
    /// `[boxes.<name>] token` and in the runner's `--token-file` (`chmod 600`).
    Token,
    /// On a dial-in box: store the box's token (`twr box add --dial-in` or `twr box token` on
    /// the hub prints it) in `~/.config/twr/runner-tokens/<box>.token` (mode 0600), where `twr
    /// runner` finds it. Asks for it at a prompt that does not show it, or reads it from a
    /// pipe. With --hub, prints the command that runs the runner and a service to paste that
    /// keeps it running (a systemd user unit, or a LaunchAgent on a Mac).
    InstallToken {
        /// The box's name on the hub.
        #[arg(value_name = "BOX")]
        box_name: String,
        /// The hub's runner URL: `wss://host:port`, or `ws://` to a loopback host (an SSH
        /// tunnel's end).
        #[arg(long, value_name = "URL")]
        hub: Option<String>,
        /// For `wss://`: trust only this CA certificate (PEM), as `twr runner --ca`.
        #[arg(long, value_name = "PEM")]
        ca: Option<PathBuf>,
        /// Store the token in this file instead (its directory must exist).
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        /// Replace another token stored for the box without asking.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum HubCmd {
    /// Stop the running hub (`hub.shutdown`) and wait until it has exited. It stops its local
    /// runtimes first, so their turns end; threads on remote boxes keep running under their
    /// runners. Nothing to do when no hub is running.
    Stop {
        /// Stop default and every named workspace in this home, ignoring -w/TWR_WORKSPACE.
        /// Uses hub.socket for default and the standard named sockets; a hub on another
        /// --socket is reported as failed (stop it separately with --socket). Other TWR_HOME
        /// instances are not included. Prints each result; exits non-zero if any failed.
        #[arg(long, conflicts_with = "socket")]
        all: bool,
        /// How long to wait for the hub to exit, in seconds (per workspace with --all).
        #[arg(
            long,
            value_name = "SECS",
            default_value_t = HUB_STOP_TIMEOUT.as_secs(),
            value_parser = clap::value_parser!(u64).range(1..)
        )]
        timeout: u64,
    },
}

#[derive(Debug, Subcommand)]
enum WsCmd {
    /// List the workspaces: whether each one's hub runs and, when it does, its threads, how
    /// many are running and how many wait on you. `*` marks the one this command is in.
    #[command(alias = "ls")]
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create a workspace (1-32 of [a-z0-9-], starting and ending with a letter or digit).
    /// Its hub starts on first use: `twr -w <name>`.
    New {
        /// The workspace's name.
        name: String,
    },
    /// Remove a workspace's data: its log, orchestrator conversation and history. Refused
    /// while its hub runs, while it has threads (delete them first) or while its trash holds
    /// deleted threads (`twr -w <name> trash empty --all`). Branches stay in their
    /// repositories.
    Rm {
        /// The workspace's name.
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum JobCmd {
    /// Queue a command (no shell: `-- sh -c '...'` for one) to run in the thread's directory.
    /// Prints the job's id and returns at once; the thread gets a prompt when it ends.
    Run {
        /// The thread (default: `TWR_THREAD`, the thread an agent runs in).
        #[arg(long)]
        thread: Option<String>,
        /// A short name for people (default: the command's first two words).
        #[arg(long)]
        name: Option<String>,
        /// Job slots it takes while it runs (default 1).
        #[arg(long)]
        slots: Option<usize>,
        /// Cancel it once it ran this long: 90s, 30m, 2h.
        #[arg(long)]
        timeout: Option<String>,
        /// Do not prompt the thread when it ends.
        #[arg(long)]
        no_wake: bool,
        /// The command and its arguments.
        #[arg(required = true, last = true)]
        argv: Vec<String>,
    },
    /// List the queued and running jobs.
    #[command(alias = "ls")]
    List {
        /// Only this thread's.
        #[arg(long)]
        thread: Option<String>,
        /// Print the raw `job.list` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the end of a job's output.
    Log {
        /// The job's id (a unique start of a queued or running one's).
        job: String,
        /// How many lines (default 40).
        #[arg(long, short = 'n')]
        lines: Option<usize>,
    },
    /// Cancel a queued or running job (its process group gets SIGTERM, then SIGKILL).
    Cancel {
        /// The job's id, or a unique start of it.
        job: String,
    },
}

/// `twr speech`.
#[derive(Subcommand, Debug)]
enum SpeechCmd {
    /// Stream the orchestrator's new messages: with `--text`, their sentences one per line
    /// (for a reader of your own); without it, the line protocol `twr speak --ssh` reads.
    Feed {
        /// Print each message's sentences, one per line, and nothing else.
        #[arg(long)]
        text: bool,
        /// The line protocol (the default); for `twr speak --ssh`.
        #[arg(long, conflicts_with = "text")]
        proto: bool,
        /// Every workspace of this machine, not only the current one (`-w`).
        #[arg(long)]
        all_workspaces: bool,
    },
}

#[derive(Subcommand, Debug)]
enum LogCmd {
    /// Move the agent events `[log]` lets go (of turns older than `retention_days` whose
    /// reactions are done, and of deleted threads whose trash item is purged) and the blobs
    /// only they reach out of twr.db into archive.db now (or delete them, with `archive =
    /// false`). The hub does this every hour by itself when 2 000 rows or more can go.
    Compact {
        /// Then rewrite twr.db whole (`VACUUM`), which gives the freed space back to the
        /// file system and turns on incremental vacuum for the next runs. The hub waits for
        /// it, doing nothing else meanwhile: 3 to 18 seconds for a 475 MB twr.db.
        #[arg(long)]
        vacuum: bool,
        /// Drop the hub's checkpoints of its state first (its next start folds the whole log).
        #[arg(long)]
        drop_checkpoints: bool,
        /// Print the raw `log.compact` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a checkpoint of the hub's folded state for this build of twr, so that its hub's
    /// next start folds only what comes after it instead of the whole log. Run it after
    /// installing a new build while a hub of the old one still runs (`docs/tools/
    /// dev-install.sh` does): checkpoints are kept per build, and a build uses only its own.
    /// Works whether or not the hub runs, never starts one, and leaves a database of another
    /// schema alone (the hub's start migrates it).
    Checkpoint {
        /// Every workspace of this home, not only this one.
        #[arg(long)]
        all: bool,
        /// Print one JSON object per workspace.
        #[arg(long)]
        json: bool,
    },
    /// Sizes of twr.db, its write-ahead log and archive.db, rows and bytes per kind, blobs,
    /// checkpoints, the control backlog (whole and compacted), and what the next compaction
    /// would move.
    Stats {
        /// Print the raw `log.stats` JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
enum TrashCmd {
    /// List the trash of every box: each deleted thread's item with its short id, project,
    /// box, when it was deleted and when it is purged, its size on disk, what its delete card
    /// said it loses, and whether it was restored already. Items of a box that is offline are
    /// shown as the hub recorded them.
    #[command(alias = "ls")]
    List {
        /// Print the raw `trash.list` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Purge items from the trash now: their worktree files, record and kept commits go for
    /// good. Lists the items it purges and asks y/N on a terminal (without one, only with
    /// --yes); then purges exactly those, so an item deleted meanwhile stays.
    Empty {
        /// Items by id, a unique end of an id (at least 4 characters, as `twr trash list`
        /// shows it), or the name of the thread of exactly one item.
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        items: Vec<String>,
        /// Purge every item of every box that is online.
        #[arg(long)]
        all: bool,
        /// Purge without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Bring a deleted thread back from the trash, parked, with its history and session (`twr
    /// resume` resumes it): its branch is made again from the trash's ref, git adds a fresh
    /// worktree on it (at its old path, or a new one when that is taken) and the saved files
    /// are copied in. Staged changes come back unstaged, the worktree is on the thread's
    /// branch even if its HEAD was elsewhere (the trash keeps that commit), and the item
    /// stays in the trash, marked restored, until it is purged or emptied.
    Restore {
        /// The item: its id, a unique end of one (at least 4 characters), or the thread's
        /// name.
        item: String,
        /// Restore it under another name (needed when another thread took its name, or its
        /// recorded branch exists again).
        #[arg(long = "as", value_name = "NAME")]
        as_name: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum AttentionCmd {
    /// Dismiss an item the hub raised (a "which thread?" question, a review, an error, a
    /// stalled notice): it closes without an answer and a question's message goes nowhere.
    /// A runtime's permission or question cannot be dismissed (the runtime waits for it):
    /// answer it with `twr respond`, or stop the thread.
    Dismiss {
        /// `<attn_id>@<revision>` as `twr attention --all` lists it (or a bare attn_id).
        target: String,
    },
}

#[derive(Debug, Subcommand)]
enum OrchestratorCmd {
    /// Clear the conversation: a fresh session on the current runtime/model, no handoff.
    /// The old conversation stays in the log; workers and their attention are kept.
    Reset,
    /// Show the orchestrator: its runtime and model, its thread, the configured default and
    /// the runtimes that can host it.
    Show {
        /// Print the raw `orchestrator.show` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Switch the orchestrator to another runtime (and model), now. A conversation cannot move
    /// between runtimes: this starts a NEW orchestrator session, which gets a handoff as its
    /// first turn: the current orchestrator's own summary (asked for first when it is running;
    /// a busy one is waited for up to --wait seconds, then skipped) plus the recent
    /// conversation, threads, attention items and reports from the log. The current
    /// orchestrator is then stopped (its queued prompts dropped) and retired; the new one
    /// takes the names `orchestrator` / `orch`. The choice is kept across hub restarts (the
    /// config's `[orchestrator]` only picks the first orchestrator).
    Use {
        /// A runtime that can host the orchestrator (it must attach the twr MCP tools):
        /// `claude-code`, `codex`, `grok-build`, a `[runtimes.<name>]` ACP preset, or
        /// `acp:<command line>` for any Agent Client Protocol agent (`twr orchestrator show`
        /// lists them).
        runtime: String,
        /// Model (default: the runtime's default).
        #[arg(long)]
        model: Option<String>,
        /// Do not ask the current orchestrator for a handoff summary: the new one gets only
        /// what the log shows (recent conversation, threads, attention items, reports).
        #[arg(long)]
        no_summary: bool,
        /// How long to wait for a busy orchestrator to finish its turn before skipping its
        /// summary, in seconds (default 60, at most 600). Without it, a terminal asks first
        /// when the orchestrator is busy.
        #[arg(long, value_name = "SECS", conflicts_with = "no_summary")]
        wait: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
enum ContextCmd {
    /// Print the standing context.
    Show {
        /// A configured project name, or the project of a thread.
        project: String,
    },
    /// Edit a copy in `$VISUAL` / `$EDITOR` (else `vi`); the hub records and writes it when
    /// the editor exits successfully and the text changed.
    Edit {
        /// A configured project name, or the project of a thread.
        project: String,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCmd {
    /// List the notes: name, type, when saved, description; `!` marks a note the orchestrator
    /// saved in a turn you did not start (it may come from a worker's text).
    List {
        /// As JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print a note.
    Show {
        /// The note's name.
        name: String,
    },
    /// Edit a note (or write a new one) in `$VISUAL` / `$EDITOR` (else `vi`): its
    /// `description:` and `type:` lines, then its text. Saved when the editor exits
    /// successfully and the text changed; a note changed meanwhile is a conflict, never
    /// overwritten.
    Edit {
        /// The note's name (lowercase words joined by -).
        name: String,
    },
    /// Forget notes: their files are removed.
    Rm {
        /// The notes' names.
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Print the memory's directory (one Markdown file per note).
    Path,
}

// Parsed once per run: the size of `New` next to the small `List` costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Subcommand)]
enum ThreadCmd {
    /// Create a thread on a project (a configured project name or a directory).
    New {
        /// Project name from the config, or a path.
        project: String,
        /// Runtime (default: the project's, then `default_runtime`): `claude-code`, `codex`,
        /// `grok-build`, `fake`, `acp:<command line>` for any Agent Client Protocol agent (e.g.
        /// `acp:gemini --experimental-acp`) or a `[runtimes.<name>]` preset naming one, or
        /// `pty:<command line>` for any interactive CLI (DEGRADED). Commands are split like a
        /// shell would, without expansion.
        #[arg(long)]
        runtime: Option<String>,
        /// The runtime's account to run on (`twr account list`): a name, a glob (`max-*`) or
        /// a comma-separated list, the first usable one taken (default: the project's pin,
        /// else `[account_policy.<runtime>] default`, else the runtime's own login).
        /// `--runtime claude-code:work` picks one too.
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        /// Open it on its account (`--account`, or `--runtime <runtime>:<account>`) although
        /// the project's pin keeps it off that account.
        #[arg(long)]
        outside_pin: bool,
        /// Box to run on: `local`, or a remote box from `[boxes.<name>]` whose runner is
        /// connected (default: the project's `box`, else `local`).
        #[arg(long = "box", value_name = "BOX")]
        box_name: Option<String>,
        /// Unique thread name (default: generated).
        #[arg(long)]
        name: Option<String>,
        /// Runtime permission mode.
        #[arg(long)]
        mode: Option<String>,
        /// Model (default: `threads.model` from the config, else the runtime's default).
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort, one the runtime takes (see `twr model`; default: the runtime's).
        #[arg(long)]
        effort: Option<String>,
        /// Extra names for the thread (repeatable).
        #[arg(long = "alias", value_name = "ALIAS")]
        aliases: Vec<String>,
        /// Work in the repository itself instead of a worktree of its own.
        #[arg(long, conflicts_with_all = ["base", "worktree"])]
        no_worktree: bool,
        /// Give the thread a worktree of its own even when worktrees are off for the project
        /// (`[projects.<name>] worktree`, else `[threads] worktree`; on by default).
        #[arg(long)]
        worktree: bool,
        /// Start the thread's branch `twr/<name>` from this ref (default: the repository's
        /// HEAD).
        #[arg(long, value_name = "REF")]
        base: Option<String>,
    },
    /// List threads. Parked threads are left out (a footer names them) unless --all.
    List {
        /// Also list parked threads.
        #[arg(long, short)]
        all: bool,
    },
}

fn main() -> ExitCode {
    // A job's start gate (`twr_hub::job_gate`), as the hub runs it: before anything else, so
    // nothing of twr's (arguments, logging, a runtime) touches the job's process.
    if std::env::args_os().nth(1).as_deref() == Some(twr_hub::job_gate::GATE_ARG.as_ref()) {
        twr_hub::job_gate_main(std::env::args_os().skip(1).collect());
    }
    let cli = Cli::parse();
    twr_core::trace::mark_with(|| {
        let role = match &cli.cmd {
            None => "tui",
            Some(Cmd::Hub { cmd: None }) => "hub",
            Some(_) => "cli",
        };
        format!("{role}: process started")
    });
    let stopping_hub = matches!(
        cli.cmd,
        Some(Cmd::Hub {
            cmd: Some(HubCmd::Stop { .. })
        })
    );
    // The relay's read of stdin is a blocking task, which a runtime's graceful shutdown would
    // wait on after the daemon's side ended: it leaves without it. So does `--find-repo`, past
    // its deadline, without a walk of the box's folders that hangs (a dead network mount). And
    // so does `twr speech feed`, whose proxy reads stdin the same way: once its output is gone
    // (or its speaker went quiet) it ends with stdin still open. Every line it wrote was
    // flushed as it was written.
    let relaying = matches!(
        cli.cmd,
        Some(
            Cmd::Runner { stdio: true, .. }
                | Cmd::Runner {
                    find_repo: true,
                    ..
                }
                | Cmd::Speech {
                    cmd: SpeechCmd::Feed { .. }
                }
        )
    );
    // Before anything else that could fail: a hook that exits with anything but 2 lets the
    // tool call through.
    if let Some(Cmd::ClaudeMemoryGuard { memory_dir }) = &cli.cmd {
        return memory_guard(memory_dir.clone());
    }
    // The hub sets the agent marker only on what it starts for a thread:
    // a hub started from an agent's shell must not pass that shell's marker on to the rest
    // of what it runs. Removed while this process has a single thread, before the runtime
    // starts its workers.
    if matches!(cli.cmd, Some(Cmd::Hub { cmd: None })) {
        std::env::remove_var(twr_hub::host::AGENT_ENV);
        // What the hub runs inherits the workspace it serves, so an agent's own `twr` calls
        // reach this hub (an invalid name fails below, before anything starts).
        let from_env = std::env::var_os(WORKSPACE_ENV);
        // One that is not UTF-8 stays, for `run` to refuse.
        let unreadable =
            cli.workspace.is_none() && from_env.as_ref().is_some_and(|v| v.to_str().is_none());
        let workspace = cli
            .workspace
            .clone()
            .or_else(|| from_env.and_then(|v| v.into_string().ok()))
            .filter(|w| !w.is_empty() && w != DEFAULT_WORKSPACE);
        match workspace {
            Some(w) => std::env::set_var(WORKSPACE_ENV, w),
            None if unreadable => {}
            None => std::env::remove_var(WORKSPACE_ENV),
        }
    }
    // A runner serves whichever hub it connects to: no workspace of this machine is its, and
    // nothing it runs inherits one.
    if matches!(cli.cmd, Some(Cmd::Runner { .. })) {
        std::env::remove_var(WORKSPACE_ENV);
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            errln!("twr: starting the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let code = match runtime.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) => {
            // The hub's hints quote commands of its own workspace's: run as this command was.
            errln!("twr: {}", localize(&format!("{e:#}")));
            ExitCode::FAILURE
        }
    };
    shutdown_cli_runtime(runtime, stopping_hub || relaying);
    code
}

/// A timed-out process-table read is a blocking task, which Tokio cannot cancel. Short
/// stop commands must exit without waiting on it, as must an SSH box's relay (its read of
/// stdin); the foreground hub keeps graceful drop.
fn shutdown_cli_runtime(runtime: tokio::runtime::Runtime, background: bool) {
    if background {
        runtime.shutdown_background();
    }
}

/// `twr claude-memory-guard`: answers the `PreToolUse` hook input on stdin
/// (`twr_adapters::claude_code::guard`) within the guard's deadline. A denial (outside the
/// memory directory, stdin unreadable, a panic, no decision by the deadline) is a JSON decision
/// on stdout and exit 2, which Claude Code blocks on either way. With `memory_dir`
/// (`--memory-dir`), exactly that directory is allowed (`guard::check_with`).
fn memory_guard(memory_dir: Option<PathBuf>) -> ExitCode {
    use std::io::Read;
    use twr_adapters::claude_code::guard;
    let read = || {
        let mut input = String::new();
        std::io::stdin()
            .take(4 << 20)
            .read_to_string(&mut input)
            .map(|_| input)
    };
    match guard::run_with(read, guard::deadline(), memory_dir) {
        Some(deny) => {
            outln!("{deny}");
            ExitCode::from(2)
        }
        None => ExitCode::SUCCESS,
    }
}

/// Refuses `--socket` and `--workspace` with `twr runner`: a runner never uses the hub's
/// socket (it connects to `--hub`) and serves whichever hub that is, so the flags would be
/// silently ignored.
fn refuse_runner_socket(cli: &Cli) -> anyhow::Result<()> {
    if matches!(cli.cmd, Some(Cmd::Runner { .. })) {
        if cli.socket.is_some() {
            bail!("--socket does not apply to `twr runner`: it connects to the hub at --hub");
        }
        if cli.workspace.is_some() {
            bail!("--workspace does not apply to `twr runner`: it connects to the hub at --hub");
        }
    }
    Ok(())
}

/// clap checks the conflict when --socket follows the subcommand, but a global flag
/// supplied before it is not checked against that subcommand's local --all flag.
fn refuse_bulk_stop_socket(cli: &Cli) -> anyhow::Result<()> {
    if cli.socket.is_some()
        && matches!(
            cli.cmd,
            Some(Cmd::Hub {
                cmd: Some(HubCmd::Stop { all: true, .. })
            })
        )
    {
        bail!("--socket cannot be combined with `twr hub stop --all`: stop that socket separately with `twr --socket PATH hub stop`");
    }
    Ok(())
}

/// Refuses a `hub.socket` (the `default` workspace's) in the directory of the named
/// workspaces' sockets (`<socket dir>/ws/`): it could be one of theirs, and both would then
/// reach the same hub.
fn check_socket_layout(root: &Paths) -> anyhow::Result<()> {
    let config = Config::load_from(root)?;
    let Some(socket) = &config.hub.socket else {
        return Ok(());
    };
    let ws_dir = root.socket.parent().map(|d| d.join("ws"));
    let resolve = |p: &Path| twr_core::paths::resolve_lenient(p).unwrap_or_else(|| p.to_path_buf());
    if let (Some(ws_dir), Some(parent)) = (ws_dir, socket.parent()) {
        if resolve(parent) == resolve(&ws_dir) {
            bail!(
                "hub.socket {} is in {}, where the workspaces' sockets are: give the default \
                 hub a socket elsewhere",
                socket.display(),
                ws_dir.display()
            );
        }
    }
    Ok(())
}

/// How messages name the workspace a command ran in: ` -w <name>` for a named one, empty for
/// `default` (`twr{hint} hub`). Set once by `run`.
static WORKSPACE_HINT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// The workspace the hub a command connects to must serve, when the socket was not given
/// (`--socket`): set once by `run` ([`check_workspace`]).
static EXPECTED_WORKSPACE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Refuses a hub that serves another workspace than the command's ([`EXPECTED_WORKSPACE`]),
/// by what its `hello` said: it took the socket of that workspace (`--socket` when it
/// started). A hub that says no workspace predates them: it serves `default` only.
fn check_workspace(hello: &Value, socket: &Path) -> anyhow::Result<()> {
    let Some(expected) = EXPECTED_WORKSPACE.get() else {
        return Ok(());
    };
    check_workspace_name(hello, socket, expected)
}

fn check_workspace_name(hello: &Value, socket: &Path, expected: &str) -> anyhow::Result<()> {
    if hub_serves(hello, expected) {
        return Ok(());
    }
    match hello["workspace"].as_str() {
        Some(w) => bail!(
            "the hub at {} serves workspace {w}, not {expected}",
            socket.display()
        ),
        None => bail!(
            "the hub at {} serves no workspace {expected} (it is older than workspaces)",
            socket.display()
        ),
    }
}

/// Whether the hub that said `hello` serves workspace `name`: the workspace it says, or
/// `default` for one that says none (older than workspaces).
fn hub_serves(hello: &Value, name: &str) -> bool {
    hello["workspace"].as_str().unwrap_or(DEFAULT_WORKSPACE) == name
}

/// `text` (the hub's) with the commands it quotes run in this command's workspace
/// ([`twr_core::paths::localize_commands`]).
fn localize(text: &str) -> String {
    twr_core::paths::localize_commands(text, workspace_hint())
}

/// An attention item's summary as shown: as raised. A runtime's request, or a hub card
/// quoting one, may quote a command it would run, which approving runs as written (only
/// `twr delete` localizes the delete card it raised, [`localize`]).
fn item_summary(item: &Value) -> String {
    str_of(&item["summary"]).to_string()
}

/// ` -w <name>` when the command runs in a named workspace, else empty.
fn workspace_hint() -> &'static str {
    WORKSPACE_HINT.get().map_or("", String::as_str)
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    refuse_runner_socket(&cli)?;
    refuse_bulk_stop_socket(&cli)?;
    // An SSH box's relay and daemon need no twr home: every path comes from the box's
    // environment (the relay) or the relay (the daemon).
    let cli = match cli.cmd {
        Some(Cmd::Runner {
            cmd: None,
            agents: true,
            ..
        }) => {
            let report = twr_runner::agents::agents_report(&twr_runner::BoxEnv::user()).await;
            outln!("{}", serde_json::to_string(&report)?);
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd: None,
            login: Some(agent),
            ..
        }) => return runner_login(&agent).await,
        Some(Cmd::Runner {
            cmd: None,
            find_repo: true,
            repo_name: Some(name),
            origin,
            hint,
            ..
        }) => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_absolute());
            let report = twr_runner::repos::find_repo(
                &twr_runner::local::LocalGit::default(),
                home.as_deref(),
                &name,
                origin.as_deref(),
                hint.as_deref(),
                tokio::time::Instant::now() + twr_runner::repos::FIND_DEADLINE,
            )
            .await?;
            outln!("{}", serde_json::to_string(&report)?);
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd: None,
            clone_repo: true,
            repo_name: Some(name),
            clone_url: Some(url),
            clone_parent: Some(parent),
            ..
        }) => return runner_clone(&url, &parent, &name).await,
        Some(Cmd::Runner {
            cmd: None,
            exec: Some(runtime),
            exec_path,
            exec_env,
            exec_unset,
            exec_args,
            ..
        }) => {
            return runner_exec(
                &runtime,
                exec_path.as_deref(),
                &exec_env,
                &exec_unset,
                &exec_args,
            )
            .await
        }
        Some(Cmd::Runner {
            cmd: None,
            stdio: true,
            status: true,
            box_name: Some(box_name),
            hub_key: Some(hub_key),
            ..
        }) => {
            let hub_key = twr_core::sealed::parse_hub_public(&hub_key)?;
            let opts = twr_runner::relay::RelayOptions::new(&box_name, hub_key, false)?;
            match twr_runner::relay::status(&opts).await? {
                Some(status) => outln!("{}", status.line().trim_end()),
                None => outln!("no daemon"),
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd: None,
            stdio: true,
            box_name: Some(box_name),
            hub_key: Some(hub_key),
            replace,
            stop,
            restart,
            build,
            ..
        }) => {
            let hub_key = twr_core::sealed::parse_hub_public(&hub_key)?;
            let mut opts = twr_runner::relay::RelayOptions::new(&box_name, hub_key, replace)?;
            opts.restart = restart;
            if let Some(b) = build {
                if !twr_core::valid_build_id(&b) {
                    bail!("--build {b:?} is not a build id");
                }
                opts.build = Some(b);
            }
            if stop {
                twr_runner::relay::stop(&opts, &mut |line: &str| note(line)).await?;
            } else {
                twr_runner::relay::relay(opts).await?;
            }
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd: None,
            daemon: true,
            box_name: Some(box_name),
            hub_key: Some(hub_key),
            daemon_socket: Some(socket),
            data_dir: Some(data_dir),
            lock: Some(lock),
            log: Some(log),
            ..
        }) => {
            let hub_key = twr_core::sealed::parse_hub_public(&hub_key)?;
            let opts = twr_runner::RunnerOptions::for_daemon(
                box_name,
                data_dir,
                twr_runner::DaemonOptions {
                    socket,
                    hub_key,
                    lock,
                    build: twr_core::build_id().map(str::to_string),
                    launch: Some(twr_runner::daemon::own_launch()),
                },
            );
            run_box_daemon(opts, &log).await?;
            return Ok(ExitCode::SUCCESS);
        }
        cmd => Cli { cmd, ..cli },
    };
    // A bulk stop selects a home, not one workspace; even a stale workspace selection
    // must not keep it from reaching the others. Refuse locally before discovering them,
    // including when no hub is there to enforce the RPC's agent guard.
    if let Some(Cmd::Hub {
        cmd: Some(HubCmd::Stop { all: true, timeout }),
    }) = cli.cmd
    {
        refuse_agent_action("stop hubs")?;
        let root = Paths::from_env_in(Some(DEFAULT_WORKSPACE))?;
        hub_stop_all(&root, Duration::from_secs(timeout)).await?;
        return Ok(ExitCode::SUCCESS);
    }
    // A runner's default storage is the root's (`main` removed `TWR_WORKSPACE` for it).
    let paths = match cli.cmd {
        Some(Cmd::Runner { .. }) => Paths::from_env_in(Some(DEFAULT_WORKSPACE))?,
        _ => Paths::from_env_in(cli.workspace.as_deref())?,
    };
    // With `-w`, a `default` that the shell's `TWR_WORKSPACE` would not select is named too;
    // a socket given explicitly is how commands shown reach its hub.
    let inherited = std::env::var_os(WORKSPACE_ENV).filter(|_| cli.workspace.is_some());
    let _ = WORKSPACE_HINT.set(match &cli.socket {
        Some(socket) => format!(" --socket {}", sh_word(&socket.to_string_lossy())),
        None => {
            twr_core::paths::workspace_selection(paths.workspace.as_deref(), inherited.as_deref())
        }
    });
    let cli = match cli.cmd {
        Some(Cmd::Runner {
            cmd: Some(RunnerCmd::Token),
            ..
        }) => {
            outln!("{}", twr_core::runner::new_token()?);
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd:
                Some(RunnerCmd::InstallToken {
                    box_name,
                    hub,
                    ca,
                    token_file,
                    yes,
                }),
            ..
        }) => {
            dial_in::install_token(dial_in::InstallToken {
                box_name,
                hub,
                ca,
                token_file,
                yes,
            })?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Runner {
            cmd: None,
            hub,
            token_file,
            box_name,
            data_dir,
            ca,
            allow_insecure,
            ..
        }) => {
            // clap requires both without a subcommand.
            let (Some(hub), Some(box_name)) = (hub, box_name) else {
                bail!("twr runner needs --hub and --box");
            };
            let data_dir = data_dir.unwrap_or_else(|| paths.data_dir.clone());
            let default_token = paths.runner_token_file(&box_name);
            let mut opts = twr_runner::RunnerOptions::new(
                hub,
                twr_core::runner::Secret::default(),
                box_name,
                data_dir,
            );
            opts.ca_file = ca;
            opts.allow_insecure = allow_insecure;
            run_runner(opts, token_file, default_token).await?;
            return Ok(ExitCode::SUCCESS);
        }
        // Before the config is loaded: a config this twr cannot read must not stop an update
        // (the new one may read it).
        Some(Cmd::Update { version }) => {
            update(version, cli.socket, &paths).await?;
            return Ok(ExitCode::SUCCESS);
        }
        // About the terminal, not a hub: no config needed.
        Some(Cmd::TerminalSetup { terminal, print }) => {
            let terminal = match terminal.as_deref() {
                None => None,
                Some(name) => Some(twr_tui::terminal_setup::Terminal::parse(name).ok_or_else(
                    || {
                        anyhow!(
                            "no terminal {name:?}: name one of {}",
                            twr_tui::terminal_setup::ALL
                                .iter()
                                .map(|t| t.name())
                                .chain(["vscode-insiders", "cursor", "windsurf", "vscodium"])
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                )?),
            };
            let place = twr_tui::terminal_setup::place(twr_tui::keys::under_mosh());
            let agent = calling_agent();
            let (lines, failed) =
                twr_tui::terminal_setup::run(&place, terminal, print, agent.as_deref());
            for line in lines {
                outln!("{line}");
            }
            return Ok(if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            });
        }
        // Workspaces are the root's: these work from any workspace, existing or not.
        Some(Cmd::Ws { cmd }) => {
            ws(cmd, &paths).await?;
            return Ok(ExitCode::SUCCESS);
        }
        // A speaker is the root's (its state and `[speech]`), and with `--ssh` (or `twr
        // remote`) `-w` names a workspace of the other machine, which this one need not have.
        Some(Cmd::Speak {
            ssh,
            all_workspaces,
            name,
            engine,
            voice,
            rate,
            print_service,
        }) => {
            refuse_agent_action("start a speaker")?;
            if cli.socket.is_some() {
                bail!("--socket does not apply to `twr speak`: it reads the workspaces' own hubs");
            }
            speech::speak(
                speech::Speak {
                    ssh,
                    workspace: cli.workspace.clone(),
                    all_workspaces,
                    name,
                    engine,
                    voice,
                    rate,
                    print_service,
                },
                &paths,
            )
            .await?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(Cmd::Remote {
            host,
            ssh,
            name,
            args,
        }) => {
            refuse_agent_action("start a remote session with a speaker")?;
            if cli.socket.is_some() {
                bail!("--socket does not apply to `twr remote`");
            }
            return speech::remote(
                speech::Remote {
                    host,
                    ssh,
                    name,
                    workspace: cli.workspace.clone(),
                    args,
                },
                &paths,
            )
            .await;
        }
        // Before the config is loaded: it repairs config files that do not load (a box in
        // both, or one whose settings twr refuses, comes out of boxes.toml). The hub's socket
        // as config.toml alone says, else the default.
        Some(Cmd::Boxes {
            cmd: boxes::BoxCmd::Remove { name },
        }) => {
            let socket = cli.socket.clone().unwrap_or_else(|| {
                Config::load(&paths.config_file)
                    .unwrap_or_default()
                    .in_workspace(&paths)
                    .socket_path(&paths)
            });
            boxes::remove(&paths, &socket, &name).await?;
            return Ok(ExitCode::SUCCESS);
        }
        cmd => Cli { cmd, ..cli },
    };
    if !paths.exists() {
        let name = paths.workspace_name();
        let from = if cli.workspace.is_none() {
            format!(" (from {WORKSPACE_ENV})")
        } else {
            String::new()
        };
        bail!("there is no workspace {name}{from}: create it with `twr ws new {name}` (`twr ws` lists them)");
    }
    let config = Config::load_from(&paths)?;
    twr_core::trace::mark("cli: config loaded");
    if cli.socket.is_none() {
        let _ = EXPECTED_WORKSPACE.set(paths.workspace_name().to_string());
        // `hub.socket` is the default workspace's alone.
        if paths.workspace.is_none() {
            check_socket_layout(&paths.root())?;
        }
    }
    let socket = cli
        .socket
        .clone()
        .unwrap_or_else(|| config.socket_path(&paths));
    // Every subcommand talks to (or is) the hub on this socket.
    check_socket_path(&socket)?;
    let Some(cmd) = cli.cmd else {
        // A socket given explicitly is that hub's alone: no workspaces to switch to.
        let switching = cli.socket.is_none().then(|| twr_tui::Switching {
            current: paths.workspace_name().to_string(),
            workspaces: std::sync::Arc::new(TuiWorkspaces {
                root: paths.root(),
                config: Config::load_from(&paths.root()).unwrap_or_else(|_| config.clone()),
            }),
        });
        open_tui(
            &socket,
            &paths,
            switching,
            config.tui.clone(),
            config.speech.clone(),
        )
        .await?;
        return Ok(ExitCode::SUCCESS);
    };
    match cmd {
        Cmd::Attach { thread, force } => attach(&socket, &thread, force).await,
        Cmd::Wait {
            threads,
            all,
            timeout,
            json,
        } => wait_cmd(&socket, &threads, all, timeout, json).await,
        Cmd::Mcp => {
            run_mcp(socket).await?;
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Boxes { cmd } => boxes::run(cmd, &config, &paths, &socket).await,
        Cmd::Account { cmd } => accounts::run(cmd, &paths, &socket)
            .await
            .map(|()| ExitCode::SUCCESS),
        Cmd::Speech {
            cmd:
                SpeechCmd::Feed {
                    text,
                    proto: _,
                    all_workspaces,
                },
        } => {
            if text {
                speech::text(speech::workspace_sockets(&paths, &[], all_workspaces)?).await?;
            } else {
                speech::proxy(&paths, all_workspaces, speech::proxy_idle()).await?;
            }
            Ok(ExitCode::SUCCESS)
        }
        cmd => run_simple(cmd, &config, paths, socket)
            .await
            .map(|()| ExitCode::SUCCESS),
    }
}

async fn run_simple(
    cmd: Cmd,
    config: &Config,
    paths: Paths,
    socket: PathBuf,
) -> anyhow::Result<()> {
    match cmd {
        Cmd::Attach { .. }
        | Cmd::Wait { .. }
        | Cmd::Mcp
        | Cmd::ClaudeMemoryGuard { .. }
        | Cmd::Runner { .. }
        | Cmd::Update { .. }
        | Cmd::Boxes { .. }
        | Cmd::Speech { .. }
        | Cmd::Speak { .. }
        | Cmd::Remote { .. }
        | Cmd::TerminalSetup { .. }
        | Cmd::Account { .. }
        | Cmd::Ws { .. } => {
            unreachable!("handled by run")
        }
        Cmd::Say { message } => say(&socket, &message.join(" ")).await,
        Cmd::Respond {
            target,
            answer,
            message,
            yes,
        } => {
            // Only a terminal can confirm: a pipe or script gets no prompt (it would answer a
            // question nobody read), so it must name the revision or pass --yes.
            let mut ask = ask_on_stderr;
            respond(&socket, &target, &answer, message, consent(yes, &mut ask)).await
        }
        Cmd::Attention { all, cmd: None } => attention(&socket, all).await,
        Cmd::Attention {
            cmd: Some(AttentionCmd::Dismiss { target }),
            ..
        } => dismiss(&socket, &target).await,
        Cmd::Orchestrator {
            cmd: OrchestratorCmd::Reset,
        } => {
            let r = connect(&socket)
                .await?
                .call("orchestrator.reset", json!({}))
                .await?;
            out!("{}", localize(&orchestrator_session_text(&r, true)));
            if let Some(w) = r["warning"].as_str() {
                errln!("twr: warning: {}", localize(w));
            }
            if let Some(e) = r["start_error"].as_str() {
                errln!("twr: the new orchestrator could not start yet: {e}");
            }
            Ok(())
        }
        Cmd::Orchestrator {
            cmd: OrchestratorCmd::Show { json },
        } => orchestrator_show(&socket, json).await,
        Cmd::Orchestrator {
            cmd:
                OrchestratorCmd::Use {
                    runtime,
                    model,
                    no_summary,
                    wait,
                },
        } => {
            // A terminal is asked whether to wait for a busy orchestrator; a script waits.
            let mut ask = |q: &str| -> anyhow::Result<String> {
                err!("{q}");
                std::io::stderr().flush()?;
                let mut line = String::new();
                std::io::stdin().read_line(&mut line)?;
                Ok(line)
            };
            let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
            let ask: Option<Ask<'_>> = if tty { Some(&mut ask) } else { None };
            orchestrator_use(&socket, &runtime, model, !no_summary, wait, ask).await
        }
        Cmd::Info { json } => info(&socket, json).await,
        Cmd::Usage {
            json,
            threads: true,
            since,
            by,
        } => usage_threads(&socket, json, since.as_deref(), by.as_deref()).await,
        Cmd::Usage { json, .. } => usage(&socket, json).await,
        Cmd::Hub { cmd: None } => run_hub(paths, socket).await,
        Cmd::Hub {
            cmd:
                Some(HubCmd::Stop {
                    timeout,
                    all: false,
                }),
        } => hub_stop(&socket, &paths, Duration::from_secs(timeout)).await,
        Cmd::Hub {
            cmd: Some(HubCmd::Stop { all: true, .. }),
        } => unreachable!("bulk stop handled before selecting a workspace"),
        Cmd::Thread {
            cmd: ThreadCmd::List { all },
        } => thread_list(&socket, all).await,
        Cmd::Thread {
            cmd:
                ThreadCmd::New {
                    project,
                    runtime,
                    account,
                    outside_pin,
                    box_name,
                    name,
                    mode,
                    model,
                    effort,
                    aliases,
                    no_worktree,
                    worktree,
                    base,
                },
        } => {
            check_outside_pin(outside_pin, account.as_deref(), runtime.as_deref())?;
            let project = project_param(config, &project, box_name.as_deref())?;
            let mut c = connect(&socket).await?;
            let mut params = json!({ "project": project, "aliases": aliases });
            if no_worktree {
                params["no_worktree"] = json!(true);
            }
            if worktree {
                params["worktree"] = json!(true);
            }
            if outside_pin {
                params["outside_pin"] = json!(true);
            }
            for (k, v) in [
                ("runtime", runtime),
                ("account", account),
                ("box", box_name),
                ("name", name),
                ("mode", mode),
                ("model", model),
                ("effort", effort),
                ("base", base),
            ] {
                if let Some(v) = v {
                    params[k] = json!(v);
                }
            }
            let t = c.call("thread.new", params).await?;
            let mut model = format!(
                " model={}",
                t["model"].as_str().unwrap_or("(runtime default)")
            );
            if let Some(e) = t["effort"].as_str() {
                model.push_str(&format!(" effort={e}"));
            }
            let command = match t["pty_command"].as_array().or(t["acp_command"].as_array()) {
                Some(argv) => format!(" command={}", Value::Array(argv.clone())),
                None => String::new(),
            };
            outln!(
                "created {} ({}) runtime={}{command}{model} worktree={}",
                str_of(&t["name"]),
                str_of(&t["id"]),
                accounts::runtime_label(&t),
                str_of(&t["worktree"])
            );
            if let Some(repo) = t["project_found"]["repo"].as_str() {
                outln!("found {} at {repo}", str_of(&t["project_found"]["name"]));
            }
            if let Some(w) = result_warning(&t) {
                errln!("{w}");
            }
            Ok(())
        }
        Cmd::Send {
            cancel: Some(id), ..
        } => {
            let mut c = connect(&socket).await?;
            let r = c.call("thread.send_cancel", json!({ "id": id })).await?;
            outln!(
                "dropped the prompt queued for {} ({})",
                r["thread_name"].as_str().unwrap_or("?"),
                r["cancelled"].as_str().unwrap_or("?")
            );
            Ok(())
        }
        Cmd::Send {
            thread: Some(thread),
            prompt,
            after,
            cancel: None,
        } if !after.is_empty() => {
            let mut c = connect(&socket).await?;
            let r = c
                .call(
                    "thread.send",
                    json!({ "thread": thread, "prompt": prompt.join(" "), "after": after }),
                )
                .await?;
            let names: Vec<&str> = r["after"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            outln!(
                "queued for {thread} until {} finish{} ({}); `twr{w} send --cancel {}` drops it",
                names.join(", "),
                if names.len() == 1 { "es" } else { "" },
                r["queued_after"].as_str().unwrap_or("?"),
                r["queued_after"].as_str().unwrap_or("?"),
                w = workspace_hint()
            );
            Ok(())
        }
        Cmd::Send { thread, prompt, .. } => {
            let thread = thread.unwrap_or_default();
            let mut c = connect(&socket).await?;
            let r = c
                .call(
                    "thread.send",
                    json!({ "thread": thread, "prompt": prompt.join(" ") }),
                )
                .await?;
            match r["turn"].as_u64() {
                Some(turn) => outln!("turn {turn} started on {thread}"),
                None if r["parked"].as_bool() == Some(true) => outln!(
                    "queued on {thread} ({} pending); it is parked: `twr{w} resume {thread}` runs it",
                    r["pending"],
                    w = workspace_hint()
                ),
                None => outln!("queued on {thread} ({} pending)", r["pending"]),
            }
            if let Some(w) = result_warning(&r) {
                errln!("{w}");
            }
            if let Some(e) = r["dispatch_error"].as_str() {
                errln!("twr: the prompt is queued, but dispatching it failed: {e}");
            }
            Ok(())
        }
        Cmd::Park {
            thread,
            keep,
            force,
        } => {
            let mut c = connect(&socket).await?;
            let r = c
                .call(
                    "thread.park",
                    json!({ "thread": thread, "keep": keep, "force": force }),
                )
                .await?;
            outln!("{}", park_outcome(&thread, &r));
            Ok(())
        }
        Cmd::Trim { threads } => {
            let mut c = connect(&socket).await?;
            let mut failed = false;
            for thread in threads {
                match c.call("thread.trim", json!({ "thread": thread })).await {
                    Ok(r) => outln!("{}", trim_outcome(&r)),
                    Err(e) => {
                        errln!("twr: {thread}: {e:#}");
                        failed = true;
                    }
                }
            }
            if failed {
                anyhow::bail!("not every thread was trimmed");
            }
            Ok(())
        }
        Cmd::Resume { thread } => {
            let mut c = connect(&socket).await?;
            let r = c.call("thread.resume", json!({ "thread": thread })).await?;
            outln!("{}", resume_outcome(&thread, &r));
            Ok(())
        }
        Cmd::Compact { thread, offers } => {
            let mut c = connect(&socket).await?;
            let mut params = json!({ "thread": thread });
            if let Some(o) = &offers {
                params["offers"] = json!(o);
            }
            let r = c.call("thread.compact", params).await?;
            outln!("{}", compact_outcome(&thread, &r));
            Ok(())
        }
        Cmd::Delete { threads, yes } => {
            // As for `twr respond`: only a terminal can confirm, else --yes.
            let mut ask = ask_on_stderr;
            delete(&socket, &threads, consent(yes, &mut ask)).await
        }
        Cmd::Trash {
            cmd: TrashCmd::List { json },
        } => trash_list(&socket, json).await,
        Cmd::Job { cmd } => job_cmd(&socket, cmd).await,
        Cmd::Log {
            cmd: LogCmd::Checkpoint { all, json },
        } => log_checkpoint(&paths, all, json).await,
        Cmd::Log { cmd } => log_cmd(&socket, cmd).await,
        Cmd::Trash {
            cmd: TrashCmd::Empty { items, all, yes },
        } => {
            // As for `twr delete`: only a terminal can confirm, else --yes.
            let mut ask = ask_on_stderr;
            trash_empty(&socket, &items, all, consent(yes, &mut ask)).await
        }
        Cmd::Trash {
            cmd: TrashCmd::Restore { item, as_name },
        } => {
            let mut c = connect(&socket).await?;
            let mut params = json!({ "item": item });
            if let Some(n) = as_name {
                params["as"] = json!(n);
            }
            let r = c.call("trash.restore", params).await?;
            outln!("{}", localize(str_of(&r["message"])));
            Ok(())
        }
        Cmd::Rename { thread, name } => {
            let mut c = connect(&socket).await?;
            let r = c
                .call("thread.rename", json!({ "thread": thread, "name": name }))
                .await?;
            outln!("{}", localize(str_of(&r["message"])));
            Ok(())
        }
        Cmd::Mode { thread, mode } => {
            let mut c = connect(&socket).await?;
            let r = c
                .call("thread.set_mode", json!({ "thread": thread, "mode": mode }))
                .await?;
            outln!("{}", mode_outcome(&thread, &r));
            if let Some(w) = result_warning(&r) {
                errln!("{w}");
            }
            if let Some(e) = r["dispatch_error"].as_str() {
                errln!("twr: the mode is set, but restarting the runtime failed: {e}");
            }
            Ok(())
        }
        Cmd::Model {
            thread,
            model,
            effort,
            runtime,
            account,
            outside_pin,
        } => {
            check_outside_pin(outside_pin, account.as_deref(), runtime.as_deref())?;
            let mut c = connect(&socket).await?;
            let t = c.call("thread.get", json!({ "thread": thread })).await?;
            if model.is_none() && effort.is_none() && runtime.is_none() && account.is_none() {
                outln!("{}", model_show(&t));
                return Ok(());
            }
            // The thread by its id, not the query: the pin question below is about this
            // thread, and the retry must move it whatever names change while it is open.
            let Some(id) = t["id"]
                .as_str()
                .filter(|i| !i.is_empty())
                .map(str::to_string)
            else {
                bail!("the hub did not say which thread {thread} is");
            };
            let mut params = json!({ "thread": id });
            for (k, v) in [
                ("model", model),
                ("effort", effort),
                ("runtime", runtime),
                ("account", account),
            ] {
                if let Some(v) = v {
                    params[k] = json!(v);
                }
            }
            if outside_pin {
                params["outside_pin"] = json!(true);
            }
            let r = match c.call("thread.set_model", params.clone()).await {
                Ok(r) => r,
                // A pin keeps the thread off that account: a terminal is asked whether to
                // leave it this once.
                Err(e)
                    if !outside_pin
                        && pin_refusal(&e)
                        && std::io::IsTerminal::is_terminal(&std::io::stdin()) =>
                {
                    errln!("twr: {}", localize(&format!("{e:#}")));
                    let answer = ask_on_stderr(&format!(
                        "move {} ({id}) anyway? [y/N] ",
                        str_of(&t["name"])
                    ))?;
                    if !matches!(answer.trim(), "y" | "Y" | "yes") {
                        bail!("not moved");
                    }
                    params["outside_pin"] = json!(true);
                    c.call("thread.set_model", params).await?
                }
                Err(e) => return Err(e),
            };
            outln!("{}", model_outcome(&thread, &r));
            if let Some(w) = result_warning(&r) {
                errln!("{w}");
            }
            if let Some(e) = r["dispatch_error"].as_str() {
                if r["runtime_changed"].as_bool() == Some(true) {
                    errln!("twr: the runtime is set, but stopping the previous one failed: {e}");
                } else {
                    errln!("twr: the model is set, but restarting the runtime failed: {e}");
                }
            }
            Ok(())
        }
        Cmd::Stop { thread } => {
            let mut c = connect(&socket).await?;
            let r = c.call("thread.stop", json!({ "thread": thread })).await?;
            if r["box_offline"].as_bool() == Some(true) {
                outln!("stopping {thread} once its box's runner reconnects (box offline)");
            } else if r["stopping"].as_bool() == Some(true) {
                outln!("stopping {thread}");
            } else {
                outln!("{thread} is stopped");
            }
            let dropped = r["dropped_prompts"].as_u64().unwrap_or(0);
            if dropped > 0 {
                outln!("dropped {dropped} queued prompt(s)");
            }
            Ok(())
        }
        Cmd::Events {
            thread,
            follow,
            since,
            hydrate,
            pretty,
        } => events(&socket, &thread, follow, since, hydrate, pretty).await,
        Cmd::Control {
            follow,
            since,
            pretty,
        } => control(&socket, follow, since, pretty).await,
        Cmd::Context {
            cmd: ContextCmd::Show { project },
        } => {
            let mut c = connect(&socket).await?;
            let r = c
                .call("context.read", json!({ "project": project }))
                .await?;
            let content = str_of(&r["content"]);
            if r["truncated"] == true {
                errln!(
                    "{project}: {} is {} bytes; showing its first {}",
                    str_of(&r["path"]),
                    r["file_bytes"],
                    r["bytes"]
                );
            }
            if content.is_empty() {
                errln!("{project} has no standing context ({})", str_of(&r["path"]));
            } else {
                out!("{content}");
                if !content.ends_with('\n') {
                    outln!();
                }
            }
            Ok(())
        }
        Cmd::Context {
            cmd: ContextCmd::Edit { project },
        } => context_edit(&socket, &project).await,
        Cmd::Memory { cmd } => memory(&socket, cmd).await,
    }
}

// --- memory ------------------------------------------------------------------------------------

/// `twr memory`.
async fn memory(socket: &Path, cmd: MemoryCmd) -> anyhow::Result<()> {
    match cmd {
        MemoryCmd::List { json } => {
            let r = connect(socket)
                .await?
                .call("memory.list", json!({}))
                .await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let notes = r["notes"].as_array().cloned().unwrap_or_default();
            if r["enabled"] == false {
                errln!("memory is off ([orchestrator] memory = false): the orchestrator neither gets nor changes these notes");
            }
            if notes.is_empty() {
                errln!("no notes ({})", str_of(&r["dir"]));
            }
            for n in &notes {
                let mark = if n["unconfirmed"] == true { "!" } else { " " };
                outln!(
                    "{mark} {:<28} {:<9} {:<10} {}",
                    str_of(&n["name"]),
                    str_of(&n["type"]),
                    str_of(&n["updated"]).get(..10).unwrap_or(""),
                    str_of(&n["description"])
                );
            }
            for u in r["unreadable"].as_array().into_iter().flatten() {
                errln!(
                    "{}.md cannot be read: {}",
                    str_of(&u["name"]),
                    str_of(&u["why"])
                );
            }
            if notes.iter().any(|n| n["unconfirmed"] == true) {
                errln!("! saved by the orchestrator in a turn you did not start");
            }
            Ok(())
        }
        MemoryCmd::Show { name } => {
            let n = connect(socket)
                .await?
                .call("memory.read", json!({ "name": name }))
                .await?;
            out!("{}", note_text(&n));
            outln!(
                "\n(rev {}, saved {} by {}{})",
                str_of(&n["rev"]),
                str_of(&n["updated"]),
                match str_of(&n["by"]) {
                    "" => "hand",
                    b => b,
                },
                match str_of(&n["origin"]) {
                    "" | "user" => String::new(),
                    o => format!(" in a {o} turn"),
                }
            );
            Ok(())
        }
        MemoryCmd::Edit { name } => memory_edit(socket, &name).await,
        MemoryCmd::Rm { names } => {
            let mut c = connect(socket).await?;
            for name in names {
                c.call("memory.forget", json!({ "name": name })).await?;
                outln!("forgot {name}");
            }
            Ok(())
        }
        MemoryCmd::Path => {
            let r = connect(socket)
                .await?
                .call("memory.list", json!({}))
                .await?;
            outln!("{}", str_of(&r["dir"]));
            Ok(())
        }
    }
}

/// A note as `twr memory show` prints it and `twr memory edit` edits it.
fn note_text(n: &Value) -> String {
    format!(
        "description: {}\ntype: {}\n---\n{}\n",
        str_of(&n["description"]),
        str_of(&n["type"]),
        str_of(&n["body"]).trim_end()
    )
}

/// The description, type and text of a note as [`note_text`] lays it out.
fn parse_note_text(text: &str) -> anyhow::Result<(String, String, String)> {
    let Some((head, body)) = text.split_once("\n---\n") else {
        bail!("keep the `---` line between the note's description and type and its text");
    };
    let (mut description, mut kind) = (String::new(), String::new());
    for line in head.lines() {
        match line.split_once(':') {
            Some(("description", v)) => description = v.trim().to_string(),
            Some(("type", v)) => kind = v.trim().to_string(),
            _ if line.trim().is_empty() => {}
            _ => bail!("only `description:` and `type:` lines go above the `---` line"),
        }
    }
    Ok((description, kind, body.trim().to_string()))
}

/// `twr memory edit`: edits a temporary copy of the note (a new one from a template) with
/// `$VISUAL` / `$EDITOR` and saves it with `memory.save` and the rev that was read, so a note
/// changed meanwhile is a conflict. On any failure after the editor wrote something, the
/// temporary file is kept and its path printed.
async fn memory_edit(socket: &Path, name: &str) -> anyhow::Result<()> {
    let read = connect(socket)
        .await?
        .call("memory.read", json!({ "name": name }))
        .await;
    let (before, rev) = match read {
        Ok(n) => (note_text(&n), n["rev"].as_str().map(str::to_string)),
        Err(e)
            if e.downcast_ref::<RpcError>()
                .is_some_and(|r| r.code == codes::NOT_FOUND) =>
        {
            ("description: \ntype: user\n---\n".to_string(), None)
        }
        Err(e) => return Err(e),
    };
    let tmp = TempFile::create("twr-memory", ".md", before.as_bytes())?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("twr-memory-edit")
        .arg(tmp.path())
        .status();
    let after = std::fs::read(tmp.path());
    let edited = after.as_ref().map_or(true, |a| a != before.as_bytes());
    let keep = |tmp: TempFile, why: String| -> anyhow::Error {
        if edited {
            anyhow!(
                "{why}; the note is unchanged (your edit is kept in {})",
                tmp.keep().display()
            )
        } else {
            anyhow!("{why}; the note is unchanged")
        }
    };
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => return Err(keep(tmp, format!("the editor `{editor}` exited with {s}"))),
        Err(e) => {
            return Err(keep(
                tmp,
                format!("running the editor `{editor}` failed ({e})"),
            ))
        }
    }
    let after = match after.map(String::from_utf8) {
        Ok(Ok(a)) => a,
        Ok(Err(_)) => return Err(keep(tmp, "the edited note is not UTF-8".into())),
        Err(e) => return Err(keep(tmp, format!("reading the edited note failed ({e})"))),
    };
    if after == before {
        outln!("{name}: no changes");
        return Ok(());
    }
    let (description, kind, body) = match parse_note_text(&after) {
        Ok(p) => p,
        Err(e) => return Err(keep(tmp, e.to_string())),
    };
    let saved = async {
        connect(socket)
            .await?
            .call(
                "memory.save",
                json!({ "name": name, "type": kind, "description": description,
                        "body": body, "rev": rev }),
            )
            .await
    }
    .await;
    match saved {
        Ok(r) => {
            outln!("{name}: {} (rev {})", str_of(&r["op"]), str_of(&r["rev"]));
            Ok(())
        }
        Err(e) => Err(keep(tmp, format!("saving the note failed: {e:#}"))),
    }
}

// --- context -----------------------------------------------------------------------------------

/// `twr context edit`: edits a temporary copy of the project's standing context with
/// `$VISUAL` / `$EDITOR` (run through `sh`, so it may carry arguments; `vi` when neither is
/// set) and sends it with `context.set` only when the editor exits 0 and the text changed.
///
/// The edit is never lost: the hub is reconnected after the editor exits (the first
/// connection may be long gone), the set carries the hash that was read (`expect_hash`, so a
/// concurrent change is a conflict, not overwritten), and on any failure after the editor
/// wrote something the temporary file is kept and its path printed.
async fn context_edit(socket: &Path, project: &str) -> anyhow::Result<()> {
    let r = connect(socket)
        .await?
        .call("context.read", json!({ "project": project }))
        .await?;
    let path = str_of(&r["path"]).to_string();
    if r["truncated"] == true {
        bail!(
            "{path} is {} bytes, over what twr edits ({}); edit the file itself",
            r["file_bytes"],
            r["bytes"]
        );
    }
    if r["utf8"] == false {
        bail!("{path} is not UTF-8; twr will not edit it (fix or remove the file)");
    }
    let before = r["content"].as_str().unwrap_or_default().to_string();
    let hash = r["hash"].as_str().map(str::to_string);
    let tmp = TempFile::create("twr-context", ".md", before.as_bytes())?;
    let editor = ["VISUAL", "EDITOR"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| "vi".to_string());
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("twr-context-edit")
        .arg(tmp.path())
        .status();
    // The temporary file is removed only when a successful read proves it unchanged: any
    // read failure keeps it (whatever the editor's status), since it may hold the only copy
    // of the edit.
    let after = match std::fs::read(tmp.path()) {
        Ok(a) => a,
        Err(e) => {
            let editor_note = match &status {
                Ok(s) if s.success() => String::new(),
                Ok(s) => format!("the editor `{editor}` exited with {s}; "),
                Err(e) => format!("running the editor `{editor}` failed ({e}); "),
            };
            let kept = tmp.keep();
            return Err(anyhow!(e)).with_context(|| {
                format!(
                    "{editor_note}reading the edited context failed; the context is unchanged \
                     (the edited copy, if any, is kept in {})",
                    kept.display()
                )
            });
        }
    };
    let edited = after != before.as_bytes();
    let status = match status {
        Ok(s) => s,
        Err(e) if edited => {
            let kept = tmp.keep();
            return Err(anyhow!(e)).with_context(|| {
                format!(
                    "running the editor `{editor}` failed; the context is unchanged (the edited \
                     copy is kept in {})",
                    kept.display()
                )
            });
        }
        Err(e) => {
            return Err(anyhow!(e)).with_context(|| format!("running the editor `{editor}`"));
        }
    };
    if !status.success() {
        if edited {
            bail!(
                "the editor `{editor}` exited with {status}; the context is unchanged (the edited \
                 copy is kept in {})",
                tmp.keep().display()
            );
        }
        bail!("the editor `{editor}` exited with {status}; the context is unchanged");
    }
    let after = match String::from_utf8(after) {
        Ok(a) => a,
        Err(_) => bail!(
            "the edited context is not UTF-8; nothing was saved (the edit is kept in {})",
            tmp.keep().display()
        ),
    };
    if after == before {
        outln!("{project}: no changes");
        return Ok(());
    }
    let saved = async {
        connect(socket)
            .await?
            .call(
                "context.set",
                json!({ "project": project, "content": after, "expect_hash": hash }),
            )
            .await
    }
    .await;
    let set = match saved {
        Ok(set) => set,
        Err(e) => {
            let kept = tmp.keep();
            let conflict = e
                .downcast_ref::<RpcError>()
                .is_some_and(|r| r.code == codes::CONFLICT);
            let why = if conflict {
                " (merge it with the current context and edit again)"
            } else {
                ""
            };
            return Err(e.context(format!(
                "saving the standing context failed; your edit is kept in {}{why}",
                kept.display()
            )));
        }
    };
    outln!(
        "{project}: standing context saved ({} bytes, {})",
        set["bytes"],
        str_of(&set["path"])
    );
    Ok(())
}

/// A private (0600) temporary file, removed when dropped unless kept ([`TempFile::keep`]).
struct TempFile(Option<PathBuf>);

impl TempFile {
    fn create(prefix: &str, suffix: &str, content: &[u8]) -> anyhow::Result<TempFile> {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for n in 0..100u32 {
            let path = dir.join(format!(
                "{prefix}-{}-{nanos}-{n}{suffix}",
                std::process::id()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut f) => {
                    let file = TempFile(Some(path));
                    f.write_all(content)
                        .with_context(|| format!("writing {}", file.path().display()))?;
                    return Ok(file);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("cannot create a temporary file in {}", dir.display())
    }

    fn path(&self) -> &Path {
        self.0.as_deref().unwrap_or(Path::new(""))
    }

    /// Leaves the file in place (it holds an edit that was not saved) and returns its path.
    fn keep(mut self) -> PathBuf {
        self.0.take().unwrap_or_default()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Some(p) = self.0.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or("-")
}

/// A configured project name is sent as is; an existing directory becomes an absolute path
/// (the hub resolves paths in its own working directory).
/// The `project` of a `thread.new` for what was typed: a configured project's name as is, a
/// directory of this machine as its absolute path, anything else as is (the hub looks it up).
/// For a remote box (`box_name` other than `local`), only the hub resolves it: a project
/// configured for that box (`[boxes.<box>.projects.<name>]`) is a name, and a path means a
/// path on the box, never a directory here.
fn project_param(config: &Config, project: &str, box_name: Option<&str>) -> anyhow::Result<String> {
    if config.projects.contains_key(project) {
        return Ok(project.to_string());
    }
    if box_name.is_some_and(|b| b != "local") {
        return Ok(project.to_string());
    }
    let p = Path::new(project);
    if p.is_dir() {
        let abs = std::fs::canonicalize(p).with_context(|| format!("resolving {project}"))?;
        return Ok(abs.to_string_lossy().into_owned());
    }
    Ok(project.to_string())
}

/// What `twr mode` prints for a `thread.set_mode` result.
fn mode_outcome(thread: &str, r: &Value) -> String {
    let name = r["thread_name"].as_str().unwrap_or(thread);
    let mode = str_of(&r["mode"]);
    if r["changed"].as_bool() != Some(true) {
        return format!("{name} is already in {mode} mode");
    }
    let when = match r["applies"].as_str() {
        Some("now") => "its runtime restarts on the same session now",
        Some("after_turn") => {
            "its runtime finishes the current turn, then restarts on the same session"
        }
        Some("never") => "pty threads apply no mode",
        _ => "its runtime starts in it next time",
    };
    format!(
        "{name}: {} -> {mode} ({when}{})",
        str_of(&r["previous"]),
        tasks_end(r)
    )
}

/// `; its background tasks end with it` when a mode or runtime change does not wait for the
/// runtime's background tasks (`stops_background_tasks`: it narrows the mode).
fn tasks_end(r: &Value) -> &'static str {
    if r["stops_background_tasks"].as_bool() == Some(true) {
        "; its background tasks end with it"
    } else {
        ""
    }
}

/// `model M, effort E` for a model and an effort (JSON strings, null: the runtime's default).
fn model_effort(model: &Value, effort: &Value) -> String {
    format!(
        "model {}, effort {}",
        model.as_str().unwrap_or("(runtime default)"),
        effort.as_str().unwrap_or("(runtime default)")
    )
}

/// What `twr model <thread>` prints for a thread object: its runtime, model and effort, the
/// model its runtime reports it runs, what a live runtime still runs with until it restarts
/// (`runtime_model` / `runtime_effort`), and a move to another runtime that its next turn
/// completes (`handoff_pending`).
fn model_show(t: &Value) -> String {
    let runtime = str_of(&t["runtime"]);
    // The account while its runtime shows account labels, or when it was removed.
    let account = match t["account"].as_str() {
        Some(a) if t["account_removed"].as_bool() == Some(true) => {
            format!(", account {a} (removed)")
        }
        Some(a) if t["account_shown"].as_bool() == Some(true) => format!(", account {a}"),
        _ => String::new(),
    };
    let mut out = format!(
        "{}: runtime {runtime}{account}, {}",
        str_of(&t["name"]),
        model_effort(&t["model"], &t["effort"])
    );
    if let Some(a) = t["model_actual"].as_str() {
        out.push_str(&format!(" (runs {a})"));
    }
    if t["handoff_pending"].as_bool() == Some(true) {
        // A live runtime now is the previous one, finishing its turn: it is not restarted.
        if t["live"].as_bool() == Some(true) {
            out.push_str("; its previous runtime stops once the current turn ends");
        }
        match t["resume_session"].as_str() {
            Some(s) => out.push_str(&format!(
                "; its next turn resumes its earlier {runtime} session {s}, told what the thread \
                 did since"
            )),
            None => out.push_str(&format!(
                "; its next turn starts a new {runtime} session, given a handoff of the previous \
                 one"
            )),
        }
        return out;
    }
    let pending = |k: &str| t.as_object().is_some_and(|o| o.contains_key(k));
    if pending("runtime_model") || pending("runtime_effort") {
        let model = if pending("runtime_model") {
            &t["runtime_model"]
        } else {
            &t["model"]
        };
        let effort = if pending("runtime_effort") {
            &t["runtime_effort"]
        } else {
            &t["effort"]
        };
        out.push_str(&format!(
            "; its runtime runs {} until it restarts",
            model_effort(model, effort)
        ));
    }
    out
}

/// Refuses `--outside-pin` without an account to leave the pin for: `--account <name>`, or
/// the account part of `--runtime <runtime>:<account>`.
fn check_outside_pin(
    outside_pin: bool,
    account: Option<&str>,
    runtime: Option<&str>,
) -> anyhow::Result<()> {
    let in_runtime = runtime.is_some_and(|r| twr_core::account::split_runtime(r).1.is_some());
    if outside_pin && account.is_none() && !in_runtime {
        bail!(
            "--outside-pin needs the account it is for: --account <name>, or --runtime \
             <runtime>:<account>"
        );
    }
    Ok(())
}

/// Whether `e` is the hub refusing a move because a pin keeps the thread off the account
/// (`thread.set_model` without `outside_pin`), which the user may override.
fn pin_refusal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>()
        .is_some_and(|r| r.message.contains("pass outside_pin"))
}

/// What `twr model` prints for a `thread.set_model` result.
/// The advisory of a `thread.new` / `thread.set_model` / `thread.set_mode` / `thread.send`
/// result `r` (its `warning`: the box reports the CLI of the runtime the call starts or
/// restarts missing; the thread was made, changed or sent to all the same), as the
/// line printed to stderr: `warning: <it>`, its commands run in this workspace ([`localize`]).
/// `errln!` cleans it for the terminal.
fn result_warning(r: &Value) -> Option<String> {
    let w = r["warning"].as_str().filter(|w| !w.trim().is_empty())?;
    Some(format!("warning: {}", localize(w)))
}

fn model_outcome(thread: &str, r: &Value) -> String {
    let name = r["thread_name"].as_str().unwrap_or(thread);
    let now = model_effort(&r["model"], &r["effort"]);
    if r["changed"].as_bool() != Some(true) {
        return format!("{name} already runs with {now}");
    }
    let before = model_effort(&r["previous_model"], &r["previous_effort"]);
    // A move to another account (and maybe runtime): the hub's message names both accounts.
    if r["account_changed"].as_bool() == Some(true) {
        let from = format!(
            "{}:{}",
            str_of(&r["previous_runtime"]),
            str_of(&r["previous_account"])
        );
        let old = match r["applies"].as_str() {
            Some("now") => format!("its {from} runtime stops now"),
            Some("after_turn") => format!("its {from} runtime stops once the current turn ends"),
            _ => format!("no {from} runtime is running"),
        };
        let message = match r["message"].as_str() {
            Some(m) => localize(m),
            None => format!(
                "{name} moves to {}:{}",
                str_of(&r["runtime"]),
                str_of(&r["account"])
            ),
        };
        return format!("{message} ({before} -> {now}; {old}{})", tasks_end(r));
    }
    if r["runtime_changed"].as_bool() == Some(true) {
        let (from, to) = (str_of(&r["previous_runtime"]), str_of(&r["runtime"]));
        let old = match r["applies"].as_str() {
            Some("now") => format!("its {from} runtime stops now"),
            Some("after_turn") => format!("its {from} runtime stops once the current turn ends"),
            _ => format!("no {from} runtime is running"),
        };
        return format!(
            "{name}: {from} ({before}) -> {to} ({now}); a new {to} session starts with its next \
             turn, given a handoff of the previous one ({old}{})",
            tasks_end(r)
        );
    }
    let when = match r["applies"].as_str() {
        Some("now") => "its runtime restarts on the same session now",
        Some("after_turn") => {
            "its runtime finishes the current turn, then restarts on the same session"
        }
        _ => "its runtime starts with them next time",
    };
    format!("{name}: {before} -> {now} ({when})")
}

/// What `twr park` prints for a `thread.park` result.
fn park_outcome(thread: &str, r: &Value) -> String {
    let flag = |k: &str| r[k].as_bool() == Some(true);
    let worktree = str_of(&r["worktree"]);
    let mut out = format!("parked {thread}");
    if flag("worktree_removed") {
        out.push_str(&format!("; removed worktree {worktree} (branch kept)"));
    } else if flag("kept_dirty") {
        let n = r["dirty_files"].as_u64().unwrap_or(0);
        out.push_str(&format!(
            "; kept worktree {worktree}: {n} uncommitted file(s)"
        ));
    } else if let Some(why) = r["kept_reason"].as_str() {
        out.push_str(&format!("; kept worktree {worktree}: {why}"));
    } else if flag("kept") {
        out.push_str(&format!("; kept worktree {worktree}"));
    } else if flag("worktree_missing") {
        out.push_str("; its worktree was already gone");
    }
    if let Some(e) = r["worktree_error"].as_str() {
        out.push_str(&format!("; could not remove the worktree: {e}"));
    }
    match r["pending"].as_u64().unwrap_or(0) {
        0 => {}
        n => out.push_str(&format!(
            "; {n} queued prompt(s) wait for `twr{} resume`",
            workspace_hint()
        )),
    }
    out
}

/// What `twr compact` prints for a `thread.compact` result.
fn compact_outcome(thread: &str, r: &Value) -> String {
    let name = r["thread_name"].as_str().unwrap_or(thread);
    if let Some(offers) = r["offers"].as_str() {
        return format!(
            "{name}: offers to compact its context once its prompt cache expired are {offers}"
        );
    }
    match (r["state"].as_str(), r["turn"].as_u64()) {
        (Some("started"), Some(turn)) => format!("{name}: compacting its context (turn {turn})"),
        _ => format!("{name}: compacts its context after its running turn"),
    }
}

/// What `twr resume` prints for a `thread.resume` result.
fn resume_outcome(thread: &str, r: &Value) -> String {
    let mut out = format!("resumed {thread}");
    if r["worktree_recreated"].as_bool() == Some(true) {
        out.push_str(&format!("; recreated worktree {}", str_of(&r["worktree"])));
    }
    if let Some(turn) = r["turn"].as_u64() {
        out.push_str(&format!("; turn {turn} started"));
    }
    out
}

/// `twr hub`: the hub of `paths` on `socket`, in the foreground. Its config is read again
/// under boxes.toml's lock ([`read_config_locked`]), held until the socket is bound.
async fn run_hub(paths: Paths, socket: PathBuf) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    twr_core::paths::ensure_private_dir(&paths.data_dir)?;
    // A named workspace's orchestrator works in a directory of its own (`Config::in_workspace`).
    if let Some(dir) = paths.orchestrator_dir() {
        twr_core::paths::ensure_private_dir(&dir)?;
    }
    let (config, boxes_lock) = {
        let paths = paths.clone();
        tokio::task::spawn_blocking(move || read_config_locked(&paths)).await??
    };
    twr_core::trace::mark("hub: config read");
    let mut opts = HubOptions::new(paths.db_file(), config);
    opts.boxes_lock = boxes_lock;
    opts.config_file = Some(paths.config_file.clone());
    opts.instance_name = paths.instance_name();
    opts.workspace = paths.workspace.clone();
    opts.workspaces_root = Some(paths.root().data_dir);
    // One pool of job slots for every workspace of this home.
    opts.job_slots_dir = Some(paths.root().data_dir.join("job-slots"));
    // A test's throwaway hub (under `TWR_HOME` only) trusts the test's clients: in a debug
    // build only, which the tests run and no installed twr is ([`test_token_trusted`]), and
    // only from a private file of the hub's user ([`hub_test_token`]).
    let trusted = test_token_trusted();
    if trusted {
        opts.test_user_token = hub_test_token();
    }
    // The hub's own agent CLIs (`hub.info` `agents`), probed in the background; never under
    // a test's throwaway hub, nor with `TWR_AGENT_PROBE=off`, so tests run none of the user's
    // real agent CLIs.
    if probes_own_agents(trusted, std::env::var_os(AGENT_PROBE_ENV).as_deref()) {
        opts.agent_probe = Some(twr_hub::hub::own_agent_probe());
        // The accounts' logins too (`account.status`), on the same terms.
        opts.account_probe = Some(twr_hub::hub::own_account_probe());
    }
    // Named accounts without a `dir` of their own: the home's root's, every workspace's.
    opts.accounts_root = Some(paths.accounts_dir());
    opts.worktree_namespace = paths.worktree_namespace();
    if trusted && std::env::var_os(HUB_STOP_GATE_ENV).is_some() {
        opts.socket_removed_hook = Some(std::sync::Arc::new(|| {
            Box::pin(async {
                let _ = tokio::task::spawn_blocking(|| hub_test_gate(HUB_STOP_GATE_ENV)).await;
            })
        }));
    }
    twr_hub::run(opts, socket).await
}

/// The config of `paths` as a starting hub reads it: under the lock of boxes.toml
/// ([`BoxesFile::lock`]), which the hub keeps until its socket is bound
/// ([`HubOptions::boxes_lock`]). `twr box remove` and `twr box token --rotate` edit boxes.toml
/// under that lock, then ask the hub on the socket to take the change (`box.token`); one that
/// finds no socket made its edit before this read, never between this read and the bind,
/// where the hub would start with the old token and nobody would tell it. The lock is waited
/// for: those commands hold it only while they read, check and save the file, never while
/// they wait for a hub or for the user. A lock file that can't be opened (a read-only config
/// directory) is logged and the hub starts without it: no twr command can edit boxes.toml
/// there either, since each takes the lock first.
fn read_config_locked(
    paths: &Paths,
) -> anyhow::Result<(Config, Option<twr_core::config::BoxesLock>)> {
    let file = paths.boxes_file();
    let lock = BoxesFile::lock(&file)
        .inspect_err(|e| {
            let file = file.display();
            tracing::warn!(error = %format!("{e:#}"), "starting without the lock of {file}");
        })
        .ok();
    let config = Config::load_from(paths)?;
    hub_test_gate(HUB_START_GATE_ENV);
    Ok((config, lock))
}

/// `TWR_TEST_HUB_START_GATE=<file>`, for tests of a starting hub: right after it read its
/// config ([`read_config_locked`]), the hub waits at the gate ([`hub_test_gate`]).
const HUB_START_GATE_ENV: &str = "TWR_TEST_HUB_START_GATE";

/// `TWR_TEST_HUB_STOP_GATE=<file>`, for tests of a hub shutting down: right after it removed
/// its socket ([`twr_hub::HubOptions::socket_removed_hook`]), the hub waits at the gate
/// ([`hub_test_gate`]).
const HUB_STOP_GATE_ENV: &str = "TWR_TEST_HUB_STOP_GATE";

/// The test gate `<file>` that `env` names: the hub creates `<file>.reached` and waits while
/// `<file>` exists (at most a minute). Only in a test's throwaway hub
/// ([`test_token_trusted`]): ignored by an installed twr.
fn hub_test_gate(env: &str) {
    let Some(gate) = std::env::var_os(env).filter(|_| test_token_trusted()) else {
        return;
    };
    let gate = PathBuf::from(gate);
    let mut reached = gate.clone().into_os_string();
    reached.push(".reached");
    if let Err(e) = std::fs::write(&reached, b"") {
        tracing::warn!(error = %e, "{env}: cannot create {}", Path::new(&reached).display());
        return;
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while gate.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `TWR_AGENT_PROBE`: `off` keeps the hub from probing its own box's agent CLIs (no
/// `claude`, `codex` or `grok` runs; `hub.info` reports no local agents). Any other value, or
/// none: it probes them. For tests and troubleshooting.
const AGENT_PROBE_ENV: &str = "TWR_AGENT_PROBE";

/// Whether the hub probes its own agent CLIs: not for a test's throwaway hub (`test`,
/// [`test_token_trusted`]), nor when [`AGENT_PROBE_ENV`] is `off` (`value`).
fn probes_own_agents(test: bool, value: Option<&std::ffi::OsStr>) -> bool {
    !test && value.is_none_or(|v| v != "off")
}

/// The test user token a test's throwaway hub trusts ([`twr_core::client::TEST_USER_TOKEN_ENV`]),
/// from a file [`private_token_file`] accepts for this process's user; any other is ignored
/// (logged): a file someone else could have written, or swapped for a link, makes no one the
/// user.
fn hub_test_token() -> Option<String> {
    let path = std::env::var_os(twr_core::client::TEST_USER_TOKEN_ENV)?;
    let uid = twr_core::paths::effective_uid();
    match private_token_file(Path::new(&path), uid) {
        Ok(token) => Some(token),
        Err(why) => {
            tracing::warn!(path = %Path::new(&path).display(), "ignoring the test user token: {why}");
            None
        }
    }
}

/// The token in `path` when it is a regular file (not a symbolic link) owned by `uid` with no
/// group or other permission bits, holding a token ([`twr_core::client::user_token_of`]); else
/// why not. The checks are made on the file opened (`fstat`), opened without following a
/// symlink and without blocking on a FIFO ([`twr_hub::hub::OPEN_NOFOLLOW_NONBLOCK`]), and it must
/// be the file inspected before (device and inode): a swap or a permission change in between
/// is refused, not read.
fn private_token_file(path: &Path, uid: u32) -> Result<String, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("cannot read it: {e}"))?;
    if !meta.file_type().is_file() {
        return Err("it is not a regular file".into());
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(twr_hub::hub::OPEN_NOFOLLOW_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot open it: {e}"))?;
    let opened = file
        .metadata()
        .map_err(|e| format!("cannot read it: {e}"))?;
    if (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err("it changed while it was read".into());
    }
    if !opened.file_type().is_file() {
        return Err("it is not a regular file".into());
    }
    if opened.uid() != uid {
        return Err(format!("it is owned by uid {}, not {uid}", opened.uid()));
    }
    if opened.mode() & 0o077 != 0 {
        return Err(format!(
            "its mode {:o} lets others than its owner read or write it (make it 0600)",
            opened.mode() & 0o777
        ));
    }
    let mut text = String::new();
    file.take(4096)
        .read_to_string(&mut text)
        .map_err(|e| format!("cannot read it: {e}"))?;
    twr_core::client::user_token_of(&text).ok_or_else(|| "it holds no token".into())
}

/// Whether connecting failed because the socket's backlog is full (`EAGAIN`) or the connect
/// timed out: a hub listens there, too busy to accept yet.
fn backlog_full(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
}

/// Whether connecting failed because nothing listens at the socket (no socket file, or a stale
/// one nobody accepts on): no hub is running there.
fn nothing_listens(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|c| c.downcast_ref::<std::io::Error>())
        .any(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
}

/// What `twr hub stop` makes of one look at the hub's process ([`twr_runner::reap::Alive`]):
/// `Ok(Some(line))` when it is done (the hub exited), `Ok(None)` to look again, an error when
/// `past_deadline` and it may still run. A process table that cannot be read counts as running:
/// never "stopped" on a guess, so a second hub is not started next to it.
fn hub_stop_step(
    alive: Option<twr_runner::reap::Alive>,
    past_deadline: bool,
    pid: u32,
    timeout: Duration,
    log: &Path,
) -> anyhow::Result<Option<String>> {
    use twr_runner::reap::Alive;
    match alive {
        Some(Alive::No) => Ok(Some(format!("hub stopped (pid {pid})"))),
        _ if !past_deadline => Ok(None),
        Some(Alive::Yes) => bail!(
            "the hub (pid {pid}) is still shutting down after {}s (it stops its runtimes \
             first); see {}",
            timeout.as_secs(),
            log.display()
        ),
        Some(Alive::Unknown) => bail!(
            "can't tell whether the hub (pid {pid}) exited (the process table is unreadable); \
             check before starting another; see {}",
            log.display()
        ),
        None => bail!(
            "could not confirm whether the hub (pid {pid}) exited within {}s (the process \
             check did not finish before the deadline); check before starting another; see {}",
            timeout.as_secs(),
            log.display()
        ),
    }
}

/// `twr hub stop`: asks the hub to shut down (`hub.shutdown`) and waits, within `timeout`, for
/// its process (the pid `hub.info` reports) to exit. The socket cannot tell: the hub removes it
/// as soon as the shutdown starts, before it stops its runtimes.
async fn hub_stop(socket: &Path, paths: &Paths, timeout: Duration) -> anyhow::Result<()> {
    let result = hub_stop_result(
        socket,
        paths,
        timeout,
        EXPECTED_WORKSPACE.get().map(String::as_str),
        false,
    )
    .await?;
    match result {
        Some(done) => outln!("{done}"),
        None => outln!("no hub is running at {}", socket.display()),
    }
    Ok(())
}

fn hub_stop_timeout(socket: &Path, paths: &Paths, timeout: Duration) -> anyhow::Error {
    anyhow!(
        "the hub at {} did not stop within {}s; it may be starting, shutting down, or using \
         another --socket (stop that hub separately with `twr --socket <path> hub stop`); see {}",
        socket.display(),
        timeout.as_secs(),
        paths.hub_log().display()
    )
}

/// Shared shutdown, with an explicit workspace identity for bulk stops (the global
/// EXPECTED_WORKSPACE belongs to single-workspace commands). One deadline bounds every
/// phase, including connecting and process-table reads, with the phase's own diagnostics;
/// it moves on by as long as writing this build's checkpoint took, unless the hub runs this
/// very build ([`prewarm_for_stop`], between `hub.info` and `hub.shutdown` on one connection).
async fn hub_stop_result(
    socket: &Path,
    paths: &Paths,
    timeout: Duration,
    expected: Option<&str>,
    wait_start: bool,
) -> anyhow::Result<Option<String>> {
    let mut deadline = tokio::time::Instant::now()
        .checked_add(timeout)
        .context("hub stop timeout is too large")?;
    let mut connection_losses = 0;
    let mut prewarmed = false;
    let proc = loop {
        let connected = tokio::time::timeout_at(deadline, async {
            loop {
                match Client::connect(socket).await {
                    Ok(c) => return Ok(Some(c)),
                    Err(e) if nothing_listens(&e) => {
                        if !wait_start {
                            return Ok(None);
                        }
                        // Every hub claims this lock before binding and holds it through
                        // shutdown, whatever its socket. Absence requires a free lock and
                        // another socket check under it. A later startup is outside this scan.
                        match stop_workspace_lock(paths)? {
                            StopWorkspaceLock::Missing => return Ok(None),
                            StopWorkspaceLock::Free(_lock) => match Client::connect(socket).await {
                                Ok(c) => return Ok(Some(c)),
                                Err(e) if nothing_listens(&e) => return Ok(None),
                                Err(e) => return Err(e),
                            },
                            StopWorkspaceLock::Held => {}
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    // Its backlog is full: a hub listens, busy. Try again.
                    Err(e) if backlog_full(&e) => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(e) => return Err(e),
                }
            }
        })
        .await;
        let mut c = match connected {
            Ok(result) => match result? {
                Some(c) => c,
                None => return Ok(None),
            },
            Err(_) if wait_start => return Err(hub_stop_timeout(socket, paths, timeout)),
            Err(_) => bail!(
                "connecting to the hub at {} timed out within {}s",
                socket.display(),
                timeout.as_secs()
            ),
        };
        let mut shutdown_sent = false;
        let mut known_proc = None;
        let info = tokio::time::timeout_at(deadline, async {
            let hello = c.hello(json!({ "client": "cli" })).await?;
            if let Some(expected) = expected {
                check_workspace_name(&hello, socket, expected)?;
            }
            c.call("hub.info", Value::Null).await
        })
        .await;
        // The hub's process, read while it surely runs (as it just answered): once it is gone,
        // the pid may name another process. Before the prewarm below, which may take seconds.
        let pid = match &info {
            Ok(Ok(info)) => info["pid"].as_u64().and_then(|p| u32::try_from(p).ok()),
            _ => None,
        };
        let start = match pid {
            Some(pid) => tokio::time::timeout_at(
                deadline,
                tokio::task::spawn_blocking(move || twr_runner::reap::process_start_time(pid)),
            )
            .await
            .ok()
            .and_then(Result::ok)
            .flatten(),
            None => None,
        };
        if let (Ok(Ok(info)), false) = (&info, prewarmed) {
            prewarmed = true;
            let started = tokio::time::Instant::now();
            if let Some(line) = prewarm_for_stop(info, paths).await {
                outln!("{line}");
            }
            deadline += started.elapsed();
        }
        let asked = match info {
            Ok(Ok(_)) => {
                tokio::time::timeout_at(deadline, async {
                    known_proc = pid.map(|pid| (pid, start));
                    shutdown_sent = true;
                    c.call("hub.shutdown", Value::Null).await?;
                    anyhow::Ok(known_proc)
                })
                .await
            }
            Ok(Err(e)) => Ok(Err(e)),
            Err(elapsed) => Err(elapsed),
        };
        let proc = match asked {
            Ok(Err(e))
                if wait_start
                    && shutdown_sent
                    && known_proc.is_some()
                    && stop_connection_lost(&e) =>
            {
                known_proc
            }
            Ok(Err(e)) if wait_start && !shutdown_sent && stop_connection_lost(&e) => {
                connection_losses += 1;
                if connection_losses >= 2 {
                    return Err(e).context(
                        "the hub keeps closing connections before shutdown can be requested",
                    );
                }
                // The kernel may accept a connection just before serve drops its listener.
                // Reconnect under the same deadline and wait for the lock if shutdown began.
                drop(c);
                tokio::time::sleep_until(
                    deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
                )
                .await;
                continue;
            }
            Ok(r) => r?,
            // The request may have reached the hub even though its reply missed the
            // deadline. Its identity and pid are known: confirm exit rather than calling
            // a hub that answered hello and info unresponsive.
            Err(_) if shutdown_sent && known_proc.is_some() => known_proc,
            Err(_) => bail!(
                "the hub at {} accepts connections but did not answer within {}s; see {}",
                socket.display(),
                timeout.as_secs(),
                paths.hub_log().display()
            ),
        };
        drop(c);
        break proc;
    };
    let Some((pid, start)) = proc else {
        if wait_start {
            bail!("shutdown requested but the hub did not report its pid, so twr cannot confirm it exited");
        }
        return Ok(Some(
            "the hub is shutting down (it did not report its pid, so twr cannot wait for it)"
                .into(),
        ));
    };
    wait_for_stopped_hub(pid, timeout, &paths.hub_log(), deadline, || async move {
        tokio::task::spawn_blocking(move || twr_runner::reap::process_liveness(pid, start))
            .await
            .unwrap_or(twr_runner::reap::Alive::Unknown)
    })
    .await
    .map(Some)
}

/// Whether `twr hub stop` writes this build's checkpoint of the log at `db` before stopping a
/// hub that answers `hub.info` with `info` ([`prewarm_for_stop`]): unless that hub runs this
/// very build (its `checkpoint_build`, the build its checkpoints are for, is `this`), as the
/// start that follows is likely this build's, which uses only its own checkpoints; on that log.
/// A hub from before `checkpoint_build` may be another build of the same version (a dirty
/// checkout's): it gets one too. (A build that has one up to date writes nothing.)
fn prewarm_wanted(info: &Value, this: &str, db: &Path) -> bool {
    info.is_object()
        && info["checkpoint_build"].as_str() != Some(this)
        && info["db"].as_str().is_none_or(|d| Path::new(d) == db)
}

/// Before `twr hub stop` asks the hub of workspace `paths` that answered `hub.info` with `info`
/// to stop: when it runs another version of twr ([`prewarm_wanted`]), writes this build's
/// checkpoint of its log, as `twr log checkpoint` does ([`twr_hub::prewarm_checkpoint`]), so
/// that the start that follows on this build folds only what came after it. (A running older
/// hub keeps only its own newest checkpoints, and may have dropped one written at install.)
/// Best effort: the line saying what it did, or `None` when it did nothing (the same version,
/// another log, an agent asking).
async fn prewarm_for_stop(info: &Value, paths: &Paths) -> Option<String> {
    // An agent may not stop the hub (the hub refuses it): it writes nothing either.
    if caller_is_agent() {
        return None;
    }
    let db = paths.db_file();
    if !prewarm_wanted(info, &twr_hub::checkpoint_build(), &db) {
        return None;
    }
    let name = paths.workspace_name().to_string();
    let notify = Config::load_from(paths)
        .map(|c| c.notify)
        .unwrap_or_default();
    let done = tokio::task::spawn_blocking(move || twr_hub::prewarm_checkpoint(&db, &notify)).await;
    Some(match done {
        Ok(Ok(p)) => prewarm_text(&prewarm_json(&name, &p)),
        Ok(Err(e)) => format!(
            "{name}: no checkpoint written for twr {} ({e:#}): its next start folds the whole log",
            twr_core::version()
        ),
        Err(e) => format!("{name}: no checkpoint written ({e})"),
    })
}

fn hub_stop_failure(name: &str, e: &anyhow::Error) -> String {
    let reason = twr_core::paths::localize_commands(&format!("{e:#}"), &format!(" -w {name}"));
    format!("{name}: failed: {reason}")
}

/// The last observation gets a short independent bound, including when the shutdown
/// reply arrived at the deadline. `None` means an actual check was started but timed out.
async fn wait_for_stopped_hub<F, Fut>(
    pid: u32,
    timeout: Duration,
    log: &Path,
    deadline: tokio::time::Instant,
    mut read_liveness: F,
) -> anyhow::Result<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = twr_runner::reap::Alive>,
{
    loop {
        let now = tokio::time::Instant::now();
        let final_check = now >= deadline;
        let read_deadline = if final_check {
            now + HUB_STOP_FINAL_CHECK
        } else {
            deadline
        };
        let alive = tokio::time::timeout_at(read_deadline, read_liveness())
            .await
            .ok();
        if alive.is_none() && !final_check {
            continue;
        }
        let past = tokio::time::Instant::now() >= deadline;
        if let Some(done) = hub_stop_step(alive, past, pid, timeout, log)? {
            return Ok(done);
        }
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(100)),
        )
        .await;
    }
}

/// Only transport loss before a shutdown request is retried. RPC refusals and workspace
/// mismatches must remain failures; retrying them would hide safety checks.
fn stop_connection_lost(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<RpcError>().is_some() {
        return false;
    }
    if e.downcast_ref::<twr_core::client::HubClosed>().is_some() {
        return true;
    }
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|e| {
            matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
    })
}

/// Default plus the same named directories as `twr ws`. Each stop has its own deadline
/// and task, so even a hub that never answers cannot delay shutdown of the others.
async fn hub_stop_all(root: &Paths, timeout: Duration) -> anyhow::Result<()> {
    let config = Config::load_from(root);
    let mut failed = 0;
    let mut names = vec![DEFAULT_WORKSPACE.to_string()];
    match root
        .named_workspaces()
        .with_context(|| format!("listing {}", root.workspaces_dir().display()))
    {
        Ok(named) => names.extend(named),
        Err(e) => {
            outln!("workspaces: failed: {e:#}");
            failed += 1;
        }
    }
    let mut stops = tokio::task::JoinSet::new();
    for name in names {
        let ws = root.workspace(&name)?;
        let socket = if name == DEFAULT_WORKSPACE {
            match &config {
                Ok(config) => config.socket_path(&ws),
                Err(e) => {
                    outln!("{}", hub_stop_failure(&name, e));
                    failed += 1;
                    continue;
                }
            }
        } else {
            ws.socket.clone()
        };
        stops.spawn(async move {
            let result = async {
                check_socket_path(&socket)?;
                if name == DEFAULT_WORKSPACE {
                    check_socket_layout(&ws)?;
                }
                hub_stop_result(&socket, &ws, timeout, Some(&name), true).await
            }
            .await;
            (name, result)
        });
    }
    while let Some(result) = stops.join_next().await {
        match result {
            Ok((name, Ok(Some(done)))) => {
                outln!("{name}: {}", done.strip_prefix("hub ").unwrap_or(&done));
            }
            Ok((name, Ok(None))) => outln!("{name}: not running"),
            Ok((name, Err(e))) => {
                outln!("{}", hub_stop_failure(&name, &e));
                failed += 1;
            }
            Err(e) => {
                errln!("twr: hub stop task failed: {e}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} failure(s) while stopping workspace hubs");
    }
    Ok(())
}

// --- update ------------------------------------------------------------------------------------

/// `twr update`: picks the release ([`update_source`]), downloads its installer and checksums
/// and checks both signatures ([`fetch_verified`]) and that both are of that release
/// ([`release_of`]), then runs the installer with `sh` (output on this terminal) to install
/// that release into this executable's directory, with the verified checksums. Nothing runs
/// unless all of that holds. Then, when a hub runs another version than the new `twr`, says
/// how to restart it; it never restarts the hub itself (that stops every thread's runtime).
async fn update(
    version: Option<String>,
    socket: Option<PathBuf>,
    paths: &Paths,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("locating the twr executable")?;
    let exe = exe
        .canonicalize()
        .with_context(|| format!("resolving {}", exe.display()))?;
    let dir = install_dir(&exe, &cargo_bin_dirs(|k| std::env::var_os(k)))?;
    let key = release_key(test_token_trusted(), std::env::var_os(TEST_RELEASE_KEY_ENV))?;
    let mirror = std::env::var(INSTALL_SCRIPT_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty());
    let source = update_source(version.as_deref(), mirror).await?;
    let (script, sums) = fetch_verified(&source, &key).await?;
    let tag = release_of(&source, &script, &sums)?;
    check_script(&source.script, &script)?;
    let tmp = PrivateDir::create("twr-update")?;
    let script_file = tmp.write("install.sh", &script)?;
    let sums_file = tmp.write("sha256sums.txt", &sums)?;
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(&script_file)
        .env("TWR_INSTALL_DIR", &dir)
        .env("TWR_SHA256SUMS_FILE", &sums_file)
        .env("TWR_UPDATE", "1")
        // The release the checksums name: the one asked for, or a mirror's.
        .env("TWR_VERSION", &tag);
    // From a mirror, the archive comes from the mirror too: the release its checksums are of.
    if let Some(archives) = &source.archives {
        cmd.env("TWR_DOWNLOAD_URL", archives);
    }
    let mut child = cmd.spawn().context("running the installer with sh")?;
    let status = loop {
        tokio::select! {
            s = child.wait() => break s.context("waiting for the installer")?,
            // Ctrl-C reaches the installer too (same process group): it cleans up and exits,
            // and twr waits for that instead of dying first.
            r = tokio::signal::ctrl_c() => if r.is_err() {
                break child.wait().await.context("waiting for the installer")?;
            },
        }
    };
    drop(tmp);
    if !status.success() {
        bail!("the installer failed ({status}); twr was not updated");
    }
    prewarm_installed(&dir.join("twr")).await;
    hub_version_note(&dir.join("twr"), socket, paths).await;
    Ok(())
}

/// Where `twr update` gets the release: `tag` (for install.sh's `TWR_VERSION`), the installer's
/// URL `script` (its signature is `<script>.sig`), `base`, the directory URL of sha256sums.txt
/// and its signature, and `archives`, where install.sh downloads the archive from
/// (`TWR_DOWNLOAD_URL`; `None`: the GitHub release of `tag`).
#[derive(Clone, Debug, PartialEq)]
struct UpdateSource {
    tag: Option<String>,
    script: String,
    base: String,
    archives: Option<String>,
}

/// The release `twr update` installs, resolved once so that the installer, the checksums and
/// the archive all come from one release: `version` normalized as install.sh normalizes
/// `TWR_VERSION`, else the latest release ([`latest_tag`]). With a `mirror` (an installer URL,
/// [`INSTALL_SCRIPT_URL_ENV`]) everything comes from the mirror's directory instead, and
/// `version`, if given, is the release its checksums must name ([`release_of`]).
async fn update_source(
    version: Option<&str>,
    mirror: Option<String>,
) -> anyhow::Result<UpdateSource> {
    let tag = match version.and_then(release_version) {
        Some(tag) if !is_release_tag(&tag) => {
            bail!("{tag} is not a release version (e.g. v0.2.0 or v0.2.0-rc.1)")
        }
        tag => tag,
    };
    if let Some(script) = mirror {
        let base = script
            .split_once("://")
            .filter(|(_, rest)| rest.contains('/'))
            .and_then(|_| script.rsplit_once('/'))
            .filter(|(_, name)| !name.is_empty())
            .map(|(base, _)| base.to_string())
            .ok_or_else(|| {
                anyhow!("{INSTALL_SCRIPT_URL_ENV}={script} does not name a file in a directory")
            })?;
        return Ok(UpdateSource {
            tag,
            script,
            archives: Some(base.clone()),
            base,
        });
    }
    let tag = match tag {
        Some(tag) => tag,
        None => latest_tag().await?,
    };
    let base = format!("{GITHUB}{RELEASES_PATH}/download/{tag}");
    Ok(UpdateSource {
        tag: Some(tag),
        script: format!("{base}/install.sh"),
        base,
        archives: None,
    })
}

/// `--version` as install.sh reads `TWR_VERSION`: empty or `latest` means the latest release
/// (`None`); a missing leading `v` is added (`0.2.0` is `v0.2.0`).
fn release_version(version: &str) -> Option<String> {
    match version {
        "" | "latest" => None,
        v if v.starts_with('v') => Some(v.to_string()),
        v => Some(format!("v{v}")),
    }
}

/// Whether `tag` is a release tag, `^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$`: safe to put in
/// a URL path as it is.
fn is_release_tag(tag: &str) -> bool {
    let Some(rest) = tag.strip_prefix('v') else {
        return false;
    };
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (rest, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers
            .iter()
            .all(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

/// The latest release's tag: `<releases>/latest` redirects to `<releases>/tag/<tag>` (no API
/// token needed). The redirect is read, not followed.
async fn latest_tag() -> anyhow::Result<String> {
    let url = format!("{GITHUB}{RELEASES_PATH}/latest");
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(RELEASE_FILE_TIMEOUT)
        .build()
        .context("setting up the HTTP client")?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("looking up the latest release at {url}"))?;
    let status = resp.status();
    if !status.is_redirection() {
        bail!("looking up the latest release at {url} failed: HTTP {status}, not a redirect");
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|l| l.to_str().ok())
        .ok_or_else(|| anyhow!("{url} redirected without a Location"))?;
    tag_of_release_url(location)
}

/// The tag in `location`, the URL `<releases>/latest` redirects to: `<releases>/tag/<tag>`,
/// absolute or from the host's root.
fn tag_of_release_url(location: &str) -> anyhow::Result<String> {
    let path = location.strip_prefix(GITHUB).unwrap_or(location);
    let tag = path
        .strip_prefix(RELEASES_PATH)
        .and_then(|p| p.strip_prefix("/tag/"))
        .ok_or_else(|| {
            anyhow!(
                "the latest release redirects to {location}, not to a release (are there no \
                 releases yet?)"
            )
        })?;
    if !is_release_tag(tag) {
        bail!("the latest release is {tag:?}, which is not a release version");
    }
    Ok(tag.to_string())
}

/// The key `twr update` verifies signatures with: [`RELEASE_KEY`], or, when `test_build`
/// ([`test_token_trusted`]: a debug build on a test `TWR_HOME`), the PEM public key at
/// `test_key` ([`TEST_RELEASE_KEY_ENV`]).
fn release_key(test_build: bool, test_key: Option<std::ffi::OsString>) -> anyhow::Result<[u8; 32]> {
    if test_build {
        if let Some(path) = test_key.filter(|p| !p.is_empty()) {
            let path = PathBuf::from(path);
            let pem = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            return ed25519_public_key(&pem).with_context(|| format!("in {}", path.display()));
        }
    }
    Ok(RELEASE_KEY)
}

/// The raw key in a PEM Ed25519 public key (SubjectPublicKeyInfo, as `openssl pkey -pubout`
/// writes it).
fn ed25519_public_key(pem: &str) -> anyhow::Result<[u8; 32]> {
    // The DER of an Ed25519 SubjectPublicKeyInfo up to the key: SEQUENCE { SEQUENCE { OID
    // 1.3.101.112 }, BIT STRING (33 bytes, no unused bits) }.
    const PREFIX: [u8; 12] = [
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    let pem = pem.trim();
    let body = pem
        .strip_prefix("-----BEGIN PUBLIC KEY-----")
        .and_then(|b| b.strip_suffix("-----END PUBLIC KEY-----"))
        .ok_or_else(|| anyhow!("not a PEM public key"))?;
    let body: String = body.split_whitespace().collect();
    let der = twr_core::b64::decode(&body).context("not a PEM public key")?;
    der.strip_prefix(&PREFIX[..])
        .and_then(|k| <[u8; 32]>::try_from(k).ok())
        .ok_or_else(|| anyhow!("not an Ed25519 public key"))
}

/// Downloads the installer and the checksums of `source` with their signatures, and checks
/// each signature against `key`. Returns (installer, checksums).
async fn fetch_verified(
    source: &UpdateSource,
    key: &[u8; 32],
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let [_, _, sums, sums_sig] = RELEASE_FILES.map(|f| format!("{}/{f}", source.base));
    let script_sig = format!("{}.sig", source.script);
    let script = fetch_release_file(&source.script).await?;
    let signature = fetch_signature(&script_sig).await?;
    check_signature(key, &script, &signature, &source.script, &script_sig)?;
    let sums_body = fetch_release_file(&sums).await?;
    let signature = fetch_signature(&sums_sig).await?;
    check_signature(key, &sums_body, &signature, &sums, &sums_sig)?;
    Ok((script, sums_body))
}

/// Downloads the signature at `url`. One that is not there (HTTP 404, a `file://` path that
/// does not exist, [`is_missing`]) says what that means: every release is signed, so its files
/// were changed or are incomplete. Any other failure (a timeout, a server error) is only
/// itself.
async fn fetch_signature(url: &str) -> anyhow::Result<Vec<u8>> {
    match fetch_release_file(url).await {
        Err(e) if is_missing(&e) => Err(e.context(format!(
            "{url}: no signature. Every twr release is signed, so this release's files were \
             changed or are incomplete; nothing was run"
        ))),
        r => r,
    }
}

/// The line of sha256sums.txt that names its release (`# twr release <tag>`), which the release
/// workflow writes before it signs the file.
const RELEASE_LINE: &str = "# twr release ";

/// The release `sums` (a sha256sums.txt whose signature verified) and `script` (the installer)
/// are of: the tag `sums` names on its [`RELEASE_LINE`], which must be the one `source` asked
/// for, if it named one, while `sums` must list `script`'s SHA-256 as `install.sh`. So signed
/// files of another release, or the installer of one with the checksums of another, are
/// refused.
fn release_of(source: &UpdateSource, script: &[u8], sums: &[u8]) -> anyhow::Result<String> {
    let url = format!("{}/sha256sums.txt", source.base);
    let tag = sums_release(source, sums)?;
    let listed = listed_sha256(sums, "install.sh");
    let actual = sha256_hex(script);
    match listed {
        None => bail!("{url} lists no install.sh; nothing was run"),
        Some(hash) if !hash.eq_ignore_ascii_case(&actual) => bail!(
            "the installer from {} is not the install.sh of {tag} (its SHA-256 is not the one \
             in sha256sums.txt); nothing was run",
            source.script
        ),
        Some(_) => Ok(tag),
    }
}

/// The release `sums` (a sha256sums.txt whose signature verified, from `source`) names on its
/// [`RELEASE_LINE`], which must be the one `source` asked for, if it named one.
fn sums_release(source: &UpdateSource, sums: &[u8]) -> anyhow::Result<String> {
    let url = format!("{}/sha256sums.txt", source.base);
    let text =
        std::str::from_utf8(sums).map_err(|_| anyhow!("{url} is not text; nothing was run"))?;
    let mut tags = text
        .lines()
        .filter_map(|l| l.strip_prefix(RELEASE_LINE))
        .map(str::trim_end);
    let tag = match (tags.next(), tags.next()) {
        (Some(tag), None) if is_release_tag(tag) => tag.to_string(),
        _ => bail!(
            "{url} does not name one release (a `{}<tag>` line); nothing was run",
            RELEASE_LINE.trim_end()
        ),
    };
    if let Some(asked) = source.tag.as_deref().filter(|asked| *asked != tag) {
        bail!("{url} is the checksums of {tag}, not of {asked}; nothing was run");
    }
    Ok(tag)
}

/// The SHA-256 `sums` (sha256sums.txt) lists for file `name`, as written there.
fn listed_sha256<'a>(sums: &'a [u8], name: &str) -> Option<&'a str> {
    std::str::from_utf8(sums).ok()?.lines().find_map(|l| {
        let (hash, file) = l.split_once("  ")?;
        (file.trim_end() == name).then_some(hash)
    })
}

/// The SHA-256 of `bytes`, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Largest release archive `twr box install` downloads ([`fetch_release_archive`]).
const RELEASE_ARCHIVE_MAX: usize = 256 << 20;

/// How long `twr box install` waits for a release archive.
const RELEASE_ARCHIVE_TIMEOUT: Duration = Duration::from_secs(600);

/// Downloads release `tag`'s archive for `target` (`twr-<target>.tar.gz`), checked the way
/// `twr update` checks a release: sha256sums.txt and its signature are downloaded from the
/// release (or the mirror of [`INSTALL_SCRIPT_URL_ENV`]), the signature must verify with the
/// release key, the checksums must name release `tag` and list the archive, and the archive
/// must have the SHA-256 they list. Returns the archive and its SHA-256 (lowercase hex).
async fn fetch_release_archive(tag: &str, target: &str) -> anyhow::Result<(Vec<u8>, String)> {
    let key = release_key(test_token_trusted(), std::env::var_os(TEST_RELEASE_KEY_ENV))?;
    let mirror = std::env::var(INSTALL_SCRIPT_URL_ENV)
        .ok()
        .filter(|u| !u.trim().is_empty());
    let source = update_source(Some(tag), mirror).await?;
    let sums_url = format!("{}/sha256sums.txt", source.base);
    let sig_url = format!("{sums_url}.sig");
    let sums = fetch_release_file(&sums_url).await?;
    let signature = fetch_signature(&sig_url).await?;
    check_signature(&key, &sums, &signature, &sums_url, &sig_url)?;
    sums_release(&source, &sums)?;
    let asset = format!("twr-{target}.tar.gz");
    let Some(want) = listed_sha256(&sums, &asset).map(str::to_ascii_lowercase) else {
        bail!("{sums_url} lists no {asset}: release {tag} has no build for {target}");
    };
    let base = source.archives.as_deref().unwrap_or(&source.base);
    let url = format!("{base}/{asset}");
    let archive = fetch_limited(&url, RELEASE_ARCHIVE_MAX, RELEASE_ARCHIVE_TIMEOUT).await?;
    let actual = sha256_hex(&archive);
    if actual != want {
        bail!(
            "{url} is not the {asset} of {tag} (its SHA-256 is not the one in sha256sums.txt); \
             nothing was installed"
        );
    }
    Ok((archive, actual))
}

/// A release file a server answered with a status other than 2xx.
#[derive(Debug)]
struct HttpStatus {
    url: String,
    status: reqwest::StatusCode,
}

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "downloading {} failed: HTTP {}", self.url, self.status)
    }
}

impl std::error::Error for HttpStatus {}

/// Whether `e`, from [`fetch_release_file`], says the file is not there: HTTP 404, or a
/// `file://` path that does not exist.
fn is_missing(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<HttpStatus>()
            .is_some_and(|h| h.status == reqwest::StatusCode::NOT_FOUND)
            || c.downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Checks that `signature` (base64 of a raw Ed25519 signature, from `sig_url`) signs `data`
/// (from `url`) with `key`.
fn check_signature(
    key: &[u8; 32],
    data: &[u8],
    signature: &[u8],
    url: &str,
    sig_url: &str,
) -> anyhow::Result<()> {
    let signature = std::str::from_utf8(signature)
        .ok()
        .and_then(|s| twr_core::b64::decode(s.trim()).ok())
        .filter(|s| s.len() == 64)
        .ok_or_else(|| anyhow!("{sig_url} is not an Ed25519 signature; nothing was run"))?;
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(data, &signature)
        .map_err(|_| {
            anyhow!(
                "{url} does not match its signature {sig_url} with twr's release key (it was \
                 changed, or is not from a twr release); nothing was run"
            )
        })
}

/// A private temporary directory (0700), removed with its files when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create(prefix: &str) -> anyhow::Result<PrivateDir> {
        use std::os::unix::fs::DirBuilderExt;
        let dir = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        for n in 0..100u32 {
            let path = dir.join(format!("{prefix}-{}-{nanos}-{n}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(PrivateDir(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("cannot create a temporary directory in {}", dir.display())
    }

    /// Writes `content` to a new file `name` in the directory (0600) and returns its path.
    fn write(&self, name: &str, content: &[u8]) -> anyhow::Result<PathBuf> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.0.join(name);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(content))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The directory `twr update` installs into: the one holding this executable `exe` (resolved,
/// so the real file is replaced, not a symlink to it). Refused when it is one of cargo's bin
/// directories `cargo_bins` (installing over a binary cargo manages confuses cargo), or when
/// the executable is not named `twr` (the installer writes `<dir>/twr`, which would not replace
/// it).
fn install_dir(exe: &Path, cargo_bins: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let dir = exe
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("{} is not in a directory", exe.display()))?;
    if exe.file_name() != Some(std::ffi::OsStr::new("twr")) {
        bail!(
            "this twr is {}, but the installer installs {}; rename it to twr first, or install \
             with `curl -fsSL https://twr.tools/install.sh | sh`",
            exe.display(),
            dir.join("twr").display()
        );
    }
    if cargo_bins.iter().any(|c| c == dir) {
        bail!(
            "{} is in cargo's bin directory, and `twr update` does not replace a binary cargo \
             manages. Update it with cargo (`cargo install --path crates/twr-cli` in a twr \
             checkout), or remove it (`cargo uninstall twr-cli`) and install the release with \
             `curl -fsSL https://twr.tools/install.sh | sh`",
            exe.display()
        );
    }
    Ok(dir.to_path_buf())
}

/// cargo's bin directories, `$CARGO_HOME/bin` and `~/.cargo/bin`, resolved where they exist;
/// `env` looks up variables (empty counts as unset).
fn cargo_bin_dirs(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let mut dirs = Vec::new();
    if let Some(home) = var("CARGO_HOME") {
        dirs.push(home.join("bin"));
    }
    if let Some(home) = var("HOME") {
        dirs.push(home.join(".cargo").join("bin"));
    }
    dirs.into_iter()
        .map(|d| d.canonicalize().unwrap_or(d))
        .collect()
}

/// Downloads one release file from `url`: https only (redirects included; a status other than
/// 2xx is an error), or `file:///<path>` read from disk ([`read_release_file`]). At most
/// [`RELEASE_FILE_MAX`] bytes, within [`RELEASE_FILE_TIMEOUT`].
async fn fetch_release_file(url: &str) -> anyhow::Result<Vec<u8>> {
    fetch_limited(url, RELEASE_FILE_MAX, RELEASE_FILE_TIMEOUT).await
}

/// [`fetch_release_file`] with another size limit `max` and time limit `timeout` (a release
/// archive, [`fetch_release_archive`]).
async fn fetch_limited(url: &str, max: usize, timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let too_large = || anyhow!("{url} is over {max} bytes; nothing was run");
    if let Some(rest) = url.strip_prefix("file://") {
        let path = rest.strip_prefix("localhost").unwrap_or(rest);
        if !path.starts_with('/') {
            bail!("{url} is not a local file:// URL (use file:///absolute/path)");
        }
        let owned = PathBuf::from(path);
        let bytes = read_detached(timeout, move || read_release_file(&owned, max))
            .await
            .map_err(|_| anyhow!("reading {path} timed out; nothing was run"))?
            .with_context(|| format!("reading {path}"))?;
        if bytes.len() > max {
            return Err(too_large());
        }
        return Ok(bytes);
    }
    if !url.starts_with("https://") {
        bail!("{url}: twr only downloads over https:// (or reads file:///)");
    }
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(timeout)
        .build()
        .context("setting up the HTTP client")?;
    let what = || format!("downloading {url}");
    let mut resp = client.get(url).send().await.with_context(what)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(HttpStatus {
            url: url.to_string(),
            status,
        }
        .into());
    }
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.with_context(what)? {
        if body.len() + chunk.len() > max {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Runs `read` on a thread of its own and waits for it at most `timeout` (`Err` past it). The
/// thread is never joined: a read stuck in the kernel (a stalled network or FUSE mount) holds
/// only that thread, and the update fails and exits after the timeout. On the runtime's
/// blocking pool, it would keep the runtime, and so the command, from ending.
async fn read_detached<F>(
    timeout: Duration,
    read: F,
) -> Result<std::io::Result<Vec<u8>>, tokio::time::error::Elapsed>
where
    F: FnOnce() -> std::io::Result<Vec<u8>> + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("twr-release-read".into())
        .spawn(move || {
            let _ = tx.send(read());
        });
    tokio::time::timeout(timeout, async move {
        if let Err(e) = spawned {
            return Err(e);
        }
        rx.await
            .unwrap_or_else(|_| Err(std::io::Error::other("the reading thread ended")))
    })
    .await
}

/// The release file at local `path`, read only when it is a regular file, as its opened
/// descriptor says (a symbolic link to one will do): opened without blocking, so a FIFO there
/// cannot hang the update, and read up to one byte past `max`, so a device such as
/// `/dev/zero` or a huge file cannot fill memory (the caller refuses what is longer).
fn read_release_file(path: &Path, max: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(twr_runner::client::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(max as u64 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Refuses what is not the installer: an empty body, or anything whose first line is not
/// `#!/bin/sh` (say, an error page served with status 200).
fn check_script(url: &str, script: &[u8]) -> anyhow::Result<()> {
    if script.is_empty() {
        bail!("the installer from {url} is empty; nothing was run");
    }
    let first = script.split(|b| *b == b'\n').next().unwrap_or_default();
    let first = first.strip_suffix(b"\r").unwrap_or(first);
    if first != b"#!/bin/sh" && !first.starts_with(b"#!/bin/sh ") {
        bail!("{url} did not return the installer (it does not start with #!/bin/sh); nothing was run");
    }
    Ok(())
}

/// The version `exe --version` prints (`twr <version>`), if it runs.
fn installed_version(exe: &Path) -> Option<String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    // No terminal: `twr --version` needs none.
    twr_adapters::process::new_session_std(&mut cmd);
    let out = cmd.output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    let v = text.trim().strip_prefix("twr ")?.trim();
    (out.status.success() && !v.is_empty()).then(|| v.to_string())
}

/// After an update: when a hub answers (on `--socket`, else the configured or standard socket)
/// and runs another version than the new `twr` at `new_exe`, says so and how to restart it.
async fn hub_version_note(new_exe: &Path, socket: Option<PathBuf>, paths: &Paths) {
    let Some(new) = installed_version(new_exe) else {
        return;
    };
    let bulk_hint = socket.is_none();
    let socket = socket
        .or_else(|| Config::load_from(paths).ok().map(|c| c.socket_path(paths)))
        .unwrap_or_else(|| paths.socket.clone());
    let info = tokio::time::timeout(PROBE_TIMEOUT, async {
        Client::connect(&socket)
            .await?
            .call("hub.info", Value::Null)
            .await
    })
    .await;
    let Ok(Ok(info)) = info else {
        return;
    };
    let Some(running) = info["version"].as_str() else {
        return;
    };
    if running != new {
        outln!();
        outln!(
            "The running hub (pid {}) is still twr {running}. To restart it on {new}, run \
             `twr{w} hub stop`, then `twr{w}`. Stopping the hub stops your threads' runtimes, so \
             pick a moment when no turn is running.",
            info["pid"],
            w = workspace_hint()
        );
        if bulk_hint {
            outln!("To stop every workspace's hub in this home, use `twr hub stop --all`, then reopen the workspaces you need.");
        }
    }
}

/// After `twr update` installed `new_exe`: its checkpoint of each workspace's log (`twr log
/// checkpoint --all`, run by it: checkpoints are per build), so that the next start of a hub
/// on it folds only what comes after it. Best effort: a release from before `log checkpoint`
/// refuses it, and the hub's start then folds the whole log, as before.
async fn prewarm_installed(new_exe: &Path) {
    outln!();
    let prewarm = tokio::process::Command::new(new_exe)
        .args(["log", "checkpoint", "--all"])
        .env_remove(twr_core::paths::WORKSPACE_ENV)
        .status()
        .await;
    if !prewarm.is_ok_and(|s| s.success()) {
        outln!(
            "(no checkpoint was written for the new build: a hub's first start on it folds the whole log)"
        );
    }
}

/// `twr runner`: serves the hub from this box until SIGINT / SIGTERM, with `opts` but their
/// token, which comes from `--token-file` (mode 0600), `$TWR_RUNNER_TOKEN` or the box's
/// default file `default_token` ([`dial_in::runner_token`]) and is never logged; the variable
/// is removed from the environment first thing, so nothing the runner starts inherits it. A
/// token shorter than [`twr_core::runner::TOKEN_GOOD_LEN`] is warned about (one too short to
/// use is refused). A SIGINT or SIGTERM is listened for from the start: one that comes while
/// it starts stops it before it connects ([`twr_runner::run`]).
async fn run_runner(
    mut opts: twr_runner::RunnerOptions,
    token_file: Option<PathBuf>,
    default_token: PathBuf,
) -> anyhow::Result<()> {
    let shutdown = twr_runner::Shutdown::install();
    let env_token = twr_runner::client::take_token_env();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let (token, file) =
        dial_in::runner_token(token_file, env_token, default_token, &opts.box_name)?;
    // Too short a token is refused (`RunnerOptions::validate`); a short one only warned about.
    if let Ok(Some(warning)) = twr_core::runner::check_token_len(token.expose(), "the runner token")
    {
        tracing::warn!("{warning}");
    }
    opts.token = token;
    opts.token_file = file;
    twr_runner::run(opts, shutdown).await
}

/// `twr runner --daemon`: an SSH box's daemon ([`twr_runner::daemon`]), started by its relay
/// with every path as an argument, its stdout and stderr `/dev/null`: it logs to `log`
/// (appended, 0600, in its private data directory), its failure included, and runs until a
/// `stop` request, SIGINT or SIGTERM. Those signals are listened for from the start: one that
/// comes while it starts (its relay stopped meanwhile, say) stops it before it binds its socket
/// ([`twr_runner::run`]).
async fn run_box_daemon(opts: twr_runner::RunnerOptions, log: &Path) -> anyhow::Result<()> {
    let shutdown = twr_runner::Shutdown::install();
    if let Some(dir) = log.parent() {
        twr_core::paths::ensure_private_dir(dir)?;
    }
    let file = twr_core::paths::open_private_append(log)
        .with_context(|| format!("opening the box daemon's log {}", log.display()))?;
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tracing::info!(
        version = twr_core::version(),
        build = twr_core::build_id().unwrap_or("-"),
        box_name = %opts.box_name,
        "the box daemon starts"
    );
    let r = twr_runner::run(opts, shutdown).await;
    if let Err(e) = &r {
        tracing::error!(error = %format!("{e:#}"), "the box daemon failed");
    }
    r
}

/// `twr runner --login <agent>` (`twr box login` runs it over ssh with a terminal): resolves
/// the agent as this box's runner would (its `TWR_*_BIN`, else on the box's baseline login
/// `PATH` merged before this one, none with `TWR_BOX_PROBE=off`:
/// [`twr_runner::agents::login_command`] with [`twr_runner::BoxEnv::user`], the daemon's) and
/// replaces this process with its login command, so the login talks to the user's terminal
/// directly. Refused without a terminal on stdin and stdout.
async fn runner_login(agent: &str) -> anyhow::Result<ExitCode> {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    let Some(runtime) = twr_adapters::agents::agent_runtime(agent) else {
        bail!("{agent:?} is not an agent twr logs in (claude, codex or grok)");
    };
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!(
            "`twr runner --login` needs a terminal on stdin and stdout: the login asks you \
             things (`twr box login <box> {agent}` runs it with one)"
        );
    }
    let mut cmd = twr_runner::agents::login_command(
        &twr_runner::BoxEnv::user(),
        runtime,
        |k| std::env::var_os(k),
        |why| {
            note(&format!(
                "twr: the login shell gave no PATH ({why}): looking for {agent} on this one"
            ))
        },
    )
    .await?;
    let shown = format!(
        "{} {}",
        cmd.get_program().to_string_lossy(),
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    );
    // Only returns when it could not run it.
    let e = cmd.exec();
    bail!("running `{shown}`: {e}")
}

/// `twr runner --clone-repo --url <url> --parent <dir> --name <name>` (`twr box project
/// --clone` runs it over ssh with a terminal): [`twr_runner::repos::clone_repo`], git talking
/// to the user's terminal, then a newline and `twr-clone-path <path>` as the last line
/// ([`twr_runner::repos::CLONE_PATH_MARK`]). Refused without a terminal on stdin and stdout,
/// after a URL twr does not clone (which needs no terminal to be told so).
async fn runner_clone(url: &str, parent: &Path, name: &str) -> anyhow::Result<ExitCode> {
    use std::io::IsTerminal;
    twr_core::gitremote::RemoteUrl::parse(url).map_err(|e| anyhow!("{e}"))?;
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        bail!(
            "`twr runner --clone-repo` needs a terminal on stdin and stdout: git may ask you \
             things (`twr box project <box> <project> --clone` runs it with one)"
        );
    }
    let path = twr_runner::repos::clone_repo(url, parent, name).await?;
    // On a line of its own, whatever git's output (a hook's) left unfinished.
    outln!(
        "\n{} {}",
        twr_runner::repos::CLONE_PATH_MARK,
        path.display()
    );
    Ok(ExitCode::SUCCESS)
}

/// `twr runner --exec <runtime> [--path <PATH>] [--env NAME=VALUE]... [--unset NAME]... --
/// <args>` (a remote attach to a thread on an SSH box runs it over ssh): resolves the runtime's
/// program as a spawn of the thread on this box resolves it
/// ([`twr_runner::agents::exec_command`] with [`twr_runner::BoxEnv::user`], the daemon's: its
/// `TWR_*_BIN`, else on the thread's `PATH` when given, else on the box's baseline login `PATH`
/// merged before this one, the shell asked with this process's environment, the box's as ssh
/// gives it, as the daemon asks it with its own, and not asked with `TWR_BOX_PROBE=off`) and
/// replaces this process with it, with `args` (the hub's) as they are and the thread's other
/// variables (`set`, `unset`) on it alone.
async fn runner_exec(
    runtime: &str,
    path: Option<&str>,
    set: &[String],
    unset: &[String],
    args: &[String],
) -> anyhow::Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    let mut env = Vec::new();
    for e in set {
        let Some((k, v)) = e.split_once('=') else {
            bail!("--env {e:?}: expected NAME=VALUE");
        };
        env.push((k.to_string(), Some(v.to_string())));
    }
    env.extend(unset.iter().map(|k| (k.clone(), None)));
    let mut cmd = twr_runner::agents::exec_command(
        &twr_runner::BoxEnv::user(),
        runtime,
        args,
        path,
        &env,
        |k| std::env::var_os(k),
        |why| {
            note(&format!(
                "twr: the login shell gave no PATH ({why}): looking for {runtime}'s program \
                 on this one"
            ))
        },
    )
    .await?;
    // Only returns when it could not run it.
    let e = cmd.exec();
    bail!("running {}: {e}", cmd.get_program().to_string_lossy())
}

/// `twr mcp`: the MCP server on stdio. Tracing goes to stderr only: stdout carries MCP.
async fn run_mcp(socket: PathBuf) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
    twr_mcp::serve_stdio(socket, vec![cli_help_page()]).await
}

/// The `cli` page `read_docs` serves: the `--help` of `twr` and of every command under it
/// that is not hidden, one `##` section each (`## twr thread new`), as this binary prints them.
fn cli_help_page() -> (String, String) {
    use clap::CommandFactory;
    fn walk(cmd: &clap::Command, path: &str, out: &mut String) {
        if cmd.is_hide_set() {
            return;
        }
        let help = cmd.clone().render_long_help().to_string();
        out.push_str(&format!(
            "\n## {path}\n\n```text\n{}\n```\n",
            help.trim_end()
        ));
        for sub in cmd.get_subcommands().filter(|s| s.get_name() != "help") {
            walk(sub, &format!("{path} {}", sub.get_name()), out);
        }
    }
    let mut cmd = Cli::command();
    cmd.build();
    let mut out = String::from(
        "# twr command line\n\nThe `--help` of every twr command, as this twr prints it.\n",
    );
    walk(&cmd, "twr", &mut out);
    ("cli".into(), out)
}

/// `twr say`: routes the message through the hub and says where it went.
async fn say(socket: &Path, message: &str) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let r = c.call("say", json!({ "message": message })).await?;
    // An answer to a pending item is never routed from here (nothing is on screen to say
    // which revision it answers): the message went nowhere, so this fails.
    if r["action"] == "pending" {
        bail!("{}", say_outcome(&r));
    }
    outln!("{}", say_outcome(&r));
    Ok(())
}

/// One line describing a `say` result.
fn say_outcome(r: &Value) -> String {
    let name = r["thread_name"]
        .as_str()
        .or(r["thread"].as_str())
        .unwrap_or("?");
    let turn = |r: &Value| match r["turn"].as_u64() {
        Some(t) => format!("turn {t} started"),
        None => "queued".to_string(),
    };
    match r["action"].as_str().unwrap_or_default() {
        "send" => format!("sent to {name} ({})", turn(r)),
        "respond" => format!(
            "answered {name}'s {}: {}",
            str_of(&r["attn_id"]),
            str_of(&r["answer"])
        ),
        "new_thread" if r["turn"].is_null() => format!("created {name}"),
        "new_thread" => format!("created {name} ({})", turn(r)),
        "delegate" => format!("handed to the orchestrator ({})", turn(r)),
        "command" => format!(
            "applied: {}",
            r["applied"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; "))
                .unwrap_or_default()
        ),
        "ask" => {
            let options: Vec<String> = r["options"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .enumerate()
                        .map(|(i, o)| format!("{} {}", i + 1, o.as_str().unwrap_or("?")))
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "which thread should get it? answer with `twr{} respond {} <n>`: {}",
                workspace_hint(),
                str_of(&r["attn_id"]),
                options.join(", ")
            )
        }
        "pending" => pending_outcome(r),
        other => format!("{other}: {r}"),
    }
}

/// What `twr say` prints for a message that tries to answer a pending item (`say` `action:
/// "pending"`): it was not sent; each open item and the `twr respond` that answers it.
fn pending_outcome(r: &Value) -> String {
    let items = r["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "not sent: it looks like an answer, but nothing waits for one any more; say it \
                again to route it"
            .to_string();
    }
    let mut lines = vec![
        "not sent: it looks like an answer, and `twr say` never answers a pending item".to_string(),
    ];
    for i in &items {
        let options = i["options"].as_array().cloned().unwrap_or_default();
        let (answer, choices) = if options.is_empty() {
            ("<answer>".to_string(), String::new())
        } else {
            let listed: Vec<&str> = options.iter().filter_map(Value::as_str).collect();
            (
                "<option>".to_string(),
                format!(" (options: {})", listed.join(", ")),
            )
        };
        lines.push(format!(
            "a {} is pending on {}: {} — answer it with `twr{w} respond {} {answer}`{choices}",
            str_of(&i["reason"]),
            str_of(&i["thread_name"]),
            truncate(&one_line(&item_summary(i)), SUMMARY_WIDTH),
            revisioned_id(i),
            w = workspace_hint()
        ));
    }
    lines.join("\n")
}

async fn connect(socket: &Path) -> anyhow::Result<Client> {
    let mut c = Client::connect(socket).await.map_err(|e| {
        anyhow!(
            "{e:#}\nno hub is running at {} (start one with `twr{w} hub`, or run `twr{w}`)",
            socket.display(),
            w = workspace_hint()
        )
    })?;
    let hello = c.hello(json!({ "client": "cli" })).await?;
    check_workspace(&hello, socket)?;
    Ok(c)
}

// --- attach ------------------------------------------------------------------------------------

/// `twr attach`: takes the attach lease (a dedicated connection that stays open while the
/// interactive CLI runs), runs the runtime's own CLI in this terminal, then ends the lease so
/// the thread's queued prompts run. Exits with the CLI's exit code.
async fn attach(socket: &Path, thread: &str, force: bool) -> anyhow::Result<ExitCode> {
    let mut lease = connect(socket).await?;
    // Said before `attach.begin` pauses the runtime. (A thread this cannot read is
    // `attach.begin`'s to refuse.)
    if let Ok(t) = lease.call("thread.get", json!({ "thread": thread })).await {
        if let Ok(info) = lease.call("hub.info", Value::Null).await {
            if let Some(w) = attach_mac_warning(&t, &info) {
                note(&w);
            }
        }
    }
    let grant = match lease
        .call("attach.begin", json!({ "thread": thread, "force": force }))
        .await
    {
        Ok(g) => g,
        Err(e) => {
            let running = e
                .downcast_ref::<RpcError>()
                .is_some_and(|r| r.code == codes::CONFLICT && r.message.contains("is running"));
            if running {
                return Err(e.context("use `twr attach --force` to interrupt it"));
            }
            return Err(e);
        }
    };
    let thread_id = grant["thread"].as_str().unwrap_or(thread).to_string();
    note(&format!(
        "twr: attached to {thread} ({} session {}); exit it to hand the thread back",
        str_of(&grant["runtime"]),
        str_of(&grant["session_id"]),
    ));
    if let Some(hint) = attach_hint(&grant) {
        note(hint);
    }
    let code = match run_attached(&grant, &mut lease).await {
        Ok(AttachRun::Exited(code)) => Ok(code),
        // The attach must not end while the user's CLI still runs on the session: leaving
        // without `attach.end` closes the lease connection, and the hub keeps the thread
        // attached until that process (reported with `attach.child`) has exited.
        Ok(AttachRun::StillRunning(pid)) => {
            note(&format!(
                "twr: the attached process {pid} did not exit; {thread} stays attached until it does"
            ));
            return Ok(ExitCode::from(1));
        }
        // It never ran: nothing to wait for.
        Err(e) => Err(e),
    };
    let ended = lease
        .call("attach.end", json!({ "thread": thread_id }))
        .await;
    if let Err(e) = &ended {
        // Closing the lease connection (on exit) detaches the thread anyway.
        note(&format!("twr: ending the attach failed: {e:#}"));
    }
    let code = code?;
    if ended.is_ok() {
        note(&format!("twr: detached from {thread}"));
    }
    Ok(code)
}

/// What `twr attach` says before it pauses the runtime of `thread` (its `thread.get`), with the
/// hub's boxes in `info` (`hub.info`): a Claude Code thread on a Mac box (`os` `Darwin`, from
/// the hub's probe of an SSH box; a token box's is not known) then runs over ssh, where Claude
/// Code may not read its keychain login. Nothing for a command box: the hub refuses attach
/// there (`attach.begin`), and nothing runs over ssh.
fn attach_mac_warning(thread: &Value, info: &Value) -> Option<String> {
    if thread["runtime"] != twr_adapters::claude_code::NAME {
        return None;
    }
    let name = thread["box_name"]
        .as_str()
        .filter(|b| *b != twr_hub::hub::LOCAL_BOX)?;
    let b = info["boxes"]
        .as_array()?
        .iter()
        .find(|b| b["name"].as_str() == Some(name))?;
    (b["os"] == "Darwin" && b["transport"] != "command").then(|| {
        format!(
            "twr: box {name} is a Mac: attached, Claude Code runs there over ssh, where it may \
             not read its keychain login. If it asks you to log in, end the attach (exit it) \
             and continue the thread from the TUI: the box daemon's Claude Code keeps its login"
        )
    })
}

/// What `twr attach` says before running the command, if anything: on the first attach to a
/// worktree twr made (`trust_prompt`), Claude Code asks whether to trust the folder, and its
/// default (No) exits.
fn attach_hint(grant: &Value) -> Option<&'static str> {
    (grant["trust_prompt"] == true).then_some(
        "twr: first attach to this worktree: Claude asks whether you trust the folder; choose \
         Yes (the default, No, exits and hands the thread back)",
    )
}

/// Writes a line to stderr, ignoring failures (after a hangup the terminal is gone, and
/// `eprintln!` would panic before the lease is ended).
fn note(line: &str) {
    let _ = writeln!(std::io::stderr(), "{}", term::clean(line));
}

/// How an attached command ended ([`run_attached`]).
enum AttachRun {
    /// It exited with this code.
    Exited(ExitCode),
    /// It was signalled to stop and still runs after [`ATTACH_TERM_GRACE`] and a SIGKILL
    /// (this pid).
    StillRunning(u32),
}

/// How long the attached command gets to exit after `twr attach` forwarded it SIGTERM or
/// SIGHUP, before it is killed.
const ATTACH_TERM_GRACE: Duration = Duration::from_secs(10);

/// How long to wait for the attached command to exit after SIGKILL.
const ATTACH_KILL_GRACE: Duration = Duration::from_secs(3);

/// Sends `SIG<sig>` to the attached command `pid` and what it started: its own process group
/// when it leads one, the group of this process when this process leads it (a shell's job:
/// the command inherited it), else the command alone (never a group this process does not
/// lead, which would reach whoever started `twr attach`). Signalling this process's own group
/// also signals this process; [`run_attached`] forwards each signal once.
fn forward_signal(pid: u32, sig: &str) {
    let me = std::process::id();
    let sent = match twr_runner::reap::process_group(pid) {
        Some(g) if g == i64::from(pid) => twr_runner::reap::signal(pid, true, sig),
        Some(g) if g == i64::from(me) => twr_runner::reap::signal(me, true, sig),
        _ => twr_runner::reap::signal(pid, false, sig),
    };
    if !sent {
        note(&format!("twr: could not forward SIG{sig} to process {pid}"));
    }
}

/// Runs the granted command with inherited stdio in its directory, and reports its process to
/// the hub on the `lease` connection (`attach.child`): if this process goes away while the
/// command still runs, the thread stays attached until the command exits. SIGINT and SIGQUIT
/// go to the child (it shares the terminal's foreground group); this process ignores them
/// until the child exits so that it can still end the lease. SIGTERM and SIGHUP are forwarded
/// to the child (see [`forward_signal`]); this process then waits for it (bounded, then kills
/// it) so that the attach ends only once the child is gone.
async fn run_attached(grant: &Value, lease: &mut Client) -> anyhow::Result<AttachRun> {
    use tokio::signal::unix::{signal, SignalKind};

    let argv: Vec<String> =
        serde_json::from_value(grant["argv"].clone()).context("decoding the attach command")?;
    let Some((program, args)) = argv.split_first() else {
        bail!("the hub returned an empty attach command");
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    if let Some(cwd) = grant["cwd"].as_str() {
        cmd.current_dir(cwd);
    }
    if let Some(env) = grant["env"].as_object() {
        for (k, v) in env {
            match v.as_str() {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            };
        }
    }
    // The interactive process is the user's, not the hub's: without the thread marker, stale
    // runtime reaping never mistakes it for a headless runtime. (It carries the attach marker,
    // `TWR_ATTACH`, from the grant instead.)
    cmd.env_remove(twr_hub::host::THREAD_ENV);
    cmd.env_remove(twr_hub::host::RUNTIME_GEN_ENV);
    // A test's token is for the test's own clients, never an agent's.
    cmd.env_remove(twr_core::client::TEST_USER_TOKEN_ENV);
    // Installed before the spawn so that a signal right away does not kill this process.
    let mut sigint = signal(SignalKind::interrupt()).context("handling SIGINT")?;
    let mut sigquit = signal(SignalKind::quit()).context("handling SIGQUIT")?;
    let mut sigterm = signal(SignalKind::terminate()).context("handling SIGTERM")?;
    let mut sighup = signal(SignalKind::hangup()).context("handling SIGHUP")?;
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "`{program}` is not installed or not on PATH; it is needed to attach to a {} thread",
            str_of(&grant["runtime"])
        ),
        Err(e) => return Err(anyhow!("starting `{program}`: {e}")),
    };
    let pid = child.id().unwrap_or_default();
    // Tell the hub which process holds the attach (its start time tells it from a later
    // process with the same pid). A child that is already gone has nothing to report.
    if let Some(start_time) = twr_runner::reap::process_start_time(pid) {
        let reported = tokio::time::timeout(
            Duration::from_secs(10),
            lease.call(
                "attach.child",
                json!({ "thread": grant["thread"], "pid": pid, "start_time": start_time }),
            ),
        )
        .await;
        let failed = match reported {
            Ok(Ok(_)) => None,
            Ok(Err(e)) => Some(format!("{e:#}")),
            Err(_) => Some("no answer".to_string()),
        };
        if let Some(e) = failed {
            note(&format!(
                "twr: the hub did not record the attached process ({e}); if this command is \
                 killed, the thread is handed back while it still runs"
            ));
        }
    }
    let mut forwarded: Vec<&str> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    let mut killed = false;
    let status = loop {
        let sig = tokio::select! {
            st = child.wait() => break st.with_context(|| format!("waiting for `{program}`"))?,
            _ = sigint.recv() => continue,
            _ = sigquit.recv() => continue,
            _ = sigterm.recv() => "TERM",
            _ = sighup.recv() => "HUP",
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)),
                if deadline.is_some() =>
            {
                if killed {
                    return Ok(AttachRun::StillRunning(pid));
                }
                note(&format!("twr: `{program}` did not exit; killing it"));
                if twr_runner::reap::process_group(pid) == Some(i64::from(pid)) {
                    twr_runner::reap::signal(pid, true, "KILL");
                }
                let _ = child.start_kill();
                killed = true;
                deadline = Some(tokio::time::Instant::now() + ATTACH_KILL_GRACE);
                continue;
            }
        };
        if !forwarded.contains(&sig) {
            forwarded.push(sig);
            forward_signal(pid, sig);
        }
        deadline.get_or_insert(tokio::time::Instant::now() + ATTACH_TERM_GRACE);
    };
    use std::os::unix::process::ExitStatusExt;
    let code = match (status.code(), status.signal()) {
        (Some(c), _) => c,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    };
    Ok(AttachRun::Exited(ExitCode::from(
        u8::try_from(code).unwrap_or(1),
    )))
}

// --- attention -----------------------------------------------------------------------------------

/// Longest summary `twr attention` prints, in characters.
const SUMMARY_WIDTH: usize = 80;

async fn info(socket: &Path, raw: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let i = c.call("hub.info", Value::Null).await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&i)?);
        return Ok(());
    }
    outln!("twr hub {} (pid {})", str_of(&i["version"]), i["pid"]);
    if let Some(name) = i["instance"].as_str() {
        outln!("instance:     {name}");
    }
    outln!("db:           {}", str_of(&i["db"]));
    outln!("threads:      {}", i["threads"]);
    outln!("attention:    {} open", i["open_attention"]);
    if let Some(load) = i["load"]["summary"].as_str() {
        outln!("load:         {load}");
    }
    if let Some(by) = i["worker_limits"].as_str() {
        outln!("limits:       workers under {by}");
    }
    let o = &i["orchestrator"];
    outln!(
        "orchestrator: runtime {}{}{}",
        orchestrator_runtime(o),
        models_text(&o["model"], &o["model_actual"]),
        if o["available"].as_bool() == Some(true) {
            ""
        } else {
            " (not available)"
        }
    );
    for line in accounts::info_lines(&i["accounts"]) {
        outln!("accounts:     {line}");
    }
    for b in i["boxes"].as_array().into_iter().flatten() {
        outln!("box:          {}", box_info_line(b));
    }
    for w in i["warnings"].as_array().into_iter().flatten() {
        outln!("warning:      {}", str_of(w));
    }
    Ok(())
}

async fn usage(socket: &Path, raw: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let u = c.call("usage.get", Value::Null).await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&u)?);
        return Ok(());
    }
    for line in usage_lines(&u, chrono::Utc::now(), &chrono::Local) {
        outln!("{line}");
    }
    Ok(())
}

async fn usage_threads(
    socket: &Path,
    raw: bool,
    since: Option<&str>,
    by: Option<&str>,
) -> anyhow::Result<()> {
    let since = since
        .map(|s| parse_since(s, chrono::Utc::now()))
        .transpose()?;
    let mut c = connect(socket).await?;
    let u = c
        .call(
            "usage.threads",
            json!({"since": since, "by": by.unwrap_or("thread")}),
        )
        .await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&u)?);
        return Ok(());
    }
    for line in spent_lines(&u) {
        outln!("{line}");
    }
    Ok(())
}

/// `--since`: a window back from `now` (`30m`, `24h`, `7d`, `2w`), a date (`2026-10-01`, its
/// start in UTC) or an RFC 3339 time.
fn parse_since(
    s: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<chrono::DateTime<chrono::Utc>> {
    let s = s.trim();
    let bad = || {
        anyhow::anyhow!("--since {s:?}: give a window such as 30m, 24h, 7d or 2w, a date such as 2026-10-01, or an RFC 3339 time")
    };
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&chrono::Utc));
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_time(chrono::NaiveTime::MIN).and_utc());
    }
    let split = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(bad)?;
    let (n, unit) = s.split_at(split);
    let n: i64 = n.parse().map_err(|_| bad())?;
    let delta = match unit {
        "m" => chrono::TimeDelta::try_minutes(n),
        "h" => chrono::TimeDelta::try_hours(n),
        "d" => chrono::TimeDelta::try_days(n),
        "w" => chrono::TimeDelta::try_weeks(n),
        _ => None,
    }
    .ok_or_else(bad)?;
    now.checked_sub_signed(delta).ok_or_else(bad)
}

/// `twr usage --threads`'s lines for a `usage.threads` answer `u`: a table of the rows, then
/// the total and what the cost means.
fn spent_lines(u: &Value) -> Vec<String> {
    let rows = u["rows"].as_array().cloned().unwrap_or_default();
    let window = match u["since"].as_str() {
        Some(t) => format!(" since {t}"),
        None => String::new(),
    };
    if rows.is_empty() {
        return vec![format!("no turn reported usage{window}")];
    }
    let spent = |r: &Value| serde_json::from_value::<twr_core::usage::Spent>(r.clone()).ok();
    let cells = |r: &Value| -> [String; 5] {
        let s = spent(r).unwrap_or_default();
        [
            str_of(&r["label"]).to_string(),
            s.turns.to_string(),
            twr_core::usage::tokens_text(s.input),
            twr_core::usage::tokens_text(s.output),
            s.cost()
                .map_or_else(|| "-".into(), twr_core::usage::cost_text),
        ]
    };
    let by = str_of(&u["by"]).to_uppercase();
    let header = [by.as_str(), "TURNS", "IN", "OUT", "COST"];
    let mut table_rows: Vec<[String; 5]> = rows.iter().map(cells).collect();
    table_rows.push(cells(&u["total"]));
    let text = table(&header, &table_rows);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines.push(format!(
        "{} turn(s){window}. IN counts cache reads and writes; COST is what the runtime \
         reported (Claude Code: the API price of the tokens, on any plan; Codex and Grok Build \
         report none: -).",
        u["total"]["turns"]
    ));
    lines
}

/// `twr usage`'s lines for a `usage.get` answer `u` at `now`, reset times in `tz`: per runtime
/// its label, plan and the data's age, then each window's remaining share and reset.
fn usage_lines<Tz: chrono::TimeZone>(
    u: &Value,
    now: chrono::DateTime<chrono::Utc>,
    tz: &Tz,
) -> Vec<String>
where
    Tz::Offset: std::fmt::Display,
{
    use twr_core::usage::{age, age_secs, remaining_pct, reset_time};
    let runtimes = u["runtimes"].as_array().cloned().unwrap_or_default();
    if runtimes.is_empty() {
        return vec![
            "no usage known yet: it comes from Claude Code, Codex and Grok Build sessions as \
             they run"
                .into(),
        ];
    }
    let mut out = Vec::new();
    for r in &runtimes {
        let mut head = str_of(&r["label"]).to_string();
        if let Some(plan) = r["plan"].as_str() {
            head.push_str(&format!(" ({plan})"));
        }
        let secs = r["age_secs"].as_i64().unwrap_or(0);
        head.push_str(&format!(", as of {} ago", age_secs(secs)));
        out.push(head);
        for w in r["windows"].as_array().into_iter().flatten() {
            let pct = remaining_pct(w["used_pct"].as_f64().unwrap_or(0.0)).round();
            let mut line = format!("  {:<3} {:>3}% left", str_of(&w["label"]), pct);
            let resets = w["resets_at"]
                .as_str()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc));
            if let Some(at) = resets {
                line.push_str(&format!(
                    ", resets {} (in {})",
                    reset_time(at, now, tz),
                    age(at - now)
                ));
            }
            out.push(line);
        }
    }
    out
}

/// Whether attention item `item` blocks its thread (a permission or a question): what `twr
/// attention` lists without `--all`, and what `twr respond <thread>` answers.
fn is_blocking(item: &Value) -> bool {
    matches!(item["reason"].as_str(), Some("permission" | "question"))
}

async fn attention(socket: &Path, all: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    // Every item: an empty default listing still says what `--all` would show.
    let items = c.call("attention.list", json!({ "all": true })).await?;
    let every = items.as_array().cloned().unwrap_or_default();
    let items: Vec<Value> = every
        .iter()
        .filter(|i| all || is_blocking(i))
        .cloned()
        .collect();
    if items.is_empty() {
        match every.len() {
            0 => outln!("nothing needs you"),
            1 => outln!(
                "nothing blocks you; 1 other item (twr{} attention --all)",
                workspace_hint()
            ),
            n => outln!(
                "nothing blocks you; {n} other items (twr{} attention --all)",
                workspace_hint()
            ),
        }
        return Ok(());
    }
    // The id carries the revision listed: `twr respond` with it answers only that revision.
    let header = ["ID", "THREAD", "REASON", "SUMMARY", "OPTIONS"];
    let rows: Vec<[String; 5]> = items
        .iter()
        .map(|i| {
            [
                revisioned_id(i),
                str_of(&i["thread_name"]).to_string(),
                str_of(&i["reason"]).to_string(),
                truncate(&one_line(&item_summary(i)), SUMMARY_WIDTH),
                numbered_options(i),
            ]
        })
        .collect();
    out!("{}", table(&header, &rows));
    Ok(())
}

/// `twr attention dismiss <attn_id>[@<revision>]`.
async fn dismiss(socket: &Path, target: &str) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let items = c.call("attention.list", json!({ "all": true })).await?;
    let items = items.as_array().cloned().unwrap_or_default();
    let Some((item, revision)) = find_item(&items, target) else {
        bail!("no open attention item {target} (`twr attention --all` lists them)");
    };
    let mut p = json!({ "attn_id": item["attn_id"] });
    if let Some(r) = revision {
        p["revision"] = json!(r);
    }
    c.call("attention.dismiss", p).await?;
    outln!(
        "dismissed {} on {}: {}",
        str_of(&item["reason"]),
        str_of(&item["thread_name"]),
        truncate(&one_line(&item_summary(item)), SUMMARY_WIDTH)
    );
    Ok(())
}

/// An item's options as `twr attention` lists them: `1:allow 2:deny`.
fn numbered_options(item: &Value) -> String {
    item["options"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(n, o)| format!("{}:{}", n + 1, o.as_str().unwrap_or("?")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An item's id as `twr attention` prints it and `twr respond` takes it: `<attn_id>@<revision>`
/// (the bare id when the hub gave no revision).
fn revisioned_id(item: &Value) -> String {
    let id = str_of(&item["attn_id"]);
    match item["revision"].as_u64().filter(|r| *r > 0) {
        Some(r) => format!("{id}@{r}"),
        None => id.to_string(),
    }
}

/// The open item `target` names among `items`: its attn_id, or `<attn_id>@<revision>` (then
/// with that revision). An attn_id that itself ends in `@<digits>` is taken as written.
fn find_item<'a>(items: &'a [Value], target: &str) -> Option<(&'a Value, Option<u64>)> {
    if let Some(i) = items.iter().find(|i| i["attn_id"] == target) {
        return Some((i, None));
    }
    let (id, rev) = target.rsplit_once('@')?;
    let rev = rev.parse::<u64>().ok()?;
    items
        .iter()
        .find(|i| i["attn_id"] == id)
        .map(|i| (i, Some(rev)))
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// How `twr respond` sends `answer` for an item with `options`.
#[derive(Debug, PartialEq)]
enum AnswerParam {
    /// An option label (matched case-insensitively) or free text.
    Label(String),
    /// A 0-based option index (from the 1-based number the user typed).
    Index(usize),
}

fn answer_param(answer: &str, options: &[String]) -> anyhow::Result<AnswerParam> {
    let answer = answer.trim();
    if answer.is_empty() {
        bail!("the answer is empty");
    }
    if let Some(label) = options.iter().find(|o| o.as_str() == answer) {
        return Ok(AnswerParam::Label(label.clone()));
    }
    let folded: Vec<&String> = options
        .iter()
        .filter(|o| o.eq_ignore_ascii_case(answer))
        .collect();
    if let [label] = folded.as_slice() {
        return Ok(AnswerParam::Label((*label).clone()));
    }
    if let (Ok(n), false) = (answer.parse::<usize>(), options.is_empty()) {
        if n == 0 || n > options.len() {
            bail!(
                "option {n} does not exist; pick 1-{} ({})",
                options.len(),
                options.join(", ")
            );
        }
        return Ok(AnswerParam::Index(n - 1));
    }
    Ok(AnswerParam::Label(answer.to_string()))
}

/// How `twr respond <thread>` gets the user's consent to answer the top item it printed.
enum Consent<'a> {
    /// `--yes`: the user consented up front to whatever item is printed.
    Yes,
    /// A terminal: show the prompt, read the user's reply line.
    Ask(&'a mut dyn FnMut(&str) -> anyhow::Result<String>),
    /// No terminal and no `--yes`: nobody can confirm, so refuse.
    NoTerminal,
}

/// Prints question `q` on stderr and reads the reply line from stdin (empty at EOF).
fn ask_on_stderr(q: &str) -> anyhow::Result<String> {
    err!("{q}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// The consent a command that confirms gets: `--yes`, else asking on a terminal (`ask`), else
/// none.
fn consent(yes: bool, ask: &mut dyn FnMut(&str) -> anyhow::Result<String>) -> Consent<'_> {
    if yes {
        Consent::Yes
    } else if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        Consent::Ask(ask)
    } else {
        Consent::NoTerminal
    }
}

/// Whether `twr respond <thread>` may send `answer` to `item`, the top item it just printed:
/// the user answered a question they may not have seen, so they must confirm it (or have
/// passed `--yes`). The refusal names the exact `<attn_id>@<revision>` to answer instead.
fn consent_to_top_item(item: &Value, answer: &str, consent: Consent<'_>) -> anyhow::Result<()> {
    let id = revisioned_id(item);
    match consent {
        Consent::Yes => Ok(()),
        Consent::Ask(ask) => {
            let reply = ask(&format!("answer {id} with {}? [y/N] ", sh_word(answer)))?;
            if matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Ok(())
            } else {
                bail!("not answered")
            }
        }
        Consent::NoTerminal => bail!(
            "not answering {}'s top item unseen (no terminal to confirm it): if the question \
             above is the one you mean, run `twr{w} respond {id} {}` (or pass --yes)",
            str_of(&item["thread_name"]),
            sh_word(answer),
            w = workspace_hint()
        ),
    }
}

/// `twr respond`: answers an attention item, or a thread's top item (after `consent`).
async fn respond(
    socket: &Path,
    target: &str,
    answer: &str,
    message: Option<String>,
    consent: Consent<'_>,
) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let items = c.call("attention.list", json!({ "all": true })).await?;
    let items = items.as_array().cloned().unwrap_or_default();
    // The revision answered: the one the user named (`<attn_id>@<rev>`), or, for a thread's
    // top item, the one shown here and confirmed. A bare attn_id sends none: the hub then
    // answers only an item never raised again (the user may have read an older revision).
    let (item, revision, top) = match find_item(&items, target) {
        Some((item, rev)) => (item.clone(), rev, false),
        None => match c.call("thread.get", json!({ "thread": target })).await {
            // Items come in stack order: the thread's first permission or question is its top
            // item. Only those are answered this way (an error or review item is dismissed,
            // `twr attention dismiss`), as `twr attention` lists by default.
            Ok(t) => {
                let ofs = |i: &&Value| i["thread"] == t["id"];
                let item = items
                    .iter()
                    .filter(ofs)
                    .find(|i| is_blocking(i))
                    .cloned()
                    .ok_or_else(|| {
                        let name = str_of(&t["name"]);
                        match items.iter().filter(ofs).count() {
                            0 => anyhow!("{name} has no open attention item"),
                            n => anyhow!(
                                "{name} has no open permission or question to answer ({n} \
                                 other item(s): twr{w} attention --all; dismiss one with \
                                 `twr{w} attention dismiss <id>`)",
                                w = workspace_hint()
                            ),
                        }
                    })?;
                outln!(
                    "{} on {} ({}): {}",
                    revisioned_id(&item),
                    str_of(&item["thread_name"]),
                    str_of(&item["reason"]),
                    item_summary(&item)
                );
                // All of a request whose summary shows only its start: what an allow runs.
                if let Some(input) = item["context"]["input"].as_str() {
                    outln!("  input: {input}");
                }
                let options = numbered_options(&item);
                if !options.is_empty() {
                    outln!("  options: {options}");
                }
                let rev = item["revision"].as_u64().filter(|r| *r > 0);
                (item, rev, true)
            }
            Err(e) if is_not_found(&e) => {
                bail!("{target:?} is neither a thread nor an open attention item")
            }
            Err(e) => return Err(e),
        },
    };
    let options: Vec<String> = serde_json::from_value(item["options"].clone()).unwrap_or_default();
    let param = answer_param(answer, &options)?;
    if top {
        std::io::stdout().flush()?;
        consent_to_top_item(&item, answer, consent)?;
    }
    let attn_id = str_of(&item["attn_id"]).to_string();
    let mut params = json!({ "attn_id": attn_id });
    if let Some(r) = revision {
        params["revision"] = json!(r);
    }
    match param {
        AnswerParam::Label(l) => params["answer"] = json!(l),
        AnswerParam::Index(i) => params["option_index"] = json!(i),
    }
    if let Some(m) = message {
        params["message"] = json!(m);
    }
    let ev = c.call("attention.respond", params).await?;
    outln!(
        "answered {} on {}: {}",
        attn_id,
        str_of(&item["thread_name"]),
        str_of(&ev["data"]["answer"]["decision"])
    );
    // A delete card a thread of which was deleted and restored since: nothing went.
    for why in ev["data"]["ext"]["twr"]["changed"]
        .as_array()
        .into_iter()
        .flatten()
    {
        outln!("nothing deleted: {}", str_of(why));
    }
    Ok(())
}

// --- delete ------------------------------------------------------------------------------------

/// How often `twr delete` looks whether the deletion it confirmed finished.
const DELETE_POLL: Duration = Duration::from_millis(200);

/// How long `twr delete` waits for the deletion to finish.
const DELETE_WAIT: Duration = Duration::from_secs(120);

/// How the summary of the error item the hub raises on a thread whose deletion failed starts
/// (`Deleting <name> failed: ...`).
const DELETE_FAILED: &str = "Deleting ";

/// Whether `e` is the hub's `not_found`.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<RpcError>()
        .is_some_and(|r| r.code == codes::NOT_FOUND)
}

/// What `twr delete` does with the delete card it printed.
#[derive(Debug, PartialEq)]
enum DeleteReply {
    /// Answer `Delete`.
    Delete,
    /// Answer `Cancel`.
    Cancel,
    /// Leave the card open for the user to answer later.
    Open,
}

/// The reply to the delete card of `names` (the threads, as [`names_text`] says them):
/// `--yes` deletes; on a terminal `y`/`yes` deletes, `n`/`no` cancels, and anything else (an
/// empty line, EOF) leaves the card open, as does no terminal.
fn delete_reply(names: &str, consent: Consent<'_>) -> anyhow::Result<DeleteReply> {
    Ok(match consent {
        Consent::Yes => DeleteReply::Delete,
        Consent::Ask(ask) => {
            let reply = ask(&format!("Delete {names}? [y/N] "))?;
            match reply.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => DeleteReply::Delete,
                "n" | "no" => DeleteReply::Cancel,
                _ => DeleteReply::Open,
            }
        }
        Consent::NoTerminal => DeleteReply::Open,
    })
}

/// `a`, `a and b`, `a, b and c`.
fn names_text(names: &[String]) -> String {
    match names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The threads of a `thread.delete` result, `(id, name)` in the card's order (a hub that
/// names one thread: that one; `asked` when it says nothing).
fn card_threads(card: &Value, asked: &str) -> Vec<(String, String)> {
    let pair = |t: &Value, id: &str, name: &str| {
        let id = t[id].as_str().unwrap_or(asked).to_string();
        let name = t[name].as_str().unwrap_or(&id).to_string();
        (id, name)
    };
    match card["threads"].as_array() {
        Some(ts) if !ts.is_empty() => ts
            .iter()
            .map(|t| pair(t, "thread", "thread_name"))
            .collect(),
        _ => vec![pair(card, "thread", "thread_name")],
    }
}

/// What `twr delete` prints when it leaves delete card `id` (`<attn_id>@<revision>`) open.
fn delete_open_note(id: &str) -> String {
    format!(
        "not deleted: the card stays open until you answer it (`twr{w} respond {id} Delete`, \
         or Cancel to keep the thread), or on the TUI",
        w = workspace_hint()
    )
}

/// What `twr delete` prints once thread `name` is gone, from its `thread.deleted` payload
/// (`None`: not found, then only that it is gone).
fn deleted_outcome(name: &str, d: Option<&Value>) -> String {
    let mut out = format!("deleted {name}");
    let Some(d) = d else {
        return out;
    };
    let n = |k: &str| d[k].as_u64().unwrap_or(0);
    if let Some(w) = d["worktree"].as_str() {
        out.push_str(&format!("; moved worktree {w} to the trash"));
        if n("dirty_files") > 0 {
            out.push_str(&format!(" with {} uncommitted file(s)", n("dirty_files")));
        }
    }
    if n("head_only_commits") > 0 {
        out.push_str(&format!(
            "; kept {} commit(s) made on a detached HEAD in it in the trash",
            n("head_only_commits")
        ));
    }
    if let Some(w) = d["worktree_left"].as_str() {
        out.push_str(&format!(
            "; left part of {w}: it could not all be moved (no worktree any more), remove it \
             by hand"
        ));
    }
    let aside: Vec<&str> = d["build_output_aside"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| p.as_str())
        .collect();
    if !aside.is_empty() {
        out.push_str(&format!(
            "; could not delete its build output {} nor put it back: it is kept in the trash \
             item, and a restore puts it back",
            aside.join(", ")
        ));
    }
    if let Some(b) = d["branch"].as_str() {
        out.push_str(&format!("; moved branch {b} to the trash"));
        if n("branch_only_commits") > 0 {
            out.push_str(&format!(
                " with {} commit(s) on no other branch, tag or remote",
                n("branch_only_commits")
            ));
        }
    }
    if n("dropped_prompts") > 0 {
        out.push_str(&format!(
            "; dropped {} queued prompt(s)",
            n("dropped_prompts")
        ));
    }
    if let Some(item) = d["trash"].as_str() {
        let short = &item[item.len().saturating_sub(8)..];
        out.push_str(&format!("; trash item {short}"));
        if let Some(b) = d["trash_bytes"].as_u64() {
            out.push_str(&format!(" ({})", human_bytes(b)));
        }
        out.push_str(&format!(
            " (`twr trash restore {short}` brings it back, `twr trash empty {short}` purges it)"
        ));
    }
    out
}

/// The open items of `items` (an `attention.list`) saying that deleting thread `thread`
/// failed, by attn_id.
fn delete_failures<'a>(items: &'a Value, thread: &str) -> Vec<(&'a str, &'a str)> {
    items
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| i["thread"] == thread && i["reason"] == "error")
        .filter_map(|i| {
            let summary = i["summary"].as_str()?;
            summary
                .starts_with(DELETE_FAILED)
                .then_some((i["attn_id"].as_str()?, summary))
        })
        .collect()
}

/// The payload of thread `thread`'s `thread.deleted` on the control stream after gseq `since`.
async fn deleted_event(c: &mut Client, since: u64, thread: &str) -> anyhow::Result<Option<Value>> {
    let mut since = since;
    loop {
        let r = c
            .call("control.read", json!({ "since": since, "limit": PAGE }))
            .await?;
        let items = r["items"].as_array().cloned().unwrap_or_default();
        if let Some(i) = items
            .iter()
            .find(|i| i["event"]["kind"] == "thread.deleted" && i["event"]["thread"] == thread)
        {
            return Ok(Some(i["event"]["data"].clone()));
        }
        if items.is_empty() || r["more"] != true {
            return Ok(None);
        }
        since = r["next"].as_u64().unwrap_or(since);
    }
}

/// The delete card that replaced card `attn_id` (the hub found more to lose than it said once
/// it was confirmed), from the control stream after gseq `*since` (advanced past what was
/// read): `(<attn_id>@<revision>, summary)`.
async fn replacing_card(
    c: &mut Client,
    since: &mut u64,
    attn_id: &str,
) -> anyhow::Result<Option<(String, String)>> {
    let replaces = format!("/ext/twr/{DELETE_KEY}/replaces");
    loop {
        let r = c
            .call("control.read", json!({ "since": *since, "limit": PAGE }))
            .await?;
        let items = r["items"].as_array().cloned().unwrap_or_default();
        for i in &items {
            let ev = &i["event"];
            if ev["kind"] != "attention.raised" || ev["thread"] != HUB_THREAD {
                continue;
            }
            // Its fields may be in the blob store.
            let mut data = ev["data"].clone();
            hydrate_deep(&mut data, c).await?;
            if data.pointer(&replaces).and_then(Value::as_str) == Some(attn_id) {
                let id = format!("{}@{}", str_of(&data["attn_id"]), i["gseq"]);
                return Ok(Some((id, str_of(&data["summary"]).to_string())));
            }
        }
        *since = r["next"].as_u64().unwrap_or(*since);
        if items.is_empty() || r["more"] != true {
            return Ok(None);
        }
    }
}

/// `twr delete`: raises (or finds) the delete card of `threads`, prints what it deletes,
/// answers it as `consent` allows ([`delete_reply`]) and, once it answered Delete, waits until
/// each thread is gone or the hub says deleting it failed; prints a `deleted ...` line per
/// thread gone, and fails with what failed, if anything did. When the hub raises a new card
/// instead (more to lose than the card said by then), it prints that card and fails: the
/// user answers it, `--yes` never does.
async fn delete(socket: &Path, threads: &[String], consent: Consent<'_>) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let params = match threads {
        [one] => json!({ "thread": one }),
        _ => json!({ "threads": threads }),
    };
    let card = c.call("thread.delete", params).await?;
    // The ids, not the names: once deleted, a name may be another thread's.
    let targets = card_threads(&card, threads.first().map_or("", String::as_str));
    let names: Vec<String> = targets.iter().map(|(_, n)| n.clone()).collect();
    let names = names_text(&names);
    let card_id = revisioned_id(&card);
    if card["existing"].as_bool() == Some(true) {
        outln!("a delete card for {names} was already open ({card_id})");
    }
    outln!("{}", localize(str_of(&card["summary"])));
    std::io::stdout().flush()?;
    let reply = delete_reply(&names, consent)?;
    if reply == DeleteReply::Open {
        outln!("{}", delete_open_note(&card_id));
        return Ok(());
    }
    // Where to look for the outcome: the control stream from here, and failure items other
    // than the ones already open (from an earlier attempt).
    let head = c.call("hub.info", Value::Null).await?["control_head"]
        .as_u64()
        .unwrap_or(0);
    let items = c.call("attention.list", json!({ "all": true })).await?;
    let earlier: Vec<String> = targets
        .iter()
        .flat_map(|(id, _)| delete_failures(&items, id))
        .map(|(a, _)| a.to_string())
        .collect();
    let answer = if reply == DeleteReply::Delete {
        "Delete"
    } else {
        "Cancel"
    };
    let mut params = json!({ "attn_id": card["attn_id"], "answer": answer });
    if let Some(r) = card["revision"].as_u64().filter(|r| *r > 0) {
        params["revision"] = json!(r);
    }
    c.call("attention.respond", params).await?;
    if reply == DeleteReply::Cancel {
        outln!("kept {names} (answered Cancel)");
        return Ok(());
    }
    // Each thread ends gone (`Some(None)`) or with a new failure item (`Some(Some(summary))`).
    // The hub deletes them one after another: the deadline counts from the latest progress.
    let mut outcome: Vec<Option<Option<String>>> = vec![None; targets.len()];
    let mut deadline = std::time::Instant::now() + DELETE_WAIT;
    let mut scanned = head;
    while outcome.iter().any(Option::is_none) {
        let items = c.call("attention.list", json!({ "all": true })).await?;
        for (i, (id, _)) in targets.iter().enumerate() {
            if outcome[i].is_some() {
                continue;
            }
            let failed = delete_failures(&items, id)
                .into_iter()
                .find(|(a, _)| !earlier.iter().any(|e| e == a))
                .map(|(_, summary)| summary.to_string());
            let settled = match c.call("thread.get", json!({ "thread": id })).await {
                Err(e) if is_not_found(&e) => Some(None),
                Err(e) => return Err(e),
                Ok(_) => failed.map(Some),
            };
            if settled.is_some() {
                outcome[i] = settled;
                deadline = std::time::Instant::now() + DELETE_WAIT;
            }
        }
        if outcome.iter().all(Option::is_some) {
            break;
        }
        let attn_id = str_of(&card["attn_id"]);
        if let Some((id, summary)) = replacing_card(&mut c, &mut scanned, attn_id).await? {
            outln!("{}", localize(&summary));
            bail!(
                "not deleted: by the time the card was confirmed there was more to lose than it \
                 said; a new card ({id}) waits for you: `twr{w} respond {id} Delete`, or \
                 Cancel to keep them",
                w = workspace_hint()
            );
        }
        if std::time::Instant::now() >= deadline {
            let waiting: Vec<String> = targets
                .iter()
                .zip(&outcome)
                .filter(|(_, o)| o.is_none())
                .map(|((_, n), _)| n.clone())
                .collect();
            bail!(
                "{} {} not deleted after {}s; `twr{w} thread list --all` shows whether {} \
                 still there, `twr{w} attention --all` whether deleting {} failed",
                names_text(&waiting),
                if waiting.len() == 1 { "is" } else { "are" },
                DELETE_WAIT.as_secs(),
                if waiting.len() == 1 {
                    "it is"
                } else {
                    "they are"
                },
                if waiting.len() == 1 { "it" } else { "them" },
                w = workspace_hint()
            );
        }
        tokio::time::sleep(DELETE_POLL).await;
    }
    let mut failures = Vec::new();
    for ((id, name), o) in targets.iter().zip(outcome) {
        match o.flatten() {
            Some(summary) => failures.push(summary),
            None => {
                // Best effort: the thread is gone either way.
                let deleted = deleted_event(&mut c, head, id).await.unwrap_or(None);
                outln!("{}", localize(&deleted_outcome(name, deleted.as_ref())));
            }
        }
    }
    if !failures.is_empty() {
        bail!("{}", failures.join("\n"));
    }
    Ok(())
}

// --- trash -------------------------------------------------------------------------------------

/// `twr trash list`: the trash of every box, as a table (or the raw JSON with `raw`).
/// `twr log compact` and `twr log stats`.
async fn log_cmd(socket: &Path, cmd: LogCmd) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    match cmd {
        LogCmd::Compact {
            vacuum,
            drop_checkpoints,
            json,
        } => {
            if vacuum && !json {
                errln!(
                    "compacting the log, then VACUUM: the hub does nothing else until it is \
                     done (3 to 18 s for a 475 MB twr.db)…"
                );
            }
            let r = c
                .call(
                    "log.compact",
                    json!({ "vacuum": vacuum, "drop_checkpoints": drop_checkpoints }),
                )
                .await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                out!("{}", log_compact_text(&r));
            }
        }
        LogCmd::Checkpoint { .. } => unreachable!("`twr log checkpoint` needs no hub"),
        LogCmd::Stats { json } => {
            let r = c.call("log.stats", Value::Null).await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                out!("{}", log_stats_text(&r));
            }
        }
    }
    Ok(())
}

/// `twr log checkpoint`: a checkpoint for this build in this workspace's log, or in every
/// workspace's with `all` ([`twr_hub::prewarm_checkpoint`]). A workspace without a log yet is
/// skipped; one that fails is reported, and the command fails once all were tried.
async fn log_checkpoint(paths: &Paths, all: bool, json: bool) -> anyhow::Result<()> {
    let root = paths.root();
    let names = if all {
        let mut names = vec![DEFAULT_WORKSPACE.to_string()];
        names.extend(
            root.named_workspaces()
                .with_context(|| format!("listing {}", root.workspaces_dir().display()))?,
        );
        names
    } else {
        vec![paths.workspace_name().to_string()]
    };
    let mut failed = 0;
    for name in names {
        let ws = root.workspace(&name)?;
        let db = ws.db_file();
        let r = if !db.exists() {
            Ok(json!({ "workspace": name, "skipped": "no log yet" }))
        } else {
            // The `[notify]` the hub would fold with: part of what a checkpoint is for.
            let notify = Config::load_from(&ws).map(|c| c.notify).unwrap_or_default();
            tokio::task::spawn_blocking(move || twr_hub::prewarm_checkpoint(&db, &notify))
                .await?
                .map(|p| prewarm_json(&name, &p))
                .map_err(|e| (name, e))
        };
        match r {
            Ok(v) if json => outln!("{v}"),
            Ok(v) => outln!("{}", prewarm_text(&v)),
            Err((name, e)) => {
                failed += 1;
                errln!("{name}: {e:#}");
            }
        }
    }
    if failed > 0 {
        bail!("{failed} workspace(s) got no checkpoint");
    }
    Ok(())
}

/// What [`log_checkpoint`] did in workspace `name`, as JSON.
fn prewarm_json(name: &str, p: &twr_hub::Prewarm) -> Value {
    match p {
        twr_hub::Prewarm::Written {
            id,
            from,
            gseq,
            events,
            bytes,
            ms,
        } => json!({ "workspace": name, "written": id, "from": from, "gseq": gseq,
                     "events": events, "bytes": bytes, "ms": ms }),
        twr_hub::Prewarm::UpToDate => json!({ "workspace": name, "up_to_date": true }),
        twr_hub::Prewarm::SchemaDiffers { found, this } if found > this => {
            json!({ "workspace": name,
            "skipped": format!("its schema is {found}, newer than this build's {this}: this build's hub cannot open it") })
        }
        twr_hub::Prewarm::SchemaDiffers { found, this } => json!({ "workspace": name,
            "skipped": format!("its schema is {found}, this build's {this}: the hub's next start migrates it") }),
    }
}

/// [`prewarm_json`] as a line.
fn prewarm_text(v: &Value) -> String {
    let name = str_of(&v["workspace"]);
    if let Some(why) = v["skipped"].as_str() {
        return format!("{name}: skipped ({why})");
    }
    if v["up_to_date"] == true {
        return format!("{name}: up to date (this build has a checkpoint at the log's end)");
    }
    format!(
        "{name}: checkpoint {} written at control position {} ({} events folded, {}, {} ms)",
        v["written"],
        v["gseq"],
        v["events"],
        twr_core::git::human_bytes(v["bytes"].as_u64().unwrap_or(0)),
        v["ms"]
    )
}

/// Bytes as `twr trash list` shows them.
fn bytes_text(v: &Value) -> String {
    twr_core::git::human_bytes(v.as_u64().unwrap_or(0))
}

/// `twr log compact` for a `log.compact` result.
fn log_compact_text(r: &Value) -> String {
    let n = |k: &str| r[k].as_u64().unwrap_or(0);
    let mut out = String::new();
    if n("checkpoints_dropped") > 0 {
        out.push_str(&format!(
            "dropped {} checkpoint(s)\n",
            n("checkpoints_dropped")
        ));
    }
    match r["skipped"].as_str() {
        Some(why) => {
            out.push_str(&format!("moved nothing: {why}\n"));
            if n("blobs") > 0 {
                out.push_str(&format!(
                    "{} blob(s), {} no row left in twr.db reaches\n",
                    n("blobs"),
                    bytes_text(&r["blob_bytes"])
                ));
            }
        }
        None => {
            let whither = if r["archived"] == true {
                "into archive.db"
            } else {
                "deleted (archive = false)"
            };
            out.push_str(&format!(
                "{} agent event(s), {} ({} purged thread(s), {} turn(s)), {whither}\n",
                n("rows"),
                bytes_text(&r["bytes"]),
                n("purged_threads"),
                n("turns"),
            ));
            out.push_str(&format!(
                "{} blob(s), {} no row left in twr.db reaches\n",
                n("blobs"),
                bytes_text(&r["blob_bytes"])
            ));
            out.push_str(&format!(
                "{} transaction(s), the longest {} ms; {} ms in all\n",
                n("batches"),
                n("longest_batch_ms"),
                n("total_ms")
            ));
        }
    }
    if n("freed_pages") > 0 {
        out.push_str(&format!("{} free page(s) given back\n", n("freed_pages")));
    }
    if let Some(v) = r["vacuum"].as_object() {
        let b = |k: &str| bytes_text(&v[k]);
        out.push_str(&format!(
            "VACUUM: twr.db {} → {} in {} ms{}\n",
            b("before"),
            b("after"),
            v["ms"].as_u64().unwrap_or(0),
            if v["incremental_set"] == true {
                " (incremental vacuum on from now)"
            } else {
                ""
            }
        ));
    }
    out
}

/// `twr log stats` for a `log.stats` result.
fn log_stats_text(r: &Value) -> String {
    let mut out = String::new();
    let db = &r["db"];
    out.push_str(&format!(
        "twr.db       {} (write-ahead log {}; {} free page(s) of {}, auto_vacuum {})\n",
        bytes_text(&db["bytes"]),
        bytes_text(&db["wal"]),
        db["free_pages"].as_u64().unwrap_or(0),
        db["pages"].as_u64().unwrap_or(0),
        db["auto_vacuum"].as_str().unwrap_or("?"),
    ));
    let a = &r["archive"];
    if a.is_object() {
        out.push_str(&format!(
            "archive.db   {} (write-ahead log {})\n",
            bytes_text(&a["bytes"]),
            bytes_text(&a["wal"])
        ));
    } else {
        out.push_str("archive.db   none yet\n");
    }
    let kinds = |label: &str, v: &Value, out: &mut String| {
        let Some(k) = v["kinds"].as_object() else {
            return;
        };
        let rows: Vec<[String; 3]> = k
            .iter()
            .map(|(kind, e)| {
                [
                    kind.clone(),
                    e["rows"].as_u64().unwrap_or(0).to_string(),
                    bytes_text(&e["bytes"]),
                ]
            })
            .collect();
        out.push_str(&format!("\n{label}\n"));
        out.push_str(&table(&["KIND", "ROWS", "BYTES"], &rows));
        out.push_str(&format!(
            "blobs: {}, {}\n",
            v["blobs"]["count"].as_u64().unwrap_or(0),
            bytes_text(&v["blobs"]["bytes"])
        ));
    };
    kinds("twr.db", db, &mut out);
    if a.is_object() {
        kinds("archive.db", a, &mut out);
    }
    let b = &r["backlog"];
    out.push_str(&format!(
        "\ncontrol backlog: {} item(s), {} compacted\n",
        b["full"].as_u64().unwrap_or(0),
        b["compacted"].as_u64().unwrap_or(0)
    ));
    let cps = r["checkpoints"].as_array().cloned().unwrap_or_default();
    if cps.is_empty() {
        out.push_str("checkpoints: none\n");
    }
    for c in &cps {
        let age = c["age_secs"].as_u64().unwrap_or(0);
        out.push_str(&format!(
            "checkpoint {}: {} min old, {}, {} event(s) after it\n",
            c["id"],
            age / 60,
            bytes_text(&c["bytes"]),
            c["events_after"].as_u64().unwrap_or(0)
        ));
    }
    let next = &r["next"];
    out.push_str(&format!(
        "\nnext compaction ([log] retention_days = {}, archive = {}): {} agent event(s), {} \
         ({} purged thread(s), {} turn(s)){}\n",
        r["retention_days"].as_u64().unwrap_or(0),
        r["archive_on"].as_bool().unwrap_or(true),
        next["rows"].as_u64().unwrap_or(0),
        bytes_text(&next["bytes"]),
        next["purged_threads"].as_u64().unwrap_or(0),
        next["turns"].as_u64().unwrap_or(0),
        if next["automatic"] == true {
            ", within the hour"
        } else {
            "; the hub waits for 2 000 (`twr log compact` moves them now)"
        }
    ));
    out
}

async fn trash_list(socket: &Path, raw: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let r = c.call("trash.list", Value::Null).await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    out!("{}", trash_text(&r));
    Ok(())
}

/// An RFC 3339 time as `trash.list` gives it, shorter: `2026-10-04 12:30 UTC`.
/// A box of `hub.info` for `twr info`: its name, whether it's online, the twr version its
/// runner last reported, and while it's offline the latest refused connection that named it.
fn box_info_line(b: &Value) -> String {
    let state = if b["online"].as_bool() == Some(true) {
        "online"
    } else {
        "offline"
    };
    let mut line = format!("{} {state}", str_of(&b["name"]));
    if let Some(v) = b["version"].as_str() {
        line.push_str(&format!(" (twr {v})"));
    }
    if let Some(r) = b["refused"].as_object() {
        line.push_str(&format!(
            "; last refused {}: {}",
            trash_time(&r["at"]),
            str_of(&r["reason"])
        ));
    }
    line
}

fn trash_time(t: &Value) -> String {
    let t = str_of(t);
    match t.get(..16) {
        Some(head) => format!("{} UTC", head.replace('T', " ")),
        None => t.to_string(),
    }
}

/// `twr trash list` for a `trash.list` result: a table of its items, the boxes that could
/// not be asked, then the hub's message.
fn trash_text(r: &Value) -> String {
    let items = r["items"].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    if !items.is_empty() {
        let header = [
            "ID", "NAME", "PROJECT", "BOX", "DELETED", "PURGED", "SIZE", "LOSSES",
        ];
        let rows: Vec<[String; 8]> = items
            .iter()
            .map(|i| {
                let mut losses = i["losses"].as_str().unwrap_or_default().to_string();
                let restored = i["restored_as"]
                    .as_str()
                    .map(|n| format!("restored as {n} on {}", trash_time(&i["restored_at"])));
                let marks = [("incomplete", "incomplete"), ("box_offline", "box offline")]
                    .into_iter()
                    .filter(|(k, _)| i[*k].as_bool() == Some(true))
                    .map(|(k, mark)| match i["kept"].as_str() {
                        Some(why) if k == "incomplete" => format!("{mark}, kept: {why}"),
                        _ => mark.to_string(),
                    })
                    .chain(restored);
                for mark in marks {
                    losses = if losses.is_empty() {
                        format!("({mark})")
                    } else {
                        format!("{losses} ({mark})")
                    };
                }
                [
                    str_of(&i["short"]).to_string(),
                    str_of(&i["thread_name"]).to_string(),
                    str_of(&i["project"]).to_string(),
                    str_of(&i["box"]).to_string(),
                    trash_time(&i["deleted_at"]),
                    trash_time(&i["purge_at"]),
                    str_of(&i["size"]).to_string(),
                    losses,
                ]
            })
            .collect();
        out.push_str(&table(&header, &rows));
    }
    for u in r["unavailable"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "box {} could not list its trash: {}\n",
            str_of(&u["box"]),
            str_of(&u["error"])
        ));
    }
    out.push_str(&localize(str_of(&r["message"])));
    out.push('\n');
    out
}

/// The question `twr trash empty` asks before purging `items` (a dry run's, as `trash.list`
/// lists them, `size` in all): one line per item, then how many and how much.
fn trash_empty_question(items: &[Value], size: &str) -> String {
    let mut out = String::from("This purges, for good:\n");
    for i in items {
        out.push_str(&format!(
            "  {} ({}, box {}, {})\n",
            str_of(&i["short"]),
            str_of(&i["thread_name"]),
            str_of(&i["box"]),
            str_of(&i["size"])
        ));
    }
    let what = match items.len() {
        1 => "this item".to_string(),
        n => format!("these {n} items"),
    };
    out.push_str(&format!(
        "Purge {what} ({size})? This cannot be undone. [y/N] "
    ));
    out
}

/// `twr trash empty`: purges `items` (or every item with `all`) once `consent` allows it.
/// What the answer covers is settled before asking: the hub's dry run names the items (`all`
/// and names resolved then), the question lists them, and only those are purged, by id. An
/// item that arrives, or a name that comes to mean another item, while the question waits is
/// left alone; one purged meanwhile makes the hub refuse, purging nothing. With `--yes` the
/// same: what the dry run named, printed as it is purged.
async fn trash_empty(
    socket: &Path,
    items: &[String],
    all: bool,
    consent: Consent<'_>,
) -> anyhow::Result<()> {
    if matches!(consent, Consent::NoTerminal) {
        bail!("not purging without confirmation (no terminal to ask on): pass --yes to purge");
    }
    let mut c = connect(socket).await?;
    let chosen = c
        .call(
            "trash.empty",
            json!({ "items": items, "all": all, "dry_run": true }),
        )
        .await?;
    let listed = chosen["items"].as_array().cloned().unwrap_or_default();
    if listed.is_empty() {
        outln!("{}", localize(str_of(&chosen["message"])));
        return Ok(());
    }
    let ids: Vec<&str> = listed.iter().map(|i| str_of(&i["item"])).collect();
    if let Consent::Ask(ask) = consent {
        let reply = ask(&trash_empty_question(&listed, str_of(&chosen["size"])))?;
        if !matches!(reply.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            outln!("nothing purged");
            return Ok(());
        }
    }
    let r = c
        .call("trash.empty", json!({ "items": ids, "all": false }))
        .await?;
    outln!("{}", trash_empty_text(&r));
    if r["failed"].as_array().is_some_and(|f| !f.is_empty()) {
        bail!("some items could not be purged");
    }
    Ok(())
}

/// What `twr trash empty` prints for a `trash.empty` result.
fn trash_empty_text(r: &Value) -> String {
    let mut out = String::new();
    for p in r["purged"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "purged {} ({}, box {}, {})\n",
            str_of(&p["short"]),
            str_of(&p["thread_name"]),
            str_of(&p["box"]),
            human_bytes(p["bytes"].as_u64().unwrap_or(0))
        ));
    }
    for f in r["failed"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "could not purge {} (box {}): {}\n",
            str_of(&f["item"]),
            str_of(&f["box"]),
            str_of(&f["error"])
        ));
    }
    out.push_str(&localize(str_of(&r["message"])));
    out
}

// --- thread list -------------------------------------------------------------------------------

/// The model part of an orchestrator line: ` model <m>` or ` (the runtime's default model)`.
fn model_text(model: &Value) -> String {
    match model.as_str() {
        Some(m) => format!(" model {m}"),
        None => " (the runtime's default model)".into(),
    }
}

/// The model part of an orchestrator line once its runtime may have reported the model it runs
/// (`model_actual`): ` model <m>`, ` model <m> (runs <actual>)` when they differ, ` model
/// <actual> (the runtime's default)`, or [`model_text`] while nothing was reported.
fn models_text(model: &Value, actual: &Value) -> String {
    match (model.as_str(), actual.as_str()) {
        (Some(m), Some(a)) if m == a => format!(" model {m}"),
        (Some(m), Some(a)) => format!(" model {m} (runs {a})"),
        (None, Some(a)) => format!(" model {a} (the runtime's default)"),
        (_, None) => model_text(model),
    }
}

async fn orchestrator_show(socket: &Path, raw: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let o = c.call("orchestrator.show", Value::Null).await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&o)?);
        return Ok(());
    }
    out!("{}", localize(&orchestrator_text(&o)));
    Ok(())
}

/// The orchestrator's runtime as `twr orchestrator show`, `twr info` and `twr orchestrator
/// use` write it (`o`: an `orchestrator.show` object, `hub.info`'s `orchestrator`, an
/// `orchestrator.use` answer), as a thread's
/// ([`accounts::runtime_label`]): `claude-code:work` while its runtime shows account labels,
/// else the runtime alone; a removed account always shows, `claude-code:work (removed)`. (An
/// older hub's `runtime` may carry the configured account.)
fn orchestrator_runtime(o: &Value) -> String {
    let base = twr_core::account::split_runtime(str_of(&o["runtime"])).0;
    accounts::runtime_label(&json!({
        "runtime": base,
        "account": o["account"],
        "account_shown": o["account_shown"],
        "account_removed": o["account_removed"],
    }))
}

/// `twr orchestrator show` for an `orchestrator.show` result.
fn orchestrator_text(o: &Value) -> String {
    let runtime = orchestrator_runtime(o);
    let mut out = format!(
        "orchestrator: {runtime}{}{}\n",
        models_text(&o["model"], &o["model_actual"]),
        if o["available"].as_bool() == Some(true) {
            ""
        } else {
            " (runtime not available)"
        }
    );
    match o["thread"].as_str() {
        Some(t) => out.push_str(&format!(
            "thread:       {} ({t}) {}{}\n",
            str_of(&o["name"]),
            str_of(&o["status"]),
            if o["live"].as_bool() == Some(true) {
                ", running"
            } else {
                ""
            }
        )),
        None => out.push_str("thread:       none yet (created with the first message you say)\n"),
    }
    let conf = &o["configured"];
    // `[orchestrator] account`, or the account part of its runtime (written apart).
    let (conf_runtime, account) = match conf["account"].as_str() {
        Some(a) => (
            twr_core::account::split_runtime(str_of(&conf["runtime"])).0,
            format!(", account {a}"),
        ),
        None => (str_of(&conf["runtime"]), String::new()),
    };
    out.push_str(&format!(
        "configured:   {conf_runtime}{}{account} (the first orchestrator's; `twr orchestrator \
         use` switches)\n",
        model_text(&conf["model"])
    ));
    let runtimes: Vec<&str> = o["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    out.push_str(&format!("can run on:   {}\n", runtimes.join(", ")));
    if let Some(n) = o["retired"].as_u64().filter(|n| *n > 0) {
        out.push_str(&format!(
            "retired:      {n} earlier orchestrator thread(s)\n"
        ));
    }
    out
}

/// Asks the user a question on the terminal and returns the reply line.
type Ask<'a> = &'a mut dyn FnMut(&str) -> anyhow::Result<String>;

async fn orchestrator_use(
    socket: &Path,
    runtime: &str,
    model: Option<String>,
    mut summary: bool,
    wait: Option<u64>,
    ask: Option<Ask<'_>>,
) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let mut params = json!({ "runtime": runtime });
    if let Some(m) = model {
        params["model"] = json!(m);
    }
    let wait_secs = wait.unwrap_or(twr_hub::hub::SUMMARY_WAIT.as_secs());
    if summary {
        // Only a running orchestrator can write a summary; a busy one is waited for.
        let o = c.call("orchestrator.show", Value::Null).await?;
        let live = o["live"].as_bool() == Some(true);
        let busy = live
            && matches!(
                o["status"].as_str(),
                Some("running" | "background" | "waiting")
            );
        if busy && wait.is_none() {
            if let Some(ask) = ask {
                let reply = ask(&format!(
                    "the orchestrator is busy (work is running). Wait up to {wait_secs}s for \
                     it to finish and write a handoff summary? [Y/n] (n switches now without it) "
                ))?;
                summary = !matches!(reply.trim().to_ascii_lowercase().as_str(), "n" | "no");
            }
        }
        if summary && live {
            errln!(
                "twr: asking the current orchestrator for a handoff summary (waiting up to \
                 {wait_secs}s if it is busy; --no-summary skips it)..."
            );
        }
    }
    params["summary"] = json!(summary);
    if let Some(w) = wait {
        params["wait_secs"] = json!(w);
    }
    let r = c.call("orchestrator.use", params).await?;
    out!("{}", localize(&orchestrator_use_text(&r)));
    if let Some(w) = r["warning"].as_str() {
        errln!("twr: warning: {}", localize(w));
    }
    if let Some(e) = r["start_error"].as_str() {
        errln!("twr: the new orchestrator could not start yet: {e}");
    }
    Ok(())
}

/// `twr orchestrator use` for an `orchestrator.use` result.
fn orchestrator_use_text(r: &Value) -> String {
    orchestrator_session_text(r, false)
}

fn orchestrator_session_text(r: &Value, reset: bool) -> String {
    let mut out = format!(
        "orchestrator is now {}{}: thread {} ({})",
        orchestrator_runtime(r),
        model_text(&r["model"]),
        str_of(&r["name"]),
        str_of(&r["thread"])
    );
    out.push_str(if r["started"].as_bool() == Some(true) {
        ", started\n"
    } else {
        ", starts with the next message you say\n"
    });
    if reset {
        out.push_str("Conversation cleared; no handoff. History stays in the log; workers stay.\n");
    }
    let p = &r["previous"];
    if !p.is_object() {
        out.push_str("This is a new session (there was no orchestrator yet).\n");
        return out;
    }
    let dropped = p["dropped_prompts"].as_u64().unwrap_or(0);
    // `stopping`: its runtime was asked to stop and has not exited yet.
    let how = if p["stopping"].as_bool() == Some(true) {
        "was retired and is being stopped"
    } else {
        "was stopped and retired"
    };
    out.push_str(&format!(
        "The previous orchestrator ({}{}, {}) {how}",
        str_of(&p["runtime"]),
        models_text(&p["model"], &p["model_actual"]),
        str_of(&p["thread"])
    ));
    if dropped > 0 {
        out.push_str(&format!(" ({dropped} queued prompt(s) dropped)"));
    }
    out.push_str(".\n");
    let h = &r["handoff"];
    if !h.is_object() {
        if !reset {
            out.push_str(
                "No handoff was delivered: the new session starts without the conversation.\n",
            );
        }
        return out;
    }
    let exchanges = h["exchanges"].as_u64().unwrap_or(0);
    let mut what = format!("{exchanges} recent exchange(s)");
    if let Some(d) = h["dropped_exchanges"].as_u64().filter(|d| *d > 0) {
        what.push_str(&format!(" ({d} older dropped)"));
    }
    if h["summary"].as_bool() == Some(true) {
        out.push_str(&format!(
            "Handed over as its first turn: the previous orchestrator's summary, {what}, the \
             threads, attention items and reports.\n"
        ));
    } else {
        out.push_str(&format!(
            "Handed over as its first turn: {what}, the threads, attention items and reports; \
             no summary ({}).\n",
            str_of(&h["summary_note"])
        ));
    }
    out
}

/// The threads `twr thread list` shows: every one with `all`, else those not parked; and the
/// names of the parked ones it leaves out.
fn listed(threads: Vec<Value>, all: bool) -> (Vec<Value>, Vec<String>) {
    if all {
        return (threads, Vec::new());
    }
    let (parked, shown): (Vec<Value>, Vec<Value>) = threads
        .into_iter()
        .partition(|t| t["parked"].as_bool() == Some(true));
    let hidden = parked
        .iter()
        .map(|t| str_of(&t["name"]).to_string())
        .collect();
    (shown, hidden)
}

/// The line under `twr thread list` naming the parked threads it left out.
fn parked_footer(hidden: &[String]) -> String {
    format!(
        "{} parked thread{} hidden: {} (twr{} thread list --all)",
        hidden.len(),
        if hidden.len() == 1 { "" } else { "s" },
        hidden.join(", "),
        workspace_hint()
    )
}

async fn thread_list(socket: &Path, all: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let threads = c.call("thread.list", Value::Null).await?;
    let threads = threads.as_array().cloned().unwrap_or_default();
    if threads.is_empty() {
        outln!(
            "no threads (create one with `twr{} thread new <project>`)",
            workspace_hint()
        );
        return Ok(());
    }
    let (threads, hidden) = listed(threads, all);
    if threads.is_empty() {
        outln!("{}", parked_footer(&hidden));
        return Ok(());
    }
    let header = [
        "NAME", "STATUS", "RUNTIME", "MODE", "MODEL", "PROJECT", "TURNS", "SPENT", "DISK", "ATTN",
        "QUEUED", "ID", "ACTIVITY",
    ];
    let rows: Vec<[String; 13]> = threads
        .iter()
        .map(|t| {
            let attn = t["open_attention"].as_object().map_or(0, |m| m.len());
            [
                str_of(&t["name"]).to_string(),
                if t["retired"].as_bool() == Some(true) {
                    "retired".to_string()
                } else {
                    let count = t["background_task_count"].as_u64().unwrap_or(0);
                    if count == 0 {
                        str_of(&t["status"]).to_string()
                    } else {
                        format!("{} ({count} bg)", str_of(&t["status"]))
                    }
                },
                accounts::runtime_label(t),
                mode_cell(t),
                model_cell(t),
                str_of(&t["project"]).to_string(),
                t["turns"].as_u64().unwrap_or(0).to_string(),
                spent_cell(t),
                disk_cell(t),
                attn.to_string(),
                t["pending"].as_u64().unwrap_or(0).to_string(),
                str_of(&t["id"]).to_string(),
                activity_cell(t),
            ]
        })
        .collect();
    out!("{}", table(&header, &rows));
    if !hidden.is_empty() {
        outln!("{}", parked_footer(&hidden));
    }
    Ok(())
}

/// `twr job ...`.
async fn job_cmd(socket: &Path, cmd: JobCmd) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    match cmd {
        JobCmd::Run {
            thread,
            name,
            slots,
            timeout,
            no_wake,
            argv,
        } => {
            let thread = thread.or_else(|| std::env::var("TWR_THREAD").ok());
            let timeout_secs = timeout.as_deref().map(parse_duration_secs).transpose()?;
            let r = c
                .call(
                    "job.start",
                    json!({"argv": argv, "thread": thread, "name": name, "slots": slots,
                           "timeout_secs": timeout_secs, "wake": !no_wake}),
                )
                .await?;
            let ahead = r["position"].as_u64().unwrap_or(0);
            outln!(
                "job {} ({}) for {}: {}; its output goes to {}{}",
                str_of(&r["job"]),
                str_of(&r["name"]),
                str_of(&r["thread_name"]),
                if ahead == 0 {
                    "starts when its slots are free".to_string()
                } else {
                    format!("queued, {ahead} ahead")
                },
                str_of(&r["log"]),
                if no_wake {
                    ""
                } else {
                    "; the thread gets a prompt when it ends"
                }
            );
        }
        JobCmd::List { thread, json } => {
            let r = c.call("job.list", json!({ "thread": thread })).await?;
            if json {
                outln!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let jobs = r.as_array().cloned().unwrap_or_default();
            if jobs.is_empty() {
                outln!("no job queued or running");
                return Ok(());
            }
            let rows: Vec<[String; 5]> = jobs
                .iter()
                .map(|j| {
                    [
                        str_of(&j["job"]).to_string(),
                        str_of(&j["thread_name"]).to_string(),
                        str_of(&j["name"]).to_string(),
                        match j["running_secs"].as_u64() {
                            Some(s) => format!("running {s}s"),
                            None => format!("queued #{}", j["position"].as_u64().unwrap_or(0) + 1),
                        },
                        j["slots"].as_u64().unwrap_or(1).to_string(),
                    ]
                })
                .collect();
            out!(
                "{}",
                table(&["JOB", "THREAD", "NAME", "STATE", "SLOTS"], &rows)
            );
        }
        JobCmd::Log { job, lines } => {
            let r = c
                .call("job.log", json!({ "job": job, "lines": lines }))
                .await?;
            outln!("{}", str_of(&r["text"]));
        }
        JobCmd::Cancel { job } => {
            let r = c.call("job.cancel", json!({ "job": job })).await?;
            outln!("cancelling job {}", str_of(&r["job"]));
        }
    }
    Ok(())
}

/// How a thread stands, for `twr wait`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitState {
    /// Still working: a turn, background work or a job, or prompts queued (also after other
    /// threads).
    Busy,
    /// Waiting on the user: a permission, question or card.
    Waiting,
    /// Its last turn completed (or it never ran a turn).
    Done,
    /// Its last turn failed or was interrupted, it crashed, or it is stopped or parked: it
    /// doesn't go on by itself.
    Failed,
}

/// How a thread waited for ended: its exit code (`twr wait`'s, the worst of them with `--all`).
fn wait_code(s: WaitState) -> u8 {
    match s {
        WaitState::Done => 0,
        WaitState::Waiting => 2,
        WaitState::Failed => 3,
        WaitState::Busy => 124,
    }
}

/// How thread `t` (as `thread.get` returns it) stands, and whether that may still change by
/// itself within moments: an interim report (it ended a turn with background work still
/// running, and the runtime may start a turn of its own once that ends) or a failure (an
/// automatic retry may follow).
fn wait_state(t: &Value) -> (WaitState, bool) {
    let status = t["status"].as_str().unwrap_or("");
    // Stopped or parked: nothing goes on, whatever is queued for it.
    if status == "parked" || t["stopped"].as_bool() == Some(true) {
        return (WaitState::Failed, false);
    }
    if status == "waiting" {
        return (WaitState::Waiting, false);
    }
    if matches!(status, "running" | "background" | "attached") {
        return (WaitState::Busy, false);
    }
    // On a box not connected and reconciled, what it shows may be old: its runtime may have
    // gone on there.
    if t["box_online"].as_bool() == Some(false) {
        return (WaitState::Busy, false);
    }
    let pending = t["pending"].as_u64().unwrap_or(0);
    // A prompt queued for it after other threads is work to come too, unless a card about it
    // waits on the user (a thread it waits for didn't finish well).
    let held = t["queued_after"].as_array();
    if held.is_some_and(|g| g.iter().any(|p| !p["card"].is_null())) {
        return (WaitState::Waiting, false);
    }
    let gated = held.is_some_and(|g| !g.is_empty());
    let last = &t["last_turn"];
    let failed =
        status == "error" || matches!(last["result"].as_str(), Some("error" | "interrupted"));
    // A prompt held for it after other threads goes out by itself once they finished, after
    // a failure of its own too, and its dispatch releases a queue the failure held.
    if gated {
        return (WaitState::Busy, false);
    }
    if pending > 0 {
        // A queue held after a failure goes nowhere until someone acts.
        return if failed {
            (WaitState::Failed, true)
        } else {
            (WaitState::Busy, false)
        };
    }
    if failed {
        return (WaitState::Failed, true);
    }
    // A worker's completed turn has its report written just after it ends (the
    // orchestrator's turns have none).
    let orchestrator = t["role"].as_str() == Some("orchestrator");
    if !orchestrator
        && last["result"].as_str() == Some("completed")
        && t["reported"].as_bool() != Some(true)
    {
        return (WaitState::Busy, false);
    }
    let interim = t["last_report"]["turn"] == last["turn"]
        && t["last_report"]["ext"]["twr"]["background_tasks"]
            .as_u64()
            .is_some_and(|n| n > 0);
    (WaitState::Done, interim)
}

/// What `twr wait` says of thread `t` that ended `state`.
fn wait_line(t: &Value, state: WaitState) -> String {
    let name = t["name"].as_str().unwrap_or("?");
    let turn = t["last_turn"]["turn"].as_u64();
    match state {
        WaitState::Done => {
            let report = t["last_report"]["summary"].as_str().unwrap_or("").trim();
            // What its report says of how it went, when that needs a look.
            let grade = match t["last_report"]["grade"].as_str() {
                Some("gave_up") => " (it gave up)",
                Some("needs_verification") => " (it needs verification)",
                Some("partial") => " (partly)",
                _ => "",
            };
            match (turn, report.is_empty()) {
                (Some(n), false) => format!("{name} finished turn {n}{grade}:\n{report}"),
                (Some(n), true) => format!("{name} finished turn {n}{grade}"),
                (None, _) => format!("{name} is idle"),
            }
        }
        WaitState::Waiting => format!(
            "{name} waits on you: `twr{w} attention` shows what",
            w = workspace_hint()
        ),
        WaitState::Failed => {
            let why = if t["parked"].as_bool() == Some(true) {
                "is parked".to_string()
            } else if t["stopped"].as_bool() == Some(true) {
                "was stopped".to_string()
            } else {
                match (t["last_turn"]["result"].as_str(), turn) {
                    (Some("interrupted"), Some(n)) => format!("turn {n} was interrupted"),
                    (_, Some(n)) => format!("turn {n} failed"),
                    _ => "failed".to_string(),
                }
            };
            format!("{name}: {why}")
        }
        WaitState::Busy => format!("{name} is still working"),
    }
}

/// `twr wait`: waits until the threads (any one; each with `all`) finish or wait on the user,
/// at most `timeout` seconds (the hub's answers too), and prints how each ended. Exit code: 0
/// finished, 2 waiting on you, 3 failed (or stopped, parked, interrupted), 124 timed out.
async fn wait_cmd(
    socket: &Path,
    threads: &[String],
    all: bool,
    timeout: Option<u64>,
    as_json: bool,
) -> anyhow::Result<ExitCode> {
    let deadline = match timeout {
        None => None,
        Some(s) => Some(
            tokio::time::Instant::now()
                .checked_add(Duration::from_secs(s))
                .ok_or_else(|| anyhow::anyhow!("--timeout {s}s is too long"))?,
        ),
    };
    // Each step bounded by the deadline: `None` once it passed.
    async fn bounded<T>(
        deadline: Option<tokio::time::Instant>,
        f: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<Option<T>> {
        match deadline {
            Some(d) => match tokio::time::timeout_at(d, f).await {
                Ok(r) => r.map(Some),
                Err(_) => Ok(None),
            },
            None => f.await.map(Some),
        }
    }
    // What may still change by itself counts once it stayed so this long.
    const SETTLE: Duration = Duration::from_secs(5);
    let Some(mut c) = bounded(deadline, connect(socket)).await? else {
        errln!("twr: timed out connecting to the hub");
        return Ok(ExitCode::from(124));
    };
    // The threads, each once, by id: the last of each seen, and how it ended.
    let mut ids: Vec<String> = Vec::new();
    let mut last: HashMap<String, Value> = HashMap::new();
    for q in threads {
        let Some(t) = bounded(deadline, c.call("thread.get", json!({ "thread": q }))).await? else {
            return Ok(timed_out(&ids, &last, &[], as_json));
        };
        let id = t["id"].as_str().unwrap_or(q).to_string();
        if !ids.contains(&id) {
            ids.push(id.clone());
        }
        last.insert(id, t);
    }
    // Since when each thread has looked as it does (its last turn, report and activity): what
    // may still change counts once it stayed so for [`SETTLE`], even when a whole turn came
    // and went between two looks.
    let mut settled_since: HashMap<String, (Value, std::time::Instant)> = HashMap::new();
    let mut ended: Vec<(String, WaitState)> = Vec::new();
    let mut first = true;
    loop {
        for id in &ids {
            if ended.iter().any(|(t, _)| t == id) {
                continue;
            }
            if !first {
                let got = bounded(deadline, c.call("thread.get", json!({ "thread": id }))).await?;
                let Some(t) = got else {
                    return Ok(timed_out(&ids, &last, &ended, as_json));
                };
                last.insert(id.clone(), t);
            }
            // When it was seen so: after the hub answered.
            let now = std::time::Instant::now();
            let t = &last[id];
            let (state, may_change) = wait_state(t);
            let looks = json!([t["last_turn"], t["last_report"]["turn"], t["last_activity"]]);
            let counted = match state {
                WaitState::Busy => {
                    settled_since.remove(id);
                    false
                }
                _ if may_change => {
                    let since = match settled_since.get(id) {
                        Some((seen, at)) if *seen == looks => *at,
                        _ => {
                            settled_since.insert(id.clone(), (looks, now));
                            now
                        }
                    };
                    now.duration_since(since) >= SETTLE
                }
                _ => true,
            };
            if counted {
                ended.push((id.clone(), state));
                // The first one is all it waits for, without `--all`.
                if !all {
                    break;
                }
            }
        }
        first = false;
        let finished = if all {
            ended.len() == ids.len()
        } else {
            !ended.is_empty()
        };
        if finished {
            let mut code = 0u8;
            for (id, state) in &ended {
                code = code.max(wait_code(*state));
                print_waited(&last[id], *state, as_json);
            }
            return Ok(ExitCode::from(code));
        }
        let pause = tokio::time::Instant::now() + Duration::from_secs(1);
        match deadline {
            Some(d) if d <= pause => {
                tokio::time::sleep_until(d).await;
                return Ok(timed_out(&ids, &last, &ended, as_json));
            }
            _ => tokio::time::sleep_until(pause).await,
        }
    }
}

/// `twr wait` timed out: how each thread ended, and the rest as last seen; exit code 124.
fn timed_out(
    ids: &[String],
    last: &HashMap<String, Value>,
    ended: &[(String, WaitState)],
    as_json: bool,
) -> ExitCode {
    for id in ids {
        let state = ended
            .iter()
            .find(|(t, _)| t == id)
            .map_or(WaitState::Busy, |(_, s)| *s);
        if let Some(t) = last.get(id) {
            print_waited(t, state, as_json);
        }
    }
    ExitCode::from(124)
}

fn print_waited(t: &Value, state: WaitState, as_json: bool) {
    let covered = if t["last_report"]["turn"] == t["last_turn"]["turn"] {
        t["last_report"].clone()
    } else {
        Value::Null
    };
    if as_json {
        let state_name = match state {
            WaitState::Done => "done",
            WaitState::Waiting => "waiting",
            WaitState::Failed => "failed",
            WaitState::Busy => "timed_out",
        };
        outln!(
            "{}",
            json!({
                "thread": t["id"],
                "name": t["name"],
                "state": state_name,
                "turn": t["last_turn"]["turn"],
                // Only the report of that turn (a failed or interrupted one has none).
                "report": covered["summary"],
                "grade": covered["grade"],
            })
        );
    } else {
        outln!("{}", wait_line(t, state));
    }
}

/// `--timeout`: `90s`, `30m`, `2h` or plain seconds.
fn parse_duration_secs(s: &str) -> anyhow::Result<u64> {
    let s = s.trim();
    let bad = || anyhow::anyhow!("--timeout {s:?}: give a duration such as 90s, 30m or 2h");
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().map_err(|_| bad())?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(bad()),
    };
    n.checked_mul(mult).filter(|v| *v > 0).ok_or_else(bad)
}

/// `twr trim`'s line for a `thread.trim` answer: `trimmed api: deleted target/, node_modules/
/// (7.1 GB)`, or that there was nothing, then its notes.
fn trim_outcome(r: &Value) -> String {
    let name = str_of(&r["thread_name"]);
    let deleted: Vec<&str> = r["deleted"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out = if deleted.is_empty() {
        format!("trimmed {name}: no build output to delete")
    } else {
        format!(
            "trimmed {name}: deleted {} ({})",
            deleted.join(", "),
            twr_core::git::human_bytes(r["freed"].as_u64().unwrap_or(0))
        )
    };
    for n in r["notes"].as_array().into_iter().flatten() {
        out.push_str(&format!("\n  {}", n.as_str().unwrap_or_default()));
    }
    out
}

/// The DISK cell of `twr thread list`: its worktree's size and its build output's, as last
/// measured (`7.1 GB (6.8 GB build)`), `-` before that.
fn disk_cell(t: &Value) -> String {
    let Some(d) = t.get("disk").filter(|d| d.is_object()) else {
        return "-".into();
    };
    let (bytes, build) = (
        d["bytes"].as_u64().unwrap_or(0),
        d["build_bytes"].as_u64().unwrap_or(0),
    );
    let at_least = if d["partial"].as_bool() == Some(true) {
        "≥"
    } else {
        ""
    };
    let size = twr_core::git::human_bytes(bytes);
    if build == 0 {
        format!("{at_least}{size}")
    } else {
        format!(
            "{at_least}{size} ({} build)",
            twr_core::git::human_bytes(build)
        )
    }
}

/// The SPENT cell of `twr thread list`: the tokens its turns used and their cost when the
/// runtime reports one (`1.2M tok $3.40`), `-` before any turn reported usage.
fn spent_cell(t: &Value) -> String {
    serde_json::from_value::<twr_core::usage::Spent>(t["spent"].clone())
        .ok()
        .and_then(|s| s.short())
        .unwrap_or_else(|| "-".into())
}

/// The ACTIVITY cell of `twr thread list`: the thread's activity line (`activity`), cut to
/// [`ACTIVITY_CELL`] characters.
fn activity_cell(t: &Value) -> String {
    twr_core::activity::cut(str_of(&t["activity"]), ACTIVITY_CELL)
}

/// Longest ACTIVITY cell of `twr thread list`, in characters.
const ACTIVITY_CELL: usize = 60;

/// The MODE cell of `twr thread list`: the thread's permission mode, and the mode its live
/// runtime still runs in until it restarts (`runtime_mode`).
fn mode_cell(t: &Value) -> String {
    let mode = str_of(&t["mode"]);
    match t["runtime_mode"].as_str() {
        Some(running) => format!("{mode} (running {running} until restarted)"),
        None => mode.to_string(),
    }
}

/// The MODEL cell of `twr thread list`: the model the runtime reports it runs
/// (`model_actual`), else the requested one marked `(requested)`, else `(runtime default)`.
fn model_cell(t: &Value) -> String {
    match (t["model_actual"].as_str(), t["model"].as_str()) {
        (Some(a), _) => a.to_string(),
        (None, Some(m)) => format!("{m} (requested)"),
        (None, None) => "(runtime default)".into(),
    }
}

fn table<const N: usize>(header: &[&str; N], rows: &[[String; N]]) -> String {
    let mut widths = header.map(str::len);
    for row in rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let fmt_row = |cells: Vec<&str>| {
        let line: Vec<String> = cells
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        format!("{}\n", line.join("  ").trim_end())
    };
    let mut out = fmt_row(header.to_vec());
    for row in rows {
        out.push_str(&fmt_row(row.iter().map(String::as_str).collect()));
    }
    out
}

// --- events / control --------------------------------------------------------------------------

/// Restores the fields moved to the blob store, fetching blobs on demand (chunked `blob.get`),
/// including fields whose pointers only appear inside restored content.
async fn hydrate_event(c: &mut Client, ev: &Event) -> anyhow::Result<Value> {
    let mut v = serde_json::to_value(ev)?;
    hydrate_deep(&mut v["data"], c)
        .await
        .with_context(|| format!("hydrating event {}", ev.id))?;
    Ok(v)
}

/// A control item with the event's own field order (the `json!` map would sort keys).
#[derive(serde::Serialize)]
struct ControlLine<'a> {
    stream: &'a str,
    gseq: u64,
    event: &'a Event,
}

struct Printer {
    pretty: bool,
    /// `--pretty` line merging (streamed deltas, shutdown stops).
    merged: pretty::Pretty,
    names: HashMap<String, String>,
    out: std::io::StdoutLock<'static>,
}

/// While following with `--pretty`, a merged line held back this long (the stream paused) is
/// printed.
const PRETTY_PAUSE: Duration = Duration::from_millis(300);

impl Printer {
    fn new(pretty: bool) -> Self {
        let tty = std::io::IsTerminal::is_terminal(&std::io::stdout());
        Printer {
            pretty,
            merged: pretty::Pretty::new(tty),
            names: HashMap::new(),
            out: std::io::stdout().lock(),
        }
    }

    /// Prints the merged line held back, if any (the end, or a pause while following).
    fn flush(&mut self) -> anyhow::Result<()> {
        if let Some(l) = self.merged.flush(&self.names) {
            self.write(&l);
        }
        Ok(())
    }

    /// How long to wait for the next streamed item before [`Printer::flush`]: forever unless
    /// a merged line is held back.
    fn pause(&self) -> Option<Duration> {
        self.merged.holding().then_some(PRETTY_PAUSE)
    }

    fn write(&mut self, line: &str) {
        if writeln!(self.out, "{line}")
            .and_then(|_| self.out.flush())
            .is_err()
        {
            std::process::exit(0);
        }
    }

    fn learn(&mut self, ev: &Event) {
        if let twr_core::event::EventBody::ThreadCreated(p) = &ev.body {
            self.names.insert(ev.thread.clone(), p.name.clone());
        }
    }

    /// Prints an event (or its hydrated JSON). Broken pipes end the program quietly.
    fn event(&mut self, gseq: Option<u64>, ev: &Event, json: Option<Value>) -> anyhow::Result<()> {
        self.learn(ev);
        if self.pretty {
            let hydrated = json.map(serde_json::from_value::<Event>);
            let ev = match &hydrated {
                Some(Ok(h)) => h,
                _ => ev,
            };
            for l in self.merged.push(gseq, ev, &self.names) {
                self.write(&l);
            }
            return Ok(());
        }
        let line = {
            match (gseq, json) {
                (Some(g), Some(j)) => {
                    json!({ "stream": "control", "gseq": g, "event": j }).to_string()
                }
                (Some(g), None) => serde_json::to_string(&ControlLine {
                    stream: "control",
                    gseq: g,
                    event: ev,
                })?,
                (None, Some(j)) => j.to_string(),
                (None, None) => serde_json::to_string(ev)?,
            }
        };
        // (Pretty lines are cleaned by `pretty`, around its own styling.)
        self.write(&term::clean(&line));
        Ok(())
    }
}

/// A subscription's items, read by a task of their own: waiting for the next one can be cut
/// short (a pause, [`next_item`]) without losing what was read of it.
type Items = tokio::sync::mpsc::Receiver<anyhow::Result<StreamItem>>;

fn items(mut sub: twr_core::client::Subscription) -> Items {
    let (tx, rx) = tokio::sync::mpsc::channel(256);
    tokio::spawn(async move {
        loop {
            let r = sub.next().await.transpose();
            let end = !matches!(r, Some(Ok(_)));
            let Some(r) = r else { return };
            if tx.send(r).await.is_err() || end {
                return;
            }
        }
    });
    rx
}

/// The next streamed item; with a merged line held back (`p.pause()`), a pause in the stream
/// prints it first.
async fn next_item(sub: &mut Items, p: &mut Printer) -> anyhow::Result<Option<StreamItem>> {
    loop {
        match p.pause() {
            None => return sub.recv().await.transpose(),
            Some(d) => match tokio::time::timeout(d, sub.recv()).await {
                Ok(r) => return r.transpose(),
                Err(_) => p.flush()?,
            },
        }
    }
}

async fn events(
    socket: &Path,
    thread: &str,
    follow: bool,
    since: u64,
    hydrate: bool,
    pretty: bool,
) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let mut p = Printer::new(pretty);
    if let Ok(t) = c.call("thread.get", json!({ "thread": thread })).await {
        p.names
            .insert(str_of(&t["id"]).to_string(), str_of(&t["name"]).to_string());
    }
    // The stored events page by page through `thread.events`, which reads the archive too
    // (`[log]`); then, with `--follow`, what comes after them, from the subscription, with
    // no gap or repeat: it delivers what has a seq above the last one printed. After the
    // first page, the thread is named by the id that page answered with: a name or alias
    // that comes to mean another thread meanwhile (an orchestrator switch, a new thread
    // taking the name) does not change which one is read.
    let (id, since) = print_stored(&mut c, &mut p, thread, since, hydrate).await?;
    if !follow {
        return p.flush();
    }
    hub_test_gate(EVENTS_FOLLOW_GATE_ENV);
    let sub_conn = connect(socket).await?;
    let sub = match sub_conn
        .subscribe(
            "thread.subscribe",
            json!({ "thread": id, "since_seq": since }),
        )
        .await
    {
        Ok(sub) => sub,
        // It cannot be followed: deleted (now or before; read by its id), it records nothing
        // more. What it recorded since the last page (its deletion) is printed first.
        Err(e) if is_not_found(&e) => {
            print_stored(&mut c, &mut p, &id, since, hydrate).await?;
            p.flush()?;
            errln!("{thread} is deleted: nothing more comes");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mut sub = items(sub);
    while let Some(item) = next_item(&mut sub, &mut p).await? {
        print_item(&mut c, &mut p, &item, hydrate).await?;
    }
    p.flush()
}

/// `TWR_TEST_EVENTS_FOLLOW_GATE=<file>`, for tests of `twr events --follow`: once the stored
/// events are printed, before it subscribes, it waits at the gate ([`hub_test_gate`]).
const EVENTS_FOLLOW_GATE_ENV: &str = "TWR_TEST_EVENTS_FOLLOW_GATE";

/// Prints the stored events of thread `thread` with seq above `since`, page by page
/// (`thread.events`, archived ones too): the thread's id, as the hub resolved it, and the
/// last seq printed (`since` when none was). Later pages name the thread by that id.
async fn print_stored(
    c: &mut Client,
    p: &mut Printer,
    thread: &str,
    since: u64,
    hydrate: bool,
) -> anyhow::Result<(String, u64)> {
    let mut since = since;
    let mut id = thread.to_string();
    loop {
        let r = c
            .call(
                "thread.events",
                json!({ "thread": id, "since_seq": since, "limit": PAGE }),
            )
            .await?;
        if let Some(t) = r["thread"].as_str() {
            id = t.to_string();
        }
        let evs: Vec<Event> = serde_json::from_value(r["events"].clone())?;
        for ev in &evs {
            let j = if hydrate {
                Some(hydrate_event(c, ev).await?)
            } else {
                None
            };
            p.event(None, ev, j)?;
        }
        since = r["next"]
            .as_u64()
            .or(evs.last().map(|e| e.seq))
            .unwrap_or(since);
        // Pages are bounded by size too: `more` says whether to continue from `next`.
        if evs.is_empty() || r["more"] != true {
            return Ok((id, since));
        }
    }
}

async fn print_item(
    c: &mut Client,
    p: &mut Printer,
    item: &StreamItem,
    hydrate: bool,
) -> anyhow::Result<()> {
    let j = if hydrate {
        Some(hydrate_event(c, &item.event).await?)
    } else {
        None
    };
    p.event(item.gseq, &item.event, j)
}

async fn control(socket: &Path, follow: bool, since: u64, pretty: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let mut p = Printer::new(pretty);
    if pretty && since > 0 {
        // Items before `since` are not replayed; learn names from the thread list instead.
        let threads = c.call("thread.list", Value::Null).await?;
        for t in threads.as_array().into_iter().flatten() {
            p.names
                .insert(str_of(&t["id"]).to_string(), str_of(&t["name"]).to_string());
        }
    }
    if follow {
        let sub = c
            .subscribe("control.subscribe", json!({ "since": since }))
            .await?;
        let mut sub = items(sub);
        while let Some(item) = next_item(&mut sub, &mut p).await? {
            p.event(item.gseq, &item.event, None)?;
        }
        return p.flush();
    }
    let mut since = since;
    loop {
        let r = c
            .call("control.read", json!({ "since": since, "limit": PAGE }))
            .await?;
        let items: Vec<StreamItem> = serde_json::from_value(r["items"].clone())?;
        for item in &items {
            p.event(item.gseq, &item.event, None)?;
            since = item.gseq.unwrap_or(since);
        }
        if items.is_empty() || r["more"] != true {
            return p.flush();
        }
        since = r["next"].as_u64().unwrap_or(since);
    }
}

// --- bare `twr` --------------------------------------------------------------------------------

// --- workspaces ---------------------------------------------------------------------------------

/// `twr ws`: lists, creates or removes the root's workspaces (whichever workspace `paths` is in).
async fn ws(cmd: Option<WsCmd>, paths: &Paths) -> anyhow::Result<()> {
    let cmd = cmd.unwrap_or(WsCmd::List { json: false });
    if matches!(cmd, WsCmd::Rm { .. }) {
        refuse_agent()?;
    }
    match cmd {
        WsCmd::List { json } => ws_list(paths, json).await,
        WsCmd::New { name } => ws_new(paths, &name),
        WsCmd::Rm { name } => ws_rm(paths, &name).await,
    }
}

/// `twr ws list`: every workspace (`default` first), with what its hub reports when it runs.
async fn ws_list(paths: &Paths, json: bool) -> anyhow::Result<()> {
    let root = paths.root();
    // The root's config: its `hub.socket` is the `default` workspace's socket.
    let config = Config::load_from(&root)?;
    let mut names = vec![DEFAULT_WORKSPACE.to_string()];
    names.extend(
        root.named_workspaces()
            .with_context(|| format!("listing {}", root.workspaces_dir().display()))?,
    );
    let mut rows = Vec::new();
    for name in names {
        let ws = root.workspace(&name)?;
        let mut row = ws_status(&config.socket_path(&ws), &name).await;
        row["name"] = json!(name);
        row["current"] = json!(name == paths.workspace_name());
        rows.push(row);
    }
    if json {
        outln!("{}", Value::Array(rows));
        return Ok(());
    }
    let count = |r: &Value, k: &str| r[k].as_u64().map_or("-".to_string(), |n| n.to_string());
    let table_rows: Vec<[String; 5]> = rows
        .iter()
        .map(|r| {
            let mark = if r["current"] == true { "* " } else { "  " };
            [
                format!("{mark}{}", str_of(&r["name"])),
                str_of(&r["hub"]).to_string(),
                count(r, "threads"),
                count(r, "running"),
                count(r, "waiting"),
            ]
        })
        .collect();
    out!(
        "{}",
        table(
            &["  WORKSPACE", "HUB", "THREADS", "RUNNING", "WAITING"],
            &table_rows
        )
    );
    for r in &rows {
        if let Some(e) = r["error"].as_str() {
            errln!("twr: workspace {}: {e}", str_of(&r["name"]));
        }
    }
    Ok(())
}

/// What `twr ws` shows of workspace `name`'s hub on `socket`: `{"hub": "stopped"}` when
/// nothing listens ([`nothing_listens`]), `"error"` with the `error` when the socket can't be
/// used (another user's, say), `"busy"` when it does not answer in time, `"other"` when it
/// serves another workspace, else `"running"` with its worker threads, how many run, and how
/// many items block on you.
async fn ws_status(socket: &Path, name: &str) -> Value {
    let mut c = match Client::connect(socket).await {
        Ok(c) => c,
        Err(e) if nothing_listens(&e) => return json!({ "hub": "stopped" }),
        Err(e) if backlog_full(&e) => return json!({ "hub": "busy" }),
        Err(e) => return json!({ "hub": "error", "error": format!("{e:#}") }),
    };
    let answer = tokio::time::timeout(PROBE_TIMEOUT, async {
        let hello = c.hello(json!({ "client": "cli" })).await?;
        if !hub_serves(&hello, name) {
            return Ok(None);
        }
        let threads = c.call("thread.list", Value::Null).await?;
        let items = c.call("attention.list", json!({ "all": true })).await?;
        anyhow::Ok(Some((threads, items)))
    })
    .await;
    let (threads, items) = match answer {
        Ok(Ok(Some(answer))) => answer,
        Ok(Ok(None)) => return json!({ "hub": "other" }),
        _ => return json!({ "hub": "busy" }),
    };
    let workers: Vec<&Value> = threads
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["role"] != "orchestrator")
        .collect();
    let running = workers
        .iter()
        .filter(|t| matches!(t["status"].as_str(), Some("running" | "background")))
        .count();
    let waiting = items
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| is_blocking(i))
        .count();
    json!({
        "hub": "running",
        "threads": workers.len(),
        "running": running,
        "waiting": waiting,
    })
}

/// `twr ws new`: makes the workspace's data directory (private, like every data directory).
fn ws_new(paths: &Paths, name: &str) -> anyhow::Result<()> {
    create_workspace(&paths.root(), name)?;
    outln!("created workspace {name}; open it with `twr -w {name}`");
    Ok(())
}

/// Workspaces are created and removed without asking a hub, which would refuse an agent: so
/// this process refuses to when it is one ([`twr_hub::local_agent`]).
fn refuse_agent() -> anyhow::Result<()> {
    refuse_agent_action("create or remove workspaces")
}

fn refuse_agent_action(action: &str) -> anyhow::Result<()> {
    match calling_agent() {
        Some(agent) => bail!(
            "this request comes from {agent}: agents cannot {action}; only you \
             can, from your own terminal"
        ),
        None => Ok(()),
    }
}

/// Whether this process runs for a thread's agent ([`calling_agent`]): what it prints is then
/// read by that agent (`twr box doctor` never shows it a command box's words).
fn caller_is_agent() -> bool {
    calling_agent().is_some()
}

/// The thread's agent this process runs for, as said ([`twr_hub::local_agent`]); none for a
/// test's own processes ([`test_user_waived`]).
fn calling_agent() -> Option<String> {
    // A test's own processes, on its throwaway home only (`TWR_HOME`), with the token its
    // hubs share (`TWR_TEST_USER_TOKEN_FILE`): run from inside a thread, they would count as
    // that thread's agent.
    if test_user_waived(
        test_token_trusted(),
        std::env::var_os(twr_core::client::TEST_USER_TOKEN_ENV).as_deref(),
        twr_core::paths::effective_uid(),
    ) {
        return None;
    }
    twr_hub::local_agent(std::process::id()).map(|agent| agent.detail())
}

/// Whether [`refuse_agent_action`] lets this process act as the user: it honours the test
/// token (`trusted`, [`test_token_trusted`]) and `file` (`TWR_TEST_USER_TOKEN_FILE`) is one a
/// hub would trust, a private file of `uid` holding a token ([`private_token_file`]), not any
/// file an agent could write and name.
fn test_user_waived(trusted: bool, file: Option<&std::ffi::OsStr>, uid: u32) -> bool {
    trusted && file.is_some_and(|f| private_token_file(Path::new(f), uid).is_ok())
}

/// Whether this process honours the test token (`TWR_TEST_USER_TOKEN_FILE`): a debug build
/// (the tests' own) on a home `TWR_HOME` names. Any process can write a token file and name
/// it, so an installed twr, a release build, never does: an agent can't make itself the user
/// of an instance with one.
fn test_token_trusted() -> bool {
    cfg!(debug_assertions) && std::env::var_os("TWR_HOME").is_some()
}

/// Creates workspace `name` of `root`: its data directory, private like every data directory.
/// Refused to an agent ([`refuse_agent`]), from `twr ws new` and the TUI alike.
fn create_workspace(root: &Paths, name: &str) -> anyhow::Result<Paths> {
    use std::os::unix::fs::DirBuilderExt;
    refuse_agent()?;
    if name == DEFAULT_WORKSPACE {
        bail!("the {DEFAULT_WORKSPACE} workspace always exists");
    }
    let ws = root.workspace(name)?;
    check_socket_path(&ws.socket)?;
    twr_core::paths::ensure_private_dir(&root.data_dir)?;
    twr_core::paths::ensure_private_dir(&root.workspaces_dir())?;
    match std::fs::DirBuilder::new().mode(0o700).create(&ws.data_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("workspace {name} already exists")
        }
        Err(e) => return Err(e).with_context(|| format!("creating {}", ws.data_dir.display())),
    }
    Ok(ws)
}

/// `twr ws rm`: removes a named workspace's data directory once nothing in it can be lost by
/// accident: its hub is not running, it has no threads, and its trash is empty (a trash item's
/// commits stay referenced in its repository until it is purged).
async fn ws_rm(paths: &Paths, name: &str) -> anyhow::Result<()> {
    if name == DEFAULT_WORKSPACE {
        bail!("the {DEFAULT_WORKSPACE} workspace cannot be removed");
    }
    let ws = paths.root().workspace(name)?;
    if !ws.exists() {
        bail!("there is no workspace {name} (`twr ws` lists them)");
    }
    let running = || {
        anyhow!(
            "the hub of workspace {name} is running: stop it first with `twr -w {name} hub stop`"
        )
    };
    if probe_hub(&ws.socket).await? != Probe::Absent {
        return Err(running());
    }
    // The lock every hub of the workspace holds on its data directory, whatever its socket:
    // held from here to the end, so no hub opens the data while it is checked and removed.
    let _lock = match lock_workspace(&ws)? {
        Some(lock) => lock,
        None => return Err(running()),
    };
    let threads = ws_threads(&ws)?;
    if !threads.is_empty() {
        bail!(
            "workspace {name} still has {} thread{} ({}): delete them first (`twr -w {name} \
             delete <thread>...`, with its hub running), then stop its hub",
            threads.len(),
            if threads.len() == 1 { "" } else { "s" },
            threads.join(", ")
        );
    }
    let trash = ws.data_dir.join("trash");
    let items = match std::fs::read_dir(&trash) {
        Ok(entries) => entries.count(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(e).with_context(|| format!("reading {}", trash.display())),
    };
    if items > 0 {
        bail!(
            "the trash of workspace {name} holds {items} deleted thread{}, whose commits stay \
             referenced in their repositories until purged: empty it first (`twr -w {name} trash \
             empty --all`, with its hub running), then stop its hub",
            if items == 1 { "" } else { "s" }
        );
    }
    // The orchestrator's memory goes with the data: said, since it is the user's notes.
    let notes = std::fs::read_dir(ws.data_dir.join(twr_hub::MEMORY_DIR))
        .map(|d| {
            d.flatten()
                .filter(|e| {
                    let n = e.file_name();
                    let n = n.to_string_lossy();
                    n.ends_with(".md") && !n.starts_with('.')
                })
                .count()
        })
        .unwrap_or(0);
    // Moved aside first (a name no workspace can have), so a half-removed directory is never
    // taken for the workspace.
    let doomed = ws
        .workspaces_dir()
        .join(format!(".rm-{name}-{}", std::process::id()));
    std::fs::rename(&ws.data_dir, &doomed)
        .with_context(|| format!("moving {} aside", ws.data_dir.display()))?;
    std::fs::remove_dir_all(&doomed).with_context(|| format!("removing {}", doomed.display()))?;
    // A socket file a hub that died left is left too: the next hub there replaces it, and
    // removing it here could remove a new hub's.
    match notes {
        0 => outln!("removed workspace {name}"),
        n => outln!(
            "removed workspace {name}, with its orchestrator's {n} memory note{}",
            if n == 1 { "" } else { "s" }
        ),
    }
    Ok(())
}

/// The exclusive lock on workspace `ws`'s data directory (`twr.lock`, which its hub holds
/// while it runs); `None` when a hub holds it.
fn lock_workspace(ws: &Paths) -> anyhow::Result<Option<std::fs::File>> {
    let path = ws.data_dir.join(twr_hub::LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // Also `None` when the file left its path before it was locked (another `ws rm`): a hub
    // may hold the new one's.
    twr_hub::lock_opened(file, &path)
}

enum StopWorkspaceLock {
    Missing,
    Held,
    Free(std::fs::File),
}

/// A workspace removed after discovery (or during this open) has no hub. Other filesystem
/// errors still fail rather than being mistaken for absence.
fn stop_workspace_lock(ws: &Paths) -> anyhow::Result<StopWorkspaceLock> {
    match lock_workspace(ws) {
        Ok(Some(lock)) => Ok(StopWorkspaceLock::Free(lock)),
        Ok(None) => Ok(StopWorkspaceLock::Held),
        Err(e)
            if e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            Ok(StopWorkspaceLock::Missing)
        }
        Err(e) => Err(e),
    }
}

/// The names of the worker threads in the stopped workspace's log (deleted ones are gone; the
/// orchestrator does not count).
fn ws_threads(ws: &Paths) -> anyhow::Result<Vec<String>> {
    let db = ws.db_file();
    if !db.exists() {
        return Ok(Vec::new());
    }
    let store = twr_hub::store::Store::open(&db)?;
    let events = store.load_all()?;
    let registry = twr_core::reduce::Registry::from_events(&events);
    Ok(registry
        .list()
        .into_iter()
        .filter(|t| !t.is_orchestrator())
        .map(|t| t.name.clone())
        .collect())
}

/// What a probe of the hub socket found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    /// A hub answered `hub.info`.
    Answers,
    /// Nothing accepts connections (no socket, or connection refused, [`nothing_listens`]): a
    /// hub must be started.
    Absent,
    /// Something accepted the connection but did not answer in time (a hub starting up or
    /// shutting down): starting another one would only fail, so wait.
    Busy,
}

/// Probes the hub socket. A full backlog ([`backlog_full`]) is a busy hub. A socket that can't
/// be used for another reason than that nothing listens (another user's, which
/// [`Client::connect`] refuses; a path too long) is an error:
/// starting a hub there would only fail, and the error says why.
async fn probe_hub(socket: &Path) -> anyhow::Result<Probe> {
    let mut c = match Client::connect(socket).await {
        Ok(c) => c,
        Err(e) if nothing_listens(&e) => return Ok(Probe::Absent),
        Err(e) if backlog_full(&e) => return Ok(Probe::Busy),
        Err(e) => return Err(e),
    };
    Ok(
        match tokio::time::timeout(PROBE_TIMEOUT, c.call("hub.info", Value::Null)).await {
            Ok(Ok(_)) => Probe::Answers,
            // Closed on us (a hub going away) or no reply yet.
            Ok(Err(_)) | Err(_) => Probe::Busy,
        },
    )
}

/// Starts `twr hub` detached (a session of its own, without a controlling terminal: neither
/// it nor what it starts, a runtime or a job, can be stopped or hung up by the terminal `twr`
/// ran in; output to the hub log).
fn spawn_hub(socket: &Path, paths: &Paths) -> anyhow::Result<()> {
    twr_core::paths::ensure_private_dir(&paths.data_dir)?;
    let log_path = paths.hub_log();
    let log = twr_core::paths::open_private_append(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    // Not `std::env::current_exe`: the TUI starts hubs as it switches workspaces, maybe after
    // an upgrade replaced the file it runs ([`twr_core::current_exe`]).
    let exe = twr_core::current_exe().context("starting the hub")?;
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("--socket")
        .arg(socket)
        .arg("--workspace")
        .arg(paths.workspace_name())
        .arg("hub")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Started from an agent's shell, this process carries its marker; the hub never does.
        .env_remove(twr_hub::host::AGENT_ENV)
        // Nor a test's user token: a hub started for whoever runs the TUI trusts no token
        // file named in their environment.
        .env_remove(twr_core::client::TEST_USER_TOKEN_ENV);
    twr_adapters::process::new_session_std(&mut cmd);
    spawn_reaped(&mut cmd).with_context(|| format!("starting the hub ({})", exe.display()))?;
    Ok(())
}

/// Spawns `cmd` and waits for it on a detached thread, so a child that exits while this
/// process lives on (the TUI after the hub it started stopped) is reaped instead of lingering
/// as a zombie. Returns its pid.
fn spawn_reaped(cmd: &mut std::process::Command) -> std::io::Result<u32> {
    let mut child = cmd.spawn()?;
    let pid = child.id();
    // Without a thread the child merely lingers as a zombie once it exits; nothing to fail.
    let _ = std::thread::Builder::new()
        .name("twr-reap".into())
        .spawn(move || {
            let _ = child.wait();
        });
    Ok(pid)
}

async fn open_tui(
    socket: &Path,
    paths: &Paths,
    switching: Option<twr_tui::Switching>,
    tui: twr_core::config::TuiConfig,
    speech: twr_core::config::SpeechConfig,
) -> anyhow::Result<()> {
    wait_for_hub(socket, paths).await?;
    // The TUI's own speaker reads this root aloud (`[speech] local`).
    let speech = twr_tui::speaker::LocalSpeech {
        paths: paths.clone(),
        config: speech,
    };
    let agent = calling_agent();
    twr_tui::run_with(
        socket.to_path_buf(),
        switching,
        Some(tui),
        agent,
        Some(speech),
    )
    .await
}

/// The workspaces of a root, as the TUI switches between them: their hubs are started like
/// bare `twr` starts one ([`wait_for_hub`]).
struct TuiWorkspaces {
    root: Paths,
    /// The root's config: its `hub.socket` is the `default` workspace's.
    config: Config,
}

impl twr_tui::Workspaces for TuiWorkspaces {
    fn names(&self) -> anyhow::Result<Vec<String>> {
        let mut names = vec![DEFAULT_WORKSPACE.to_string()];
        names.extend(self.root.named_workspaces()?);
        Ok(names)
    }

    fn socket(&self, name: &str) -> anyhow::Result<PathBuf> {
        Ok(self.config.socket_path(&self.root.workspace(name)?))
    }

    fn open(
        &self,
        name: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<PathBuf>> + Send>> {
        let ws = self
            .root
            .workspace(name)
            .map(|p| (self.config.socket_path(&p), p));
        let name = name.to_string();
        Box::pin(async move {
            let (socket, paths) = ws?;
            if !paths.exists() {
                bail!("there is no workspace {name}");
            }
            check_socket_path(&socket)?;
            wait_for_hub(&socket, &paths).await?;
            // The TUI checks it is that workspace's hub when it connects.
            Ok(socket)
        })
    }

    fn create(&self, name: &str) -> anyhow::Result<()> {
        create_workspace(&self.root, name).map(|_| ())
    }
}

/// Makes sure a hub answers on `socket`: starts one when nothing listens, waits (without
/// starting another) while one is busy, and fails with why when the socket can't be used
/// ([`probe_hub`]: another user's, say).
async fn wait_for_hub(socket: &Path, paths: &Paths) -> anyhow::Result<()> {
    let started = tokio::time::Instant::now();
    let mut spawned: Option<tokio::time::Instant> = None;
    loop {
        let now = tokio::time::Instant::now();
        let probe = probe_hub(socket).await?;
        twr_core::trace::mark_with(|| format!("cli: hub probe: {probe:?}"));
        match probe {
            Probe::Answers => return Ok(()),
            Probe::Absent => match spawned {
                None => {
                    spawn_hub(socket, paths)?;
                    spawned = Some(now);
                }
                Some(at) if now.duration_since(at) >= HUB_START_TIMEOUT => bail!(
                    "the hub did not start within {}s; see {}",
                    HUB_START_TIMEOUT.as_secs(),
                    paths.hub_log().display()
                ),
                Some(_) => {}
            },
            Probe::Busy => {
                if now.duration_since(started) >= HUB_BUSY_TIMEOUT {
                    bail!(
                        "the hub at {} accepts connections but did not answer within {}s; see {}",
                        socket.display(),
                        HUB_BUSY_TIMEOUT.as_secs(),
                        paths.hub_log().display()
                    );
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    /// `twr info`'s box line: online with the runner's version, offline with the latest
    /// refusal, and a box that never connected.
    #[test]
    fn box_info_lines() {
        let line = |b: serde_json::Value| super::box_info_line(&b);
        assert_eq!(
            line(serde_json::json!({"name": "gpu", "online": true,
                "version": "0.2.0+dev.abc1234", "refused": null})),
            "gpu online (twr 0.2.0+dev.abc1234)"
        );
        assert_eq!(
            line(
                serde_json::json!({"name": "gpu", "online": false, "version": "0.2.0",
                "refused": {"at": "2026-10-16T09:30:12.345Z", "reason": "a bad proof"}})
            ),
            "gpu offline (twr 0.2.0); last refused 2026-10-16 09:30 UTC: a bad proof"
        );
        assert_eq!(
            line(
                serde_json::json!({"name": "mac", "online": false, "version": null,
                "refused": null})
            ),
            "mac offline"
        );
    }

    /// `thread new`'s project: a directory here becomes its path for the local box only; for a
    /// remote box what was typed goes to the hub as is.
    #[test]
    fn project_params_for_remote_boxes_stay_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("api");
        std::fs::create_dir(&sub).unwrap();
        let typed = sub.to_string_lossy().into_owned();
        let config = super::Config::from_toml_str(
            "[boxes.gpu]\ntoken = \"0123456789abcdef0123456789abcdef\"\n\
             [boxes.gpu.projects.api]\nrepo = \"/srv/api\"\n",
        )
        .unwrap();
        let abs = std::fs::canonicalize(&sub).unwrap();
        assert_eq!(
            super::project_param(&config, &typed, None).unwrap(),
            abs.to_string_lossy()
        );
        assert_eq!(
            super::project_param(&config, &typed, Some("local")).unwrap(),
            abs.to_string_lossy()
        );
        assert_eq!(
            super::project_param(&config, &typed, Some("gpu")).unwrap(),
            typed
        );
        assert_eq!(
            super::project_param(&config, "api", Some("gpu")).unwrap(),
            "api"
        );
    }

    /// `twr usage`: per runtime its label, plan and age, then each window's remaining share and
    /// reset; a note when nothing is known.
    #[test]
    fn usage_lines_read_well() {
        let now: chrono::DateTime<chrono::Utc> = "2026-10-09T12:00:00Z".parse().unwrap();
        let u = serde_json::json!({"runtimes": [
            {"runtime": "claude-code", "label": "Claude", "plan": "max", "age_secs": 40,
             "stale": false, "windows": [
                {"window": "five_hour", "label": "5h", "used_pct": 92.4,
                 "resets_at": "2026-10-09T14:30:00Z", "minutes": 300},
                {"window": "seven_day", "label": "wk", "used_pct": 44.0,
                 "resets_at": "2026-10-14T05:00:00Z", "minutes": 10080}]},
            {"runtime": "codex", "label": "GPT", "plan": null, "age_secs": 720, "stale": true,
             "windows": [{"window": "seven_day", "label": "wk", "used_pct": 42.0,
                          "resets_at": null, "minutes": 10080}]}]});
        assert_eq!(
            super::usage_lines(&u, now, &chrono::Utc),
            [
                "Claude (max), as of 40s ago",
                "  5h    8% left, resets 14:30 (in 2h)",
                "  wk   56% left, resets Wed 05:00 (in 4d)",
                "GPT, as of 12m ago",
                "  wk   58% left",
            ]
        );
        let none = super::usage_lines(&serde_json::json!({"runtimes": []}), now, &chrono::Utc);
        assert!(none[0].starts_with("no usage known yet"));
        let u = serde_json::json!({"runtimes": [{"label": "Claude", "windows": [
            {"label": "5h", "used_pct": 104.4},
            {"label": "wk", "used_pct": -3.0},
            {"label": "mo", "used_pct": 22.5}]}]});
        assert_eq!(
            super::usage_lines(&u, now, &chrono::Utc),
            [
                "Claude, as of 0s ago",
                "  5h    0% left",
                "  wk  100% left",
                "  mo   78% left"
            ]
        );
    }

    #[test]
    fn the_cli_help_page_has_every_visible_command() {
        let (name, page) = super::cli_help_page();
        assert_eq!(name, "cli");
        let lib = twr_docs::Library::with_pages(vec![(name, page)]);
        let ids: Vec<&str> = lib.ids().filter(|i| i.starts_with("cli#")).collect();
        for want in [
            "cli#twr",
            "cli#twr-thread-new",
            "cli#twr-ws-new",
            "cli#twr-orchestrator-use",
        ] {
            assert!(ids.contains(&want), "{want} missing from {ids:?}");
        }
        assert!(
            !ids.iter().any(|i| i.contains("claude-memory-guard")),
            "hidden: {ids:?}"
        );
        let new = lib.read("cli#twr-thread-new", 0).unwrap().text;
        assert!(new.contains("--runtime"), "{new}");
        assert!(
            new.contains("twr thread new"),
            "usage names the full command: {new}"
        );
        let stop = lib.read("cli#twr-hub-stop", 0).unwrap().text;
        assert!(
            stop.contains("--all") && stop.contains("per workspace"),
            "{stop}"
        );
        let row = include_str!("../../../SKILL.md")
            .lines()
            .find(|line| line.starts_with("| `twr hub stop "))
            .unwrap();
        assert!(
            row.contains("[--all]")
                && row.contains("timeout per workspace")
                && row.contains("non-zero"),
            "{row}"
        );
    }
    use super::*;

    /// A workspace that was present during discovery can be moved aside by ws rm before
    /// the stop opens its lock. Absence is not a stop failure; other open errors are.
    #[test]
    fn hub_stop_all_handles_a_workspace_removed_before_lock_open() {
        let home = tempfile::tempdir().unwrap();
        let ws = Paths::with_home(home.path()).workspace("a").unwrap();
        std::fs::create_dir_all(&ws.data_dir).unwrap();
        assert!(ws.data_dir.exists());
        std::fs::rename(&ws.data_dir, ws.workspaces_dir().join(".rm-a")).unwrap();
        assert!(matches!(
            stop_workspace_lock(&ws).unwrap(),
            StopWorkspaceLock::Missing
        ));
        std::fs::create_dir_all(ws.data_dir.join(twr_hub::LOCK_FILE)).unwrap();
        assert!(
            stop_workspace_lock(&ws).is_err(),
            "a lock path that is a directory is a failure"
        );
    }

    #[test]
    fn hub_stop_does_not_wait_for_a_stuck_blocking_task_at_runtime_drop() {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (read_done_tx, read_done_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .build()
                .unwrap();
            runtime.spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                read_done_tx.send(()).unwrap();
            });
            shutdown_cli_runtime(runtime, true);
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // The blocked read is only released after observing command completion. A normal
        // runtime drop cannot finish here, regardless of how fast or slow the machine is.
        let done = done_rx.recv_timeout(Duration::from_secs(2));
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        read_done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            done.is_ok(),
            "runtime drop waited for a timed-out blocking process check"
        );
    }

    /// A hub serves the workspace it says, or `default` when it says none.
    #[test]
    fn a_hub_serves_the_workspace_it_says() {
        assert!(hub_serves(&json!({"workspace": "a"}), "a"));
        assert!(!hub_serves(&json!({"workspace": "b"}), "a"));
        assert!(hub_serves(&json!({"client": "cli"}), "default"));
        assert!(
            !hub_serves(&json!({"client": "cli"}), "a"),
            "older than workspaces"
        );
    }

    /// `twr attention` prints `<attn_id>@<revision>`; `twr respond` takes that form (the
    /// revision is sent) or a bare id (none is sent), and an id that itself ends in
    /// `@<digits>` is taken as written.
    #[test]
    fn items_are_named_with_their_revision() {
        let items = vec![
            json!({"attn_id": "t:1:p1", "revision": 7}),
            json!({"attn_id": "t:1:x@3", "revision": 9}),
            json!({"attn_id": "old"}),
        ];
        assert_eq!(revisioned_id(&items[0]), "t:1:p1@7");
        assert_eq!(revisioned_id(&items[2]), "old");
        let found = |t: &str| find_item(&items, t).map(|(i, r)| (str_of(&i["attn_id"]), r));
        assert_eq!(found("t:1:p1@7"), Some(("t:1:p1", Some(7))));
        assert_eq!(found("t:1:p1@6"), Some(("t:1:p1", Some(6))));
        assert_eq!(found("t:1:p1"), Some(("t:1:p1", None)));
        assert_eq!(found("t:1:x@3"), Some(("t:1:x@3", None)));
        assert_eq!(found("t:1:x@3@9"), Some(("t:1:x@3", Some(9))));
        assert_eq!(found("t:1:p1@"), None);
        assert_eq!(found("t:1:p1@x"), None);
        assert_eq!(found("t:1:p2@7"), None);
    }

    /// The config's default `threads.park_disposable` is the runner's list (twr-core keeps a
    /// copy because it cannot depend on twr-runner).
    #[test]
    fn the_park_disposable_defaults_agree() {
        assert_eq!(
            twr_core::config::DEFAULT_PARK_DISPOSABLE,
            twr_runner::local::DEFAULT_PARK_DISPOSABLE
        );
    }

    /// The hub `twr` starts is reaped when it exits (no zombie while the TUI runs on).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_spawned_child_is_reaped_when_it_exits() {
        let pid = spawn_reaped(&mut std::process::Command::new("true")).unwrap();
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        // A zombie keeps its /proc entry (state Z) until it is waited for.
        while proc_dir.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "{pid} was not reaped: {:?}",
                std::fs::read_to_string(proc_dir.join("stat"))
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn wait_tells_states_apart() {
        let t = |v: Value| wait_state(&v);
        let done = json!({"status": "done", "pending": 0, "reported": true, "box_online": true,
            "last_turn": {"turn": 2, "result": "completed"}, "last_report": {"turn": 2}});
        assert_eq!(t(done.clone()), (WaitState::Done, false));
        // Its box away: what it shows may be old.
        let mut away = done.clone();
        away["box_online"] = json!(false);
        assert_eq!(t(away).0, WaitState::Busy);
        // Its report not written yet.
        let mut unreported = done.clone();
        unreported["reported"] = json!(false);
        assert_eq!(t(unreported).0, WaitState::Busy);
        // The orchestrator's turns have none.
        unreported_orch(&done);
        // Stopped, whatever is queued.
        let mut stopped = done.clone();
        stopped["stopped"] = json!(true);
        stopped["pending"] = json!(2);
        assert_eq!(t(stopped).0, WaitState::Failed);
        // A held prompt with its card open waits on the user.
        let mut carded = done.clone();
        carded["queued_after"] = json!([{"id": "x", "card": "a1"}]);
        assert_eq!(t(carded).0, WaitState::Waiting);
        let mut held = done;
        held["queued_after"] = json!([{"id": "x", "card": null}]);
        assert_eq!(t(held.clone()).0, WaitState::Busy);
        // After a failed turn too: the held prompt goes out by itself once the threads it
        // waits for finished, and releases a queue the failure held. Without one, that queue
        // goes nowhere.
        let mut failed = held;
        failed["status"] = json!("error");
        failed["last_turn"]["result"] = json!("error");
        assert_eq!(t(failed.clone()), (WaitState::Busy, false));
        failed["pending"] = json!(1);
        assert_eq!(t(failed.clone()), (WaitState::Busy, false));
        failed["queued_after"] = json!([]);
        assert_eq!(t(failed).0, WaitState::Failed);
    }

    fn unreported_orch(done: &Value) {
        let mut orch = done.clone();
        orch["reported"] = json!(false);
        orch["role"] = json!("orchestrator");
        assert_eq!(wait_state(&orch).0, WaitState::Done);
    }

    #[test]
    fn cli_parses() {
        let c = Cli::try_parse_from(["twr", "send", "api", "!perm", "Bash", "-rf"]).unwrap();
        match c.cmd.unwrap() {
            Cmd::Send { thread, prompt, .. } => {
                assert_eq!(thread.as_deref(), Some("api"));
                assert_eq!(prompt.join(" "), "!perm Bash -rf");
            }
            other => panic!("{other:?}"),
        }
        let c = Cli::try_parse_from([
            "twr",
            "send",
            "api",
            "--after",
            "web,db",
            "rebase",
            "on",
            "{report:web}",
        ])
        .unwrap();
        match c.cmd.unwrap() {
            Cmd::Send {
                thread,
                prompt,
                after,
                cancel,
            } => {
                assert_eq!(thread.as_deref(), Some("api"));
                assert_eq!(after, ["web", "db"]);
                assert_eq!(prompt.join(" "), "rebase on {report:web}");
                assert_eq!(cancel, None);
            }
            other => panic!("{other:?}"),
        }
        let c = Cli::try_parse_from(["twr", "send", "--cancel", "01J7Q"]).unwrap();
        assert!(
            matches!(c.cmd.unwrap(), Cmd::Send { cancel: Some(id), thread: None, .. } if id == "01J7Q")
        );
        assert!(Cli::try_parse_from(["twr", "send", "api"]).is_err());
        let c =
            Cli::try_parse_from(["twr", "wait", "web", "db", "--all", "--timeout", "30m"]).unwrap();
        match c.cmd.unwrap() {
            Cmd::Wait {
                threads,
                all,
                timeout,
                json,
            } => {
                assert_eq!(threads, ["web", "db"]);
                assert!(all && !json);
                assert_eq!(timeout, Some(1800));
            }
            other => panic!("{other:?}"),
        }
        let c = Cli::try_parse_from([
            "twr",
            "thread",
            "new",
            ".",
            "--runtime",
            "fake",
            "--alias",
            "a",
            "--alias",
            "b",
            "--box",
            "local",
        ])
        .unwrap();
        match c.cmd.unwrap() {
            Cmd::Thread {
                cmd: ThreadCmd::New {
                    aliases, box_name, ..
                },
            } => {
                assert_eq!(aliases, vec!["a", "b"]);
                assert_eq!(box_name.as_deref(), Some("local"));
            }
            other => panic!("{other:?}"),
        }
        let c = Cli::try_parse_from(["twr", "hub", "--socket", "/tmp/s"]).unwrap();
        assert_eq!(c.socket, Some(PathBuf::from("/tmp/s")));
        assert!(Cli::try_parse_from(["twr"]).unwrap().cmd.is_none());
        assert!(Cli::try_parse_from(["twr", "send", "x"]).is_err());
    }

    #[test]
    fn the_first_attach_to_a_worktree_mentions_claudes_trust_question() {
        let hint = attach_hint(&json!({"runtime": "claude-code", "trust_prompt": true})).unwrap();
        assert!(
            hint.contains("trust the folder") && hint.contains("Yes"),
            "{hint}"
        );
        assert_eq!(attach_hint(&json!({"runtime": "claude-code"})), None);
    }

    /// Attaching a Claude Code thread on a Mac box warns about the keychain over ssh; another
    /// runtime, another OS, a token box (no OS known), a command box (attach refused there, no
    /// ssh) or the hub's own box does not.
    #[test]
    fn attaching_claude_on_a_mac_warns_about_the_keychain() {
        let info = json!({"boxes": [
            {"name": "mac", "transport": "ssh", "os": "Darwin"},
            {"name": "gpu", "transport": "ssh", "os": "Linux"},
            {"name": "lab", "transport": "token", "os": null},
            {"name": "sb", "transport": "command", "os": "Darwin"},
        ]});
        let thread = |runtime: &str, b: &str| json!({"runtime": runtime, "box_name": b});
        let w = attach_mac_warning(&thread("claude-code", "mac"), &info).unwrap();
        assert!(
            w.contains("box mac is a Mac")
                && w.contains("keychain login")
                && w.contains("continue the thread from the TUI"),
            "{w}"
        );
        for (runtime, b) in [
            ("codex", "mac"),
            ("claude-code", "gpu"),
            ("claude-code", "lab"),
            ("claude-code", "sb"),
            ("claude-code", "local"),
            ("claude-code", "gone"),
        ] {
            assert_eq!(
                attach_mac_warning(&thread(runtime, b), &info),
                None,
                "{runtime} on {b}"
            );
        }
    }

    #[test]
    fn say_and_mcp_parse_and_say_outcomes_read_well() {
        let c = Cli::try_parse_from(["twr", "say", "on", "api,", "-v", "fix"]).unwrap();
        assert!(
            matches!(c.cmd, Some(Cmd::Say { ref message }) if message.join(" ") == "on api, -v fix")
        );
        let c = Cli::try_parse_from(["twr", "--socket", "/tmp/s", "mcp"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Mcp)));
        assert_eq!(
            say_outcome(&json!({"action": "send", "thread_name": "api", "turn": 3})),
            "sent to api (turn 3 started)"
        );
        assert_eq!(
            say_outcome(
                &json!({"action": "delegate", "thread_name": "orchestrator", "turn": null})
            ),
            "handed to the orchestrator (queued)"
        );
        assert_eq!(
            say_outcome(
                &json!({"action": "respond", "thread_name": "api", "attn_id": "a1", "answer": "allow"})
            ),
            "answered api's a1: allow"
        );
        assert_eq!(
            say_outcome(
                &json!({"action": "ask", "attn_id": "q1", "options": ["api", "new_thread"]})
            ),
            "which thread should get it? answer with `twr respond q1 <n>`: 1 api, 2 new_thread"
        );
        assert_eq!(
            say_outcome(&json!({"action":"command", "applied":["api: allow", "web → plan mode"]})),
            "applied: api: allow; web → plan mode"
        );
        let pending = json!({"action": "pending", "items": [{
            "attn_id": "t1:1:p1", "revision": 42, "thread_name": "api", "reason": "permission",
            "summary": "Run rm -rf build?", "options": ["allow", "deny"],
        }]});
        assert_eq!(
            say_outcome(&pending),
            "not sent: it looks like an answer, and `twr say` never answers a pending item\n\
             a permission is pending on api: Run rm -rf build? — answer it with \
             `twr respond t1:1:p1@42 <option>` (options: allow, deny)"
        );
    }

    /// `twr respond <thread>` answers the top item only with the user's consent: --yes, or a
    /// "y" at the terminal prompt naming that exact revision; with no terminal it refuses
    /// and names the `<attn_id>@<revision>` to use instead.
    #[test]
    fn top_item_answers_need_consent() {
        let item = json!({
            "attn_id": "t1:1:p1", "revision": 42, "thread_name": "api",
            "reason": "permission", "summary": "rm -rf build?", "options": ["allow", "deny"],
        });
        consent_to_top_item(&item, "allow", Consent::Yes).unwrap();

        let e = consent_to_top_item(&item, "allow", Consent::NoTerminal).unwrap_err();
        let e = e.to_string();
        assert!(e.contains("unseen"), "{e}");
        assert!(e.contains("`twr respond t1:1:p1@42 allow`"), "{e}");
        let e = consent_to_top_item(&item, "use sqlite", Consent::NoTerminal)
            .unwrap_err()
            .to_string();
        assert!(e.contains("`twr respond t1:1:p1@42 'use sqlite'`"), "{e}");

        for (reply, ok) in [
            ("y\n", true),
            ("Yes\n", true),
            ("n\n", false),
            ("\n", false),
            ("", false),
            ("yep\n", false),
        ] {
            let mut asked = Vec::new();
            let mut ask = |q: &str| -> anyhow::Result<String> {
                asked.push(q.to_string());
                Ok(reply.to_string())
            };
            let r = consent_to_top_item(&item, "deny", Consent::Ask(&mut ask));
            assert_eq!(r.is_ok(), ok, "{reply:?}");
            assert_eq!(asked, ["answer t1:1:p1@42 with deny? [y/N] "]);
        }
        assert_eq!(sh_word("it's"), r"'it'\''s'");
    }

    /// `--socket` (global) is refused with `twr runner`, which ignores it.
    #[test]
    fn a_runner_refuses_the_socket_flag() {
        let runner = ["runner", "--hub", "ws://h:1", "--box", "b"];
        for args in [
            [&["twr", "--socket", "/tmp/s"][..], &runner[..]].concat(),
            [&["twr"][..], &runner[..], &["--socket", "/tmp/s"][..]].concat(),
        ] {
            let c = Cli::try_parse_from(&args).unwrap();
            let e = refuse_runner_socket(&c).unwrap_err().to_string();
            assert!(e.contains("--socket does not apply to `twr runner`"), "{e}");
        }
        let c = Cli::try_parse_from([&["twr"][..], &runner[..]].concat()).unwrap();
        assert!(refuse_runner_socket(&c).is_ok());
        let c = Cli::try_parse_from(["twr", "--socket", "/tmp/s", "attention"]).unwrap();
        assert!(refuse_runner_socket(&c).is_ok());
    }

    /// `twr runner token` needs neither --hub nor --box, and takes no runner flag; `twr
    /// runner` without a subcommand needs both.
    #[test]
    fn the_runner_token_subcommand_parses() {
        let c = Cli::try_parse_from(["twr", "runner", "token"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                cmd: Some(RunnerCmd::Token),
                hub: None,
                box_name: None,
                ..
            })
        ));
        assert!(Cli::try_parse_from(["twr", "runner", "--hub", "ws://h:1", "token"]).is_err());
        assert!(Cli::try_parse_from(["twr", "runner", "token", "--box", "b"]).is_err());
        assert!(Cli::try_parse_from(["twr", "runner", "--hub", "ws://h:1"]).is_err());
        assert!(Cli::try_parse_from(["twr", "runner", "--box", "b"]).is_err());
        let c = Cli::try_parse_from([
            "twr",
            "runner",
            "--hub",
            "ws://h:1",
            "--box",
            "b",
            "--allow-insecure",
        ])
        .unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                cmd: None,
                hub: Some(_),
                box_name: Some(_),
                allow_insecure: true,
                ..
            })
        ));
    }

    /// `twr runner install-token <box>` takes its own --hub, --ca, --token-file and --yes,
    /// and none of the runner's other flags.
    #[test]
    fn the_install_token_subcommand_parses() {
        let c = Cli::try_parse_from([
            "twr",
            "runner",
            "install-token",
            "gpu",
            "--hub",
            "wss://hub:7878",
            "--ca",
            "ca.pem",
            "--token-file",
            "t",
            "--yes",
        ])
        .unwrap();
        let Some(Cmd::Runner {
            cmd:
                Some(RunnerCmd::InstallToken {
                    box_name,
                    hub,
                    ca,
                    token_file,
                    yes: true,
                }),
            hub: None,
            box_name: None,
            ..
        }) = c.cmd
        else {
            panic!("{:?}", c.cmd)
        };
        assert_eq!(box_name, "gpu");
        assert_eq!(hub.as_deref(), Some("wss://hub:7878"));
        assert_eq!(ca, Some(PathBuf::from("ca.pem")));
        assert_eq!(token_file, Some(PathBuf::from("t")));
        let c = Cli::try_parse_from(["twr", "runner", "install-token", "gpu"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                cmd: Some(RunnerCmd::InstallToken {
                    hub: None,
                    yes: false,
                    ..
                }),
                ..
            })
        ));
        assert!(Cli::try_parse_from(["twr", "runner", "install-token"]).is_err());
        for extra in [
            &["--allow-insecure"][..],
            &["--box", "b"],
            &["--data-dir", "d"],
        ] {
            let args = [&["twr", "runner", "install-token", "gpu"][..], extra].concat();
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        assert!(Cli::try_parse_from([
            "twr",
            "runner",
            "--hub",
            "ws://h:1",
            "install-token",
            "gpu"
        ])
        .is_err());
    }

    /// The SSH box's relay (`--stdio`) and daemon (`--daemon`) take a box and a hub key, none
    /// of the token runner's flags, and not each other; the daemon every path. Hidden from
    /// help.
    #[test]
    fn the_box_relay_and_daemon_flags_parse() {
        let key = "ab".repeat(32);
        let parse = |extra: &[&str]| {
            let args: Vec<&str> = ["twr", "runner"].iter().chain(extra).copied().collect();
            Cli::try_parse_from(args)
        };
        let relay = ["--stdio", "--box", "b", "--hub-key", key.as_str()];
        let c = parse(&[&relay[..], &["--build=0123456789abcdef"]].concat()).unwrap();
        assert!(
            matches!(&c.cmd, Some(Cmd::Runner { build: Some(b), .. }) if b == "0123456789abcdef")
        );
        let c = parse(&relay).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                stdio: true,
                daemon: false,
                replace: false,
                hub: None,
                ..
            })
        ));
        let c = parse(&[&relay[..], &["--replace"]].concat()).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Runner { replace: true, .. })));
        let c = parse(&[&relay[..], &["--stop"]].concat()).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                stop: true,
                replace: false,
                restart: false,
                ..
            })
        ));
        for extra in [&["--restart"][..], &["--restart", "--replace"]] {
            let c = parse(&[&relay[..], extra].concat()).unwrap();
            assert!(matches!(
                c.cmd,
                Some(Cmd::Runner {
                    restart: true,
                    stop: false,
                    ..
                })
            ));
        }
        let daemon = [
            "--daemon",
            "--box",
            "b",
            "--hub-key",
            key.as_str(),
            "--daemon-socket",
            "/s/x.sock",
            "--data-dir",
            "/d",
            "--lock",
            "/l",
            "--log",
            "/d/daemon.log",
        ];
        let c = parse(&daemon).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                daemon: true,
                daemon_socket: Some(_),
                lock: Some(_),
                log: Some(_),
                ..
            })
        ));
        // The names the hub passes joined to their flag (`--box=<name>`), as the relay and the
        // daemon get them.
        for args in [
            vec!["--stdio", "--box=b", "--hub-key", key.as_str()],
            [&["--daemon", "--box=b"][..], &daemon[3..]].concat(),
        ] {
            let c = parse(&args).unwrap();
            assert!(
                matches!(&c.cmd, Some(Cmd::Runner { box_name: Some(b), .. }) if b == "b"),
                "{args:?}"
            );
        }
        // What each refuses.
        for bad in [
            vec!["--stdio", "--box", "b"],
            vec!["--stdio", "--hub-key", key.as_str()],
            [&relay[..], &["--daemon"]].concat(),
            [&relay[..], &["--hub", "ws://h:1"]].concat(),
            [&relay[..], &["--token-file", "/t"]].concat(),
            [&relay[..], &["--ca", "/c"]].concat(),
            [&relay[..], &["--allow-insecure"]].concat(),
            [&relay[..], &["--lock", "/l"]].concat(),
            [&daemon[..], &["--replace"]].concat(),
            [&daemon[..], &["--stop"]].concat(),
            [&daemon[..], &["--restart"]].concat(),
            [&relay[..], &["--stop", "--replace"]].concat(),
            [&relay[..], &["--stop", "--restart"]].concat(),
            vec!["--hub", "ws://h:1", "--box", "b", "--restart"],
            vec!["--hub", "ws://h:1", "--box", "b", "--stop"],
            [&daemon[..], &["--hub", "ws://h:1"]].concat(),
            daemon[..daemon.len() - 2].to_vec(),
            daemon[..daemon.len() - 4].to_vec(),
            vec!["--hub", "ws://h:1", "--box", "b", "--replace"],
            vec!["--hub", "ws://h:1", "--box", "b", "--daemon-socket", "/s"],
            vec!["--box", "b", "--hub-key", key.as_str()],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        // Hidden: the help shows none of them (as whole flags: `twr account remove --logout`
        // is not `--log`).
        let help = cli_help_page().1;
        let shows = |flag: &str| {
            help.match_indices(flag).any(|(i, _)| {
                !help[i + flag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-')
            })
        };
        for flag in [
            "--stdio",
            "--daemon",
            "--hub-key",
            "--replace",
            "--stop",
            "--restart",
            "--lock",
            "--log",
        ] {
            assert!(!shows(flag), "{flag}");
        }
    }

    /// The hub probes its own agent CLIs unless it is a test's, or `TWR_AGENT_PROBE=off`;
    /// any other value leaves it on.
    #[test]
    fn the_hubs_own_agent_probe_can_be_turned_off() {
        use std::ffi::OsStr;
        assert!(probes_own_agents(false, None));
        assert!(probes_own_agents(false, Some(OsStr::new("on"))));
        assert!(probes_own_agents(false, Some(OsStr::new("OFF"))));
        assert!(probes_own_agents(false, Some(OsStr::new(""))));
        assert!(!probes_own_agents(false, Some(OsStr::new("off"))));
        assert!(!probes_own_agents(true, None));
    }

    /// The box-side probes of `twr box doctor` / `twr box login` / `twr box project`:
    /// `--agents`, `--login <agent>`, `--find-repo` and `--clone-repo` take no other runner
    /// flag and not each other; `--status` goes with the relay's box and hub key only. Hidden
    /// from help.
    #[test]
    fn the_box_probe_flags_parse() {
        let key = "ab".repeat(32);
        let parse = |extra: &[&str]| {
            let args: Vec<&str> = ["twr", "runner"].iter().chain(extra).copied().collect();
            Cli::try_parse_from(args)
        };
        let c = parse(&["--agents"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                agents: true,
                hub: None,
                box_name: None,
                login: None,
                ..
            })
        ));
        for agent in ["claude", "claude-code", "codex", "grok", "grok-build"] {
            let c = parse(&["--login", agent]).unwrap();
            assert!(
                matches!(&c.cmd, Some(Cmd::Runner { login: Some(a), agents: false, .. }) if a == agent),
                "{agent}"
            );
        }
        let relay = ["--stdio", "--box=b", "--hub-key", key.as_str()];
        let c = parse(&[&relay[..], &["--status"]].concat()).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Runner {
                stdio: true,
                status: true,
                stop: false,
                ..
            })
        ));
        for bad in [
            vec!["--login", "acp"],
            vec!["--login"],
            vec!["--agents", "--login", "codex"],
            vec!["--agents", "--box", "b"],
            vec!["--agents", "--hub", "ws://h:1"],
            vec!["--agents", "--hub", "ws://h:1", "--box", "b"],
            vec!["--agents", "--data-dir", "/d"],
            vec!["--agents", "--token-file", "/t"],
            vec!["--agents", "--hub-key", key.as_str()],
            [&relay[..], &["--agents"]].concat(),
            vec!["--login", "claude", "--box", "b"],
            vec!["--login", "claude", "--hub", "ws://h:1", "--box", "b"],
            vec!["--login", "claude", "--allow-insecure"],
            [&relay[..], &["--login", "codex"]].concat(),
            vec!["--login", "claude", "--daemon"],
            vec!["--status"],
            vec!["--status", "--box", "b", "--hub-key", key.as_str()],
            vec!["--hub", "ws://h:1", "--box", "b", "--status"],
            [&relay[..], &["--status", "--stop"]].concat(),
            [&relay[..], &["--status", "--replace"]].concat(),
            [&relay[..], &["--status", "--restart"]].concat(),
            [&relay[..], &["--status", "--build=0123456789abcdef"]].concat(),
            vec!["--agents", "token"],
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        // `--exec` takes a built-in runtime, `--path` and the arguments after `--`, and no
        // other runner flag.
        let c = parse(&[
            "--exec",
            "claude-code",
            "--path=bin:/usr/bin",
            "--",
            "--resume",
            "s",
        ])
        .unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner { exec: Some(r), exec_path: Some(p), exec_args, box_name: None, .. })
                if r == "claude-code" && p == "bin:/usr/bin" && exec_args == &["--resume", "s"]
        ));
        let c = parse(&["--exec=codex", "--", "resume", "--model", "x"]).unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner { exec: Some(r), exec_path: None, exec_args, exec_env, .. })
                if r == "codex" && exec_args == &["resume", "--model", "x"] && exec_env.is_empty()
        ));
        // The thread's other variables, each in an argument of its own.
        let c = parse(&[
            "--exec=codex",
            "--env=HOME=/x y",
            "--env=A=b=c",
            "--unset=CLAUDE_CODE_SIMPLE",
            "--",
            "resume",
        ])
        .unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner { exec_env, exec_unset, exec_args, .. })
                if exec_env == &["HOME=/x y", "A=b=c"]
                    && exec_unset == &["CLAUDE_CODE_SIMPLE"]
                    && exec_args == &["resume"]
        ));
        for bad in [
            vec!["--exec", "acp", "--", "x"],
            vec!["--exec", "codex", "--box", "b"],
            vec!["--exec", "codex", "--agents"],
            vec!["--exec", "codex", "--login", "codex"],
            vec!["--exec", "codex", "--hub", "ws://h:1"],
            [&relay[..], &["--exec", "codex"]].concat(),
            vec!["--path", "/x"],
            vec!["--env=A=b"],
            vec!["--unset=A"],
            vec!["--hub", "ws://h:1", "--box", "b", "--env=A=b"],
            vec!["--hub", "ws://h:1", "--box", "b", "--", "x"],
            vec!["--hub", "ws://h:1", "--box", "b", "--path", "/x"],
            vec!["--agents", "--", "x"],
            vec!["--login", "codex", "--path", "/x"],
            [&relay[..], &["--", "x"]].concat(),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        // `--find-repo` and `--clone-repo` take their own operands and no other runner flag.
        let c = parse(&[
            "--find-repo",
            "--name=api",
            "--origin=git@github.com:o/api.git",
            "--hint=/home/me/code/api",
        ])
        .unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner { find_repo: true, clone_repo: false, repo_name: Some(n), origin: Some(o), hint: Some(h), box_name: None, hub: None, .. })
                if n == "api" && o == "git@github.com:o/api.git" && h == Path::new("/home/me/code/api")
        ));
        let c = parse(&["--find-repo", "--name", "api"]).unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner {
                find_repo: true,
                origin: None,
                hint: None,
                ..
            })
        ));
        let c = parse(&[
            "--clone-repo",
            "--url",
            "https://h/o/api",
            "--parent",
            "/home/me/code",
            "--name",
            "api",
        ])
        .unwrap();
        assert!(matches!(
            &c.cmd,
            Some(Cmd::Runner { clone_repo: true, find_repo: false, clone_url: Some(u), clone_parent: Some(p), repo_name: Some(n), .. })
                if u == "https://h/o/api" && p == Path::new("/home/me/code") && n == "api"
        ));
        let clone = ["--clone-repo", "--url=u", "--parent=/p", "--name=n"];
        for bad in [
            vec!["--find-repo"],
            vec!["--name", "api"],
            vec!["--find-repo", "--name=a", "--url=u"],
            vec!["--find-repo", "--name=a", "--parent=/p"],
            vec!["--find-repo", "--name=a", "--box", "b"],
            vec!["--find-repo", "--name=a", "--hub", "ws://h:1"],
            vec!["--find-repo", "--name=a", "--agents"],
            vec!["--find-repo", "--name=a", "--exec=codex"],
            vec!["--origin=u"],
            vec!["--hint=/p"],
            vec!["--hub", "ws://h:1", "--box", "b", "--name=a"],
            vec!["--clone-repo", "--url=u", "--name=n"],
            vec!["--clone-repo", "--parent=/p", "--name=n"],
            vec!["--clone-repo", "--url=u", "--parent=/p"],
            vec!["--url=u", "--parent=/p", "--name=n"],
            [&clone[..], &["--find-repo"]].concat(),
            [&clone[..], &["--origin=o"]].concat(),
            [&clone[..], &["--login", "codex"]].concat(),
            [&clone[..], &["--box", "b"]].concat(),
            [&relay[..], &["--find-repo", "--name=a"]].concat(),
        ] {
            assert!(parse(&bad).is_err(), "{bad:?}");
        }
        let help = cli_help_page().1;
        for flag in [
            "--agents",
            "--login",
            "--status",
            "--exec",
            "--find-repo",
            "--clone-repo",
            "--hint",
            "--origin",
        ] {
            assert!(!help.contains(flag), "{flag}");
        }
    }

    #[test]
    fn new_subcommands_parse() {
        let c = Cli::try_parse_from(["twr", "attach", "api", "--force"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Attach { ref thread, force: true }) if thread == "api"));
        let c = Cli::try_parse_from(["twr", "respond", "api", "2", "-m", "why"]).unwrap();
        match c.cmd.unwrap() {
            Cmd::Respond {
                target,
                answer,
                message,
                yes,
            } => assert_eq!(
                (target.as_str(), answer.as_str(), message.as_deref(), yes),
                ("api", "2", Some("why"), false)
            ),
            other => panic!("{other:?}"),
        }
        for flag in ["-y", "--yes"] {
            let c = Cli::try_parse_from(["twr", "respond", "api", "2", flag]).unwrap();
            assert!(
                matches!(c.cmd, Some(Cmd::Respond { yes: true, .. })),
                "{flag}"
            );
        }
        assert!(matches!(
            Cli::try_parse_from(["twr", "attention", "--all"])
                .unwrap()
                .cmd,
            Some(Cmd::Attention {
                all: true,
                cmd: None
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["twr", "attention", "dismiss", "q1@7"])
                .unwrap()
                .cmd,
            Some(Cmd::Attention {
                cmd: Some(AttentionCmd::Dismiss { ref target }),
                ..
            }) if target == "q1@7"
        ));
        let c = Cli::try_parse_from(["twr", "thread", "new", ".", "--model", "opus"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Thread { cmd: ThreadCmd::New { ref model, .. } }) if model.as_deref() == Some("opus")
        ));
        let c = Cli::try_parse_from(["twr", "thread", "new", ".", "--effort", "high"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Thread { cmd: ThreadCmd::New { ref effort, .. } }) if effort.as_deref() == Some("high")
        ));
        let c = Cli::try_parse_from([
            "twr",
            "model",
            "web",
            "claude-fable-5-1",
            "--effort",
            "high",
        ])
        .unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Model { ref thread, ref model, ref effort, runtime: None, account: None,
                outside_pin: false })
                if thread == "web" && model.as_deref() == Some("claude-fable-5-1")
                    && effort.as_deref() == Some("high")
        ));
        let c = Cli::try_parse_from(["twr", "model", "web"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Model {
                model: None,
                effort: None,
                ..
            })
        ));
        // `--outside-pin` with either account syntax; with neither, refused after parsing.
        for args in [
            &[
                "twr",
                "thread",
                "new",
                ".",
                "--runtime",
                "claude-code:personal",
                "--outside-pin",
            ][..],
            &[
                "twr",
                "thread",
                "new",
                ".",
                "--account",
                "personal",
                "--outside-pin",
            ],
            &["twr", "thread", "new", ".", "--outside-pin"],
            &[
                "twr",
                "thread",
                "new",
                ".",
                "--runtime",
                "acp:x:y",
                "--outside-pin",
            ],
            &[
                "twr",
                "model",
                "web",
                "--runtime",
                "codex:team",
                "--outside-pin",
            ],
            &["twr", "model", "web", "--account", "team", "--outside-pin"],
            &["twr", "model", "web", "--outside-pin"],
            &["twr", "model", "web", "--runtime", "codex", "--outside-pin"],
        ] {
            let c = Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            let (account, runtime, outside_pin) = match c.cmd {
                Some(Cmd::Thread {
                    cmd:
                        ThreadCmd::New {
                            account,
                            runtime,
                            outside_pin,
                            ..
                        },
                })
                | Some(Cmd::Model {
                    account,
                    runtime,
                    outside_pin,
                    ..
                }) => (account, runtime, outside_pin),
                other => panic!("{args:?}: {other:?}"),
            };
            assert!(outside_pin, "{args:?}");
            let named = args.contains(&"--account")
                || args
                    .iter()
                    .any(|a| *a == "claude-code:personal" || *a == "codex:team");
            let checked = check_outside_pin(outside_pin, account.as_deref(), runtime.as_deref());
            match named {
                true => checked.unwrap_or_else(|e| panic!("{args:?}: {e}")),
                false => assert!(
                    checked
                        .unwrap_err()
                        .to_string()
                        .starts_with("--outside-pin needs the account it is for"),
                    "{args:?}"
                ),
            }
        }
        check_outside_pin(false, None, None).unwrap();
        assert!(Cli::try_parse_from(["twr", "respond", "api"]).is_err());
        let c = Cli::try_parse_from(["twr", "park", "api", "--keep", "--force"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Park { ref thread, keep: true, force: true }) if thread == "api"
        ));
        let c = Cli::try_parse_from(["twr", "park", "api"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Park {
                keep: false,
                force: false,
                ..
            })
        ));
        let c = Cli::try_parse_from(["twr", "resume", "api"]).unwrap();
        assert!(matches!(c.cmd, Some(Cmd::Resume { ref thread }) if thread == "api"));
        let c = Cli::try_parse_from(["twr", "compact", "api"]).unwrap();
        assert!(
            matches!(c.cmd, Some(Cmd::Compact { ref thread, offers: None }) if thread == "api")
        );
        let c = Cli::try_parse_from(["twr", "compact", "api", "--offers", "off"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Compact { ref thread, offers: Some(ref o) }) if thread == "api" && o == "off"
        ));
        assert!(Cli::try_parse_from(["twr", "compact", "api", "--offers", "maybe"]).is_err());
        assert_eq!(
            compact_outcome("api", &json!({"thread_name": "api", "offers": "on"})),
            "api: offers to compact its context once its prompt cache expired are on"
        );
        assert_eq!(
            compact_outcome(
                "api",
                &json!({"thread_name": "api", "state": "started", "turn": 4})
            ),
            "api: compacting its context (turn 4)"
        );
        let c = Cli::try_parse_from(["twr", "thread", "new", ".", "--base", "v1.2"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Thread { cmd: ThreadCmd::New { ref base, no_worktree: false, .. } })
                if base.as_deref() == Some("v1.2")
        ));
        let c = Cli::try_parse_from(["twr", "thread", "new", ".", "--no-worktree"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Thread {
                cmd: ThreadCmd::New {
                    no_worktree: true,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from(["twr", "thread", "new", ".", "--no-worktree", "--base", "x"])
                .is_err()
        );
        let c = Cli::try_parse_from(["twr", "thread", "new", ".", "--worktree"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Thread {
                cmd: ThreadCmd::New {
                    worktree: true,
                    no_worktree: false,
                    ..
                }
            })
        ));
        assert!(
            Cli::try_parse_from(["twr", "thread", "new", ".", "--worktree", "--no-worktree"])
                .is_err()
        );
    }

    #[test]
    fn orchestrator_commands_parse_and_read_well() {
        assert!(matches!(
            Cli::try_parse_from(["twr", "orchestrator", "reset"])
                .unwrap()
                .cmd,
            Some(Cmd::Orchestrator {
                cmd: OrchestratorCmd::Reset
            })
        ));
        assert!(Cli::try_parse_from(["twr", "orchestrator", "reset", "codex"]).is_err());
        let c =
            Cli::try_parse_from(["twr", "orchestrator", "use", "codex", "--model", "o5"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Orchestrator {
                cmd: OrchestratorCmd::Use { ref runtime, ref model, no_summary: false, wait: None }
            }) if runtime == "codex" && model.as_deref() == Some("o5")
        ));
        let c =
            Cli::try_parse_from(["twr", "orchestrator", "use", "codex", "--no-summary"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Orchestrator {
                cmd: OrchestratorCmd::Use {
                    no_summary: true,
                    wait: None,
                    ..
                }
            })
        ));
        let c =
            Cli::try_parse_from(["twr", "orchestrator", "use", "codex", "--wait", "5"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Orchestrator {
                cmd: OrchestratorCmd::Use {
                    no_summary: false,
                    wait: Some(5),
                    ..
                }
            })
        ));
        assert!(Cli::try_parse_from([
            "twr",
            "orchestrator",
            "use",
            "codex",
            "--wait",
            "5",
            "--no-summary"
        ])
        .is_err());
        let c = Cli::try_parse_from(["twr", "orchestrator", "show", "--json"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Orchestrator {
                cmd: OrchestratorCmd::Show { json: true }
            })
        ));
        assert!(Cli::try_parse_from(["twr", "orchestrator", "use"]).is_err());

        let switched = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "codex", "model": "o5",
            "started": true,
            "previous": {"thread": "01OLD", "runtime": "claude-code", "model": null,
                         "stopping": true, "dropped_prompts": 2},
        }));
        assert!(
            switched.starts_with(
                "orchestrator is now codex model o5: thread orchestrator (01NEW), started\n"
            ),
            "{switched}"
        );
        assert!(
            switched.contains("claude-code (the runtime's default model), 01OLD"),
            "{switched}"
        );
        assert!(
            switched.contains("was retired and is being stopped (2 queued prompt(s) dropped)."),
            "{switched}"
        );
        assert!(switched.contains("No handoff"), "{switched}");
        let handed = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "codex", "model": null,
            "started": true,
            "previous": {"thread": "01OLD", "runtime": "claude-code", "model": "haiku",
                         "stopping": true, "dropped_prompts": 0},
            "handoff": {"turn": 1, "bytes": 900, "summary": true, "summary_note": null,
                        "exchanges": 3, "dropped_exchanges": 2},
        }));
        assert!(
            handed.contains(
                "the previous orchestrator's summary, 3 recent exchange(s) (2 older dropped)"
            ),
            "{handed}"
        );
        let skipped = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "codex", "model": null,
            "started": true,
            "previous": {"thread": "01OLD", "runtime": "claude-code", "model": null,
                         "stopping": false, "dropped_prompts": 0},
            "handoff": {"turn": 1, "bytes": 900, "summary": false,
                        "summary_note": "the previous orchestrator was not running",
                        "exchanges": 0, "dropped_exchanges": 0},
        }));
        assert!(
            skipped.contains("no summary (the previous orchestrator was not running)"),
            "{skipped}"
        );
        assert!(
            skipped.contains(", 01OLD) was stopped and retired."),
            "{skipped}"
        );
        let first = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "grok-build", "model": null,
            "started": false, "previous": null,
        }));
        assert!(
            first.contains("starts with the next message you say"),
            "{first}"
        );
        assert!(first.contains("there was no orchestrator yet"), "{first}");

        let shown = orchestrator_text(&json!({
            "available": true, "runtime": "codex", "model": "o5", "thread": "01NEW",
            "name": "orchestrator", "status": "idle", "live": true,
            "configured": {"runtime": "claude-code", "model": "haiku"},
            "retired": 1, "runtimes": ["claude-code", "codex", "grok-build"],
        }));
        assert!(
            shown.starts_with("orchestrator: codex model o5\n"),
            "{shown}"
        );
        assert!(
            shown.contains("orchestrator (01NEW) idle, running"),
            "{shown}"
        );
        assert!(
            shown.contains("configured:   claude-code model haiku"),
            "{shown}"
        );
        assert!(
            shown.contains("can run on:   claude-code, codex, grok-build"),
            "{shown}"
        );
        assert!(shown.contains("retired:      1 earlier"), "{shown}");
    }

    /// The orchestrator's runtime is labelled with its account only while `account_shown`:
    /// a configured `claude-code:work` (an older hub's `runtime`) is the runtime alone
    /// otherwise; the configured line writes the account apart.
    #[test]
    fn the_orchestrators_account_is_labelled_only_while_shown() {
        let o = |runtime: &str, shown: bool| {
            orchestrator_text(&json!({
                "available": true, "runtime": runtime, "model": null, "account": "work",
                "account_shown": shown, "thread": null,
                "configured": {"runtime": "claude-code:work", "model": null, "account": "work"},
                "runtimes": [],
            }))
        };
        for runtime in ["claude-code", "claude-code:work"] {
            let hidden = o(runtime, false);
            assert!(
                hidden.starts_with(
                    "orchestrator: claude-code (the runtime's default model)
"
                ),
                "{hidden}"
            );
            assert!(
                hidden.contains(
                    "configured:   claude-code (the runtime's default model), account work"
                ),
                "{hidden}"
            );
            let shown = o(runtime, true);
            assert!(
                shown.starts_with(
                    "orchestrator: claude-code:work (the runtime's default model)
"
                ),
                "{shown}"
            );
        }
        // A removed account always shows, whatever `account_shown` says.
        for shown in [false, true] {
            let removed = orchestrator_text(&json!({
                "available": true, "runtime": "claude-code", "model": null, "account": "work",
                "account_shown": shown, "account_removed": true, "thread": null,
                "configured": {"runtime": "claude-code", "model": null}, "runtimes": [],
            }));
            assert!(
                removed.starts_with(
                    "orchestrator: claude-code:work (removed) (the runtime's default model)\n"
                ),
                "{removed}"
            );
        }
        // `twr info`'s and `twr orchestrator use`'s runtime: the same label.
        let o = |extra: Value| {
            let mut o = json!({"runtime": "claude-code", "account": "work"});
            o.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            orchestrator_runtime(&o)
        };
        assert_eq!(o(json!({})), "claude-code");
        assert_eq!(o(json!({"account_shown": false})), "claude-code");
        assert_eq!(o(json!({"account_shown": true})), "claude-code:work");
        assert_eq!(
            o(json!({"account_shown": false, "account_removed": true})),
            "claude-code:work (removed)"
        );
        assert_eq!(
            orchestrator_runtime(&json!({"runtime": "claude-code:work", "account_shown": false})),
            "claude-code"
        );
        let switched = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "claude-code", "model": null,
            "account": "work", "account_shown": true, "started": false, "previous": null,
        }));
        assert!(
            switched.starts_with("orchestrator is now claude-code:work (the runtime's default"),
            "{switched}"
        );
    }

    /// The orchestrator lines and `thread list` say the model the runtime reports next to the
    /// requested one.
    #[test]
    fn the_model_a_runtime_reports_is_shown() {
        let m = |model: Value, actual: Value| models_text(&model, &actual);
        assert_eq!(m(json!("o5"), Value::Null), " model o5");
        assert_eq!(
            m(Value::Null, Value::Null),
            " (the runtime's default model)"
        );
        assert_eq!(m(json!("o5"), json!("o5")), " model o5");
        assert_eq!(
            m(json!("haiku"), json!("claude-haiku-4-5-20251001")),
            " model haiku (runs claude-haiku-4-5-20251001)"
        );
        assert_eq!(
            m(Value::Null, json!("gpt-5-codex")),
            " model gpt-5-codex (the runtime's default)"
        );
        let shown = orchestrator_text(&json!({
            "available": true, "runtime": "claude-code", "model": null,
            "model_actual": "claude-sonnet-4-5-20250929", "thread": "01NEW",
            "name": "orchestrator", "status": "idle", "live": true,
            "configured": {"runtime": "claude-code", "model": null}, "runtimes": [],
        }));
        assert!(
            shown.starts_with(
                "orchestrator: claude-code model claude-sonnet-4-5-20250929 (the runtime's \
                 default)\n"
            ),
            "{shown}"
        );
        let switched = orchestrator_use_text(&json!({
            "thread": "01NEW", "name": "orchestrator", "runtime": "codex", "model": null,
            "started": false,
            "previous": {"thread": "01OLD", "runtime": "claude-code", "model": "haiku",
                         "model_actual": "claude-haiku-4-5-20251001",
                         "stopping": false, "dropped_prompts": 0},
        }));
        assert!(
            switched.starts_with("orchestrator is now codex (the runtime's default model):"),
            "{switched}"
        );
        assert!(
            switched.contains(
                "The previous orchestrator (claude-code model haiku (runs \
                 claude-haiku-4-5-20251001), 01OLD)"
            ),
            "{switched}"
        );

        let cell = |t: Value| model_cell(&t);
        assert_eq!(
            cell(json!({"model": "opus", "model_actual": "claude-opus-4-1-20250805"})),
            "claude-opus-4-1-20250805"
        );
        assert_eq!(cell(json!({"model": "opus"})), "opus (requested)");
        assert_eq!(
            cell(json!({"model": null, "model_actual": null})),
            "(runtime default)"
        );
    }

    /// `twr mode` says what changed and when the runtime takes it; `thread list` shows the
    /// mode a live runtime still runs in.
    #[test]
    fn mode_outcomes_and_cells() {
        let r = |applies: &str| {
            json!({"thread_name": "api", "mode": "plan", "previous": "auto",
                   "changed": true, "applies": applies})
        };
        assert_eq!(
            mode_outcome("api", &r("now")),
            "api: auto -> plan (its runtime restarts on the same session now)"
        );
        assert!(mode_outcome("api", &r("after_turn")).contains("finishes the current turn"));
        assert!(mode_outcome("api", &r("next_start")).contains("next time"));
        assert!(mode_outcome("api", &r("never")).contains("pty"));
        let mut narrowing = r("now");
        narrowing["stops_background_tasks"] = json!(true);
        assert_eq!(
            mode_outcome("api", &narrowing),
            "api: auto -> plan (its runtime restarts on the same session now; its background \
             tasks end with it)"
        );
        assert_eq!(
            mode_outcome("api", &json!({"mode": "plan", "changed": false})),
            "api is already in plan mode"
        );
        assert_eq!(mode_cell(&json!({"mode": "auto"})), "auto");
        assert_eq!(
            mode_cell(&json!({"mode": "auto", "runtime_mode": "manual"})),
            "auto (running manual until restarted)"
        );
    }

    /// `twr model` says what changed and when the runtime takes it, and shows the model and
    /// effort (and what a live runtime still runs with) with only the thread.
    /// `twr thread new` and `twr model` print the hub's advisory (`warning`) to stderr; a
    /// result without one prints nothing more.
    #[test]
    fn results_with_a_warning_say_it() {
        let w = "box gpu reports no `codex` on its PATH; the thread may fail to start: run `twr \
                 box doctor gpu`";
        let made = json!({"id": "t1", "name": "api", "runtime": "codex", "warning": w});
        assert_eq!(result_warning(&made), Some(format!("warning: {w}")));
        let set = json!({"thread_name": "api", "changed": true, "model": "o3", "warning": w});
        assert_eq!(result_warning(&set), Some(format!("warning: {w}")));
        assert_eq!(result_warning(&json!({"id": "t1", "name": "api"})), None);
        assert_eq!(result_warning(&json!({"warning": " "})), None);
        assert_eq!(result_warning(&json!({"warning": 3})), None);
    }

    #[test]
    fn model_outcomes_and_show() {
        let r = |applies: &str| {
            json!({"thread_name": "orbit-webapp-1", "model": "claude-fable-5-1",
                   "effort": "high", "previous_model": null, "previous_effort": null,
                   "changed": true, "applies": applies})
        };
        assert_eq!(
            model_outcome("web", &r("after_turn")),
            "orbit-webapp-1: model (runtime default), effort (runtime default) -> model \
             claude-fable-5-1, effort high (its runtime finishes the current turn, then \
             restarts on the same session)"
        );
        assert!(model_outcome("web", &r("now")).ends_with("restarts on the same session now)"));
        assert!(model_outcome("web", &r("next_start")).contains("next time"));
        assert_eq!(
            model_outcome(
                "web",
                &json!({"model": "opus", "effort": null, "changed": false})
            ),
            "web already runs with model opus, effort (runtime default)"
        );
        assert_eq!(
            model_show(
                &json!({"name": "api", "runtime": "claude-code", "model": null,
                               "effort": "low", "model_actual": "claude-sonnet-5"})
            ),
            "api: runtime claude-code, model (runtime default), effort low (runs \
             claude-sonnet-5)"
        );
        assert_eq!(
            model_show(
                &json!({"name": "api", "runtime": "claude-code", "model": "fable",
                               "effort": "high", "runtime_effort": null})
            ),
            "api: runtime claude-code, model fable, effort high; its runtime runs model \
             fable, effort (runtime default) until it restarts"
        );
        let moving = |resume: Value| {
            model_show(&json!({"name": "api", "runtime": "codex", "model": null,
                               "effort": null, "handoff_pending": true,
                               "resume_session": resume}))
        };
        assert!(moving(Value::Null).ends_with(
            "its next turn starts a new codex session, given a handoff of the previous one"
        ));
        assert!(moving(json!("th-1")).ends_with(
            "its next turn resumes its earlier codex session th-1, told what the thread did since"
        ));
    }

    /// `twr hub stop` writes this build's checkpoint first only for a hub of another version
    /// on the workspace's own log: not for one of this version, one that does not say its
    /// version, or one on another database.
    #[test]
    fn hub_stop_prewarms_only_a_hub_of_another_build_on_this_log() {
        let db = Path::new("/h/twr.db");
        let info = |version: Value, at: Option<&str>| {
            let mut v = json!({ "version": version, "pid": 7 });
            if let Some(at) = at {
                v["db"] = json!(at);
            }
            v
        };
        let (this, build) = ("0.3.0+dev.abc", "0.3.0+dev.abc src 1");
        let wanted = |info: &Value| prewarm_wanted(info, build, db);
        // An older hub says its version only: it may be another build of this version (a
        // dirty checkout's), so it gets one too.
        assert!(wanted(&info(json!("0.3.0+dev.123"), Some("/h/twr.db"))));
        assert!(wanted(&info(json!("0.2.9"), None)));
        assert!(wanted(&info(json!(this), Some("/h/twr.db"))));
        assert!(!wanted(&Value::Null), "no answer, nothing known");
        assert!(!wanted(&info(json!("0.2.9"), Some("/other/twr.db"))));
        // Two builds of one version differ in their checkpoint build.
        let mut same_version = info(json!(this), Some("/h/twr.db"));
        same_version["checkpoint_build"] = json!("0.3.0+dev.abc src 2");
        assert!(wanted(&same_version));
        same_version["checkpoint_build"] = json!(build);
        assert!(!wanted(&same_version), "this very build");
    }

    /// `twr hub stop` says "stopped" only once the hub's process is seen gone: an unreadable
    /// process table keeps it waiting, and at the deadline it says it can't tell.
    #[test]
    fn hub_stop_never_guesses_that_the_hub_exited() {
        use twr_runner::reap::Alive;
        let log = Path::new("/d/hub.log");
        let t = Duration::from_secs(30);
        let step = |alive, past| hub_stop_step(Some(alive), past, 42, t, log);
        for past in [false, true] {
            assert_eq!(
                step(Alive::No, past).unwrap().as_deref(),
                Some("hub stopped (pid 42)")
            );
        }
        assert_eq!(step(Alive::Yes, false).unwrap(), None);
        assert_eq!(step(Alive::Unknown, false).unwrap(), None);
        let e = step(Alive::Yes, true).unwrap_err().to_string();
        assert!(e.contains("still shutting down after 30s"), "{e}");
        let e = step(Alive::Unknown, true).unwrap_err().to_string();
        assert!(
            e.contains(
                "can't tell whether the hub (pid 42) exited (the process table is \
                 unreadable); check before starting another"
            ),
            "{e}"
        );
    }

    #[test]
    fn hub_stop_an_unfinished_process_check_is_not_unreadable() {
        let e = hub_stop_step(
            None,
            true,
            42,
            Duration::from_secs(1),
            Path::new("/hub.log"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("could not confirm") && e.contains("process check did not finish"),
            "{e}"
        );
        assert!(!e.contains("unreadable"), "{e}");
        assert!(hub_stop_step(
            Some(twr_runner::reap::Alive::No),
            true,
            42,
            Duration::from_secs(1),
            Path::new("/hub.log")
        )
        .unwrap()
        .is_some());
    }

    #[tokio::test]
    async fn hub_stop_observes_an_exit_at_the_deadline() {
        let mut calls = 0;
        let done = wait_for_stopped_hub(
            42,
            Duration::from_secs(1),
            Path::new("/hub.log"),
            tokio::time::Instant::now(),
            || {
                calls += 1;
                std::future::ready(twr_runner::reap::Alive::No)
            },
        )
        .await
        .unwrap();
        assert_eq!(
            calls, 1,
            "must observe the process even at an expired deadline"
        );
        assert_eq!(done, "hub stopped (pid 42)");
        let e = wait_for_stopped_hub(
            42,
            Duration::from_secs(1),
            Path::new("/hub.log"),
            tokio::time::Instant::now(),
            std::future::pending::<twr_runner::reap::Alive>,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("process check did not finish") && !e.contains("unreadable"),
            "{e}"
        );
    }

    #[test]
    fn hub_stop_connection_retries_preserve_rpc_refusals() {
        let reset = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert!(stop_connection_lost(&reset));
        let closed = twr_core::client::HubClosed::new("hub.info", true, None, None);
        assert!(stop_connection_lost(&closed.into()));
        let refusal = anyhow::Error::from(RpcError::new(
            codes::UNAUTHORIZED,
            "hub closed the connection during hello",
        ));
        assert!(!stop_connection_lost(&refusal));
        assert!(!stop_connection_lost(&anyhow!(
            "the hub serves workspace b, not a"
        )));
    }

    #[test]
    fn park_and_resume_outcomes_read_well() {
        let r = json!({"worktree": "/w/api", "worktree_removed": true, "kept_dirty": false,
                       "kept": false, "pending": 0});
        assert_eq!(
            park_outcome("api", &r),
            "parked api; removed worktree /w/api (branch kept)"
        );
        let r = json!({"worktree": "/w/api", "worktree_removed": false, "kept_dirty": true,
                       "dirty_files": 2, "kept": false, "pending": 1});
        assert_eq!(
            park_outcome("api", &r),
            "parked api; kept worktree /w/api: 2 uncommitted file(s); 1 queued prompt(s) wait for `twr resume`"
        );
        let r = json!({"worktree": "/w/api", "kept": true, "pending": 0});
        assert_eq!(park_outcome("api", &r), "parked api; kept worktree /w/api");
        assert_eq!(park_outcome("x", &json!({"pending": 0})), "parked x");
        let r = json!({"worktree": "/w/api", "worktree_recreated": true, "turn": 3});
        assert_eq!(
            resume_outcome("api", &r),
            "resumed api; recreated worktree /w/api; turn 3 started"
        );
        assert_eq!(
            resume_outcome("api", &json!({"worktree_recreated": false, "turn": null})),
            "resumed api"
        );
    }

    #[test]
    fn answers_are_labels_or_numbers() {
        let opts = vec!["allow".to_string(), "deny".to_string()];
        assert_eq!(
            answer_param("deny", &opts).unwrap(),
            AnswerParam::Label("deny".into())
        );
        assert_eq!(
            answer_param(" Allow ", &opts).unwrap(),
            AnswerParam::Label("allow".into())
        );
        assert_eq!(answer_param("2", &opts).unwrap(), AnswerParam::Index(1));
        assert!(answer_param("3", &opts).is_err());
        assert!(answer_param("0", &opts).is_err());
        assert!(answer_param("  ", &opts).is_err());
        // Labels win over numbers; free text passes through (questions without options).
        let numeric = vec!["2".to_string(), "1".to_string()];
        assert_eq!(
            answer_param("1", &numeric).unwrap(),
            AnswerParam::Label("1".into())
        );
        assert_eq!(
            answer_param("use sqlite", &[]).unwrap(),
            AnswerParam::Label("use sqlite".into())
        );
        assert_eq!(
            answer_param("7", &[]).unwrap(),
            AnswerParam::Label("7".into())
        );
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(one_line("a\n  b"), "a b");
    }

    /// The fake runtime with an interactive CLI that is not installed.
    struct NoCli(twr_adapters::FakeAdapter);

    impl twr_core::adapter::Adapter for NoCli {
        fn info(&self) -> twr_core::adapter::AdapterInfo {
            twr_core::adapter::AdapterInfo {
                name: "nocli",
                ..self.0.info()
            }
        }

        fn spawn(
            &self,
            cfg: twr_core::adapter::SpawnConfig,
            prompt: Option<String>,
        ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
            self.0.spawn(cfg, prompt)
        }

        fn attach_command(
            &self,
            _cfg: &twr_core::adapter::SpawnConfig,
            _session_id: &str,
        ) -> Option<std::process::Command> {
            Some(std::process::Command::new("twr-test-no-such-runtime-cli"))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn attach_without_the_runtime_cli_says_so_and_ends_the_lease() {
        use std::sync::Arc;
        use twr_core::event::{Source, TurnFrom};

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let mut opts = HubOptions::new(paths.db_file(), Config::default());
        opts.adapters = Arc::new(|n: &str| {
            (n == "nocli").then(|| {
                Arc::new(NoCli(twr_adapters::FakeAdapter::new()))
                    as Arc<dyn twr_core::adapter::Adapter>
            })
        });
        let listener = twr_hub::server::bind(&paths.socket).await.unwrap();
        let hub = twr_hub::Hub::open(opts).await.unwrap();
        let server = tokio::spawn(twr_hub::server::serve(
            hub.clone(),
            listener,
            paths.socket.clone(),
            false,
        ));
        let t = hub
            .new_thread(
                twr_hub::NewThread {
                    project: dir.path().to_string_lossy().into_owned(),
                    runtime: Some("nocli".into()),
                    name: Some("t".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let id = t["id"].as_str().unwrap().to_string();
        let mut sub = hub.subscribe_thread(&id, 0).unwrap();
        hub.send(&id, "hi", TurnFrom::User, Source::User("test".into()))
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while sub.next().await.unwrap().unwrap().kind() != "report.ready" {}
        })
        .await
        .unwrap();

        let e = attach(&paths.socket, "t", false).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("`twr-test-no-such-runtime-cli` is not installed"),
            "{msg}"
        );
        assert!(msg.contains("nocli thread"), "{msg}");
        let v = hub.thread_view(&id).unwrap();
        assert!(v.attached.is_none(), "the lease was ended");
        let (_, evs) = hub.thread_events(&id, 0, 100).unwrap();
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind()).collect();
        assert_eq!(
            kinds[kinds.len() - 2..],
            ["thread.attached", "thread.detached"]
        );
        hub.request_shutdown();
        server.await.unwrap().unwrap();
    }

    /// `twr delete` takes one or more threads; `twr thread list` takes `--all` / `-a`.
    #[test]
    fn delete_and_list_parse() {
        let c = Cli::try_parse_from(["twr", "delete", "api", "web", "-y"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Delete { ref threads, yes: true }) if threads == &["api", "web"]
        ));
        let c = Cli::try_parse_from(["twr", "delete", "api"]).unwrap();
        assert!(matches!(
            c.cmd,
            Some(Cmd::Delete { ref threads, yes: false }) if threads == &["api"]
        ));
        assert!(Cli::try_parse_from(["twr", "delete"]).is_err());
        for (args, want) in [
            (&["twr", "thread", "list"][..], false),
            (&["twr", "thread", "list", "--all"][..], true),
            (&["twr", "thread", "list", "-a"][..], true),
        ] {
            let c = Cli::try_parse_from(args).unwrap();
            assert!(
                matches!(c.cmd, Some(Cmd::Thread { cmd: ThreadCmd::List { all } }) if all == want),
                "{args:?}"
            );
        }
    }

    /// What `twr delete` reads from a delete card and how it names the threads.
    #[test]
    fn delete_cards_name_their_threads() {
        let names = |n: &[&str]| names_text(&n.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(names(&["api"]), "api");
        assert_eq!(names(&["api", "web"]), "api and web");
        assert_eq!(names(&["a", "b", "c"]), "a, b and c");
        let card = json!({"thread": "T1", "thread_name": "a", "threads": [
            {"thread": "T1", "thread_name": "a"}, {"thread": "T2", "thread_name": "b"}]});
        assert_eq!(
            card_threads(&card, "a"),
            [
                ("T1".to_string(), "a".to_string()),
                ("T2".to_string(), "b".to_string())
            ]
        );
        // A hub that names one thread.
        let one = json!({"thread": "T1", "thread_name": "a"});
        assert_eq!(
            card_threads(&one, "a"),
            [("T1".to_string(), "a".to_string())]
        );
        let mut said = String::new();
        let mut ask = |q: &str| {
            said = q.to_string();
            Ok("y".to_string())
        };
        assert_eq!(
            delete_reply("a and b", Consent::Ask(&mut ask)).unwrap(),
            DeleteReply::Delete
        );
        assert_eq!(said, "Delete a and b? [y/N] ");
        let mut no = |_: &str| Ok("n".to_string());
        assert_eq!(
            delete_reply("a", Consent::Ask(&mut no)).unwrap(),
            DeleteReply::Cancel
        );
        let mut empty = |_: &str| Ok(String::new());
        assert_eq!(
            delete_reply("a", Consent::Ask(&mut empty)).unwrap(),
            DeleteReply::Open
        );
        assert_eq!(
            delete_reply("a", Consent::NoTerminal).unwrap(),
            DeleteReply::Open
        );
        assert_eq!(
            deleted_outcome(
                "b",
                Some(&json!({"branch": "twr/b", "branch_only_commits": 1, "dropped_prompts": 0}))
            ),
            "deleted b; moved branch twr/b to the trash with 1 commit(s) on no other branch, tag \
             or remote"
        );
        assert_eq!(
            deleted_outcome(
                "c",
                Some(&json!({"worktree_left": "/w/c", "branch": "twr/c", "dropped_prompts": 0}))
            ),
            "deleted c; left part of /w/c: it could not all be moved (no worktree any more), \
             remove it by hand; moved branch twr/c to the trash"
        );
        assert_eq!(
            deleted_outcome(
                "e",
                Some(&json!({"build_output_aside": ["target/"], "dropped_prompts": 0}))
            ),
            "deleted e; could not delete its build output target/ nor put it back: it is kept in \
             the trash item, and a restore puts it back"
        );
        assert_eq!(
            deleted_outcome(
                "d",
                Some(&json!({"worktree": "/w/d", "head_only_commits": 2, "dropped_prompts": 0}))
            ),
            "deleted d; moved worktree /w/d to the trash; kept 2 commit(s) made on a detached \
             HEAD in it in the trash"
        );
        assert_eq!(
            deleted_outcome(
                "e",
                Some(&json!({"worktree": "/w/e", "dropped_prompts": 0,
                    "trash": "01J0000000000000000ABCDEFGH", "trash_bytes": 2048}))
            ),
            "deleted e; moved worktree /w/e to the trash; trash item ABCDEFGH (2 KB) (`twr trash \
             restore ABCDEFGH` brings it back, `twr trash empty ABCDEFGH` purges it)"
        );
    }

    /// `twr trash`: list (alias ls, --json), empty (items or --all, not both, -y) and restore
    /// (--as) parse; the list is a table with the markers of offline boxes and incomplete
    /// items, then the boxes that could not answer and the hub's message.
    #[test]
    fn trash_commands_parse_and_render() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|c| c.cmd);
        assert!(matches!(
            parse(&["twr", "trash", "ls", "--json"]).unwrap(),
            Some(Cmd::Trash {
                cmd: TrashCmd::List { json: true }
            })
        ));
        assert!(matches!(
            parse(&["twr", "trash", "empty", "abcd", "api", "-y"]).unwrap(),
            Some(Cmd::Trash { cmd: TrashCmd::Empty { ref items, all: false, yes: true } })
                if items == &["abcd", "api"]
        ));
        assert!(matches!(
            parse(&["twr", "trash", "empty", "--all"]).unwrap(),
            Some(Cmd::Trash {
                cmd: TrashCmd::Empty {
                    all: true,
                    yes: false,
                    ..
                }
            })
        ));
        assert!(parse(&["twr", "trash", "empty"]).is_err());
        assert!(parse(&["twr", "trash", "empty", "x", "--all"]).is_err());
        assert!(matches!(
            parse(&["twr", "trash", "restore", "abcd", "--as", "api-2"]).unwrap(),
            Some(Cmd::Trash { cmd: TrashCmd::Restore { ref item, as_name: Some(ref n) } })
                if item == "abcd" && n == "api-2"
        ));
        let r = json!({
            "items": [
                {"short": "ABCDEFGH", "thread_name": "api", "project": "twr", "box": "local",
                 "deleted_at": "2026-10-04T12:30:05Z", "purge_at": "2026-10-11T12:30:05Z",
                 "size": "1.2 GB", "losses": "3 uncommitted files"},
                {"short": "IJKLMNOP", "thread_name": "web", "project": null, "box": "gpu",
                 "deleted_at": "2026-10-04T13:00:00Z", "purge_at": "2026-10-11T13:00:00Z",
                 "size": "4 KB", "losses": null, "box_offline": true},
                {"short": "QRSTUVWX", "thread_name": "db", "project": "twr", "box": "local",
                 "deleted_at": "2026-10-04T14:00:00Z", "purge_at": "2026-10-11T14:00:00Z",
                 "size": "8 KB", "losses": "nothing", "restored_as": "db-2",
                 "restored_at": "2026-10-05T09:15:00Z"},
            ],
            "unavailable": [{"box": "lab", "error": "timed out"}],
            "message": "2 items in the trash (1.2 GB).",
        });
        assert_eq!(
            trash_text(&r),
            "ID        NAME  PROJECT  BOX    DELETED               PURGED                SIZE    LOSSES\n\
             ABCDEFGH  api   twr      local  2026-10-04 12:30 UTC  2026-10-11 12:30 UTC  1.2 GB  3 uncommitted files\n\
             IJKLMNOP  web   -        gpu    2026-10-04 13:00 UTC  2026-10-11 13:00 UTC  4 KB    (box offline)\n\
             QRSTUVWX  db    twr      local  2026-10-04 14:00 UTC  2026-10-11 14:00 UTC  8 KB    nothing (restored as db-2 on 2026-10-05 09:15 UTC)\n\
             box lab could not list its trash: timed out\n\
             2 items in the trash (1.2 GB).\n"
        );
        assert_eq!(
            trash_text(&json!({"items": [], "message": "The trash is empty."})),
            "The trash is empty.\n"
        );
        assert_eq!(
            trash_empty_text(&json!({
                "purged": [{"short": "ABCDEFGH", "thread_name": "api", "box": "local",
                            "bytes": 4096}],
                "failed": [{"item": "IJKL", "box": "gpu", "error": "no item"}],
                "message": "Purged 1 item (4 KB freed); 1 could not be purged.",
            })),
            "purged ABCDEFGH (api, box local, 4 KB)\ncould not purge IJKL (box gpu): no item\n\
             Purged 1 item (4 KB freed); 1 could not be purged."
        );
    }

    /// `twr trash empty` asks before purging, listing the items it purges, and without a
    /// terminal refuses unless `--yes`.
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_empty_asks_first() {
        let item = |short: &str, name: &str, size: &str| {
            json!({"item": format!("01J000000000000000{short}"), "short": short,
                   "thread_name": name, "box": "local", "size": size})
        };
        assert_eq!(
            trash_empty_question(&[item("ABCDEFGH", "api", "1.2 GB")], "1.2 GB"),
            "This purges, for good:\n  ABCDEFGH (api, box local, 1.2 GB)\n\
             Purge this item (1.2 GB)? This cannot be undone. [y/N] "
        );
        assert_eq!(
            trash_empty_question(
                &[
                    item("ABCDEFGH", "api", "1.2 GB"),
                    item("IJKLMNOP", "web", "4 KB")
                ],
                "1.2 GB"
            ),
            "This purges, for good:\n  ABCDEFGH (api, box local, 1.2 GB)\n  \
             IJKLMNOP (web, box local, 4 KB)\n\
             Purge these 2 items (1.2 GB)? This cannot be undone. [y/N] "
        );
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let opts = HubOptions::new(paths.db_file(), Config::default());
        let listener = twr_hub::server::bind(&paths.socket).await.unwrap();
        let hub = twr_hub::Hub::open(opts).await.unwrap();
        let server = tokio::spawn(twr_hub::server::serve(
            hub.clone(),
            listener,
            paths.socket.clone(),
            false,
        ));
        let e = trash_empty(&paths.socket, &["abcd".into()], false, Consent::NoTerminal)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("pass --yes"), "{e:#}");
        // An unknown item: the hub's refusal, before anything is asked.
        let mut never = |_: &str| -> anyhow::Result<String> { panic!("asked") };
        let e = trash_empty(
            &paths.socket,
            &["abcd".into()],
            false,
            Consent::Ask(&mut never),
        )
        .await
        .unwrap_err();
        assert!(format!("{e:#}").contains("no item"), "{e:#}");
        // --all on an empty trash says so without asking.
        trash_empty(&paths.socket, &[], true, Consent::Ask(&mut never))
            .await
            .unwrap();
        let e = trash_empty(&paths.socket, &["abcd".into()], false, Consent::Yes)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("no item"), "{e:#}");
        hub.request_shutdown();
        server.await.unwrap().unwrap();
    }

    /// What `twr trash empty` purges is what its question listed, by id: an item deleted while
    /// the question waits stays, with `--all` and with a thread's name that came to name two
    /// items meanwhile alike; `n` purges nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn trash_empty_purges_only_what_it_listed() {
        use std::sync::Arc;
        use twr_core::adapter::Answer;

        /// Runs `f` to its end from inside the synchronous question.
        fn block<F: std::future::Future>(f: F) -> F::Output {
            tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
        }
        /// A `fake` thread `name` on `repo` (its own worktree), deleted to the trash: its id.
        async fn trashed(hub: &twr_hub::Hub, repo: &Path, name: &str) -> String {
            let user = twr_hub::Caller::user("test");
            let t = hub
                .new_thread(
                    twr_hub::NewThread {
                        project: repo.to_string_lossy().into_owned(),
                        runtime: Some("fake".into()),
                        name: Some(name.into()),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            let id = t["id"].as_str().unwrap().to_string();
            let card = hub
                .delete_threads(std::slice::from_ref(&id), &user)
                .await
                .unwrap();
            hub.respond_at(
                card["attn_id"].as_str().unwrap(),
                Answer::new("Delete"),
                card["revision"].as_u64(),
                &user,
            )
            .unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                while hub.thread_view(&id).is_ok() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("deleted");
            id
        }
        /// The threads of the items in the trash, by id.
        async fn in_trash(hub: &twr_hub::Hub) -> Vec<String> {
            let mut threads: Vec<String> = hub.trash_list().await.unwrap()["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| str_of(&i["thread"]).to_string())
                .collect();
            threads.sort();
            threads
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let repo = twr_core::testkit::git::init_repo(&root.join("proj"));
        let paths = Paths::with_home(&root.join("home"));
        let mut opts = HubOptions::new(paths.db_file(), Config::default());
        opts.adapters = Arc::new(|n: &str| {
            (n == "fake").then(|| {
                Arc::new(twr_adapters::FakeAdapter::new()) as Arc<dyn twr_core::adapter::Adapter>
            })
        });
        let listener = twr_hub::server::bind(&paths.socket).await.unwrap();
        let hub = twr_hub::Hub::open(opts).await.unwrap();
        let server = tokio::spawn(twr_hub::server::serve(
            hub.clone(),
            listener,
            paths.socket.clone(),
            false,
        ));
        let a = trashed(&hub, &repo, "a").await;
        let x1 = trashed(&hub, &repo, "x").await;

        // `n`: nothing goes.
        let mut no = |_: &str| Ok("n".to_string());
        trash_empty(&paths.socket, &[], true, Consent::Ask(&mut no))
            .await
            .unwrap();
        let mut want = vec![a.clone(), x1.clone()];
        want.sort();
        assert_eq!(in_trash(&hub).await, want);

        // By name: "x" names x1's item when asked; a second x deleted while the question
        // waits makes the name ambiguous, and its item stays.
        let mut asked = Vec::new();
        let mut x2 = String::new();
        let mut yes_after_x2 = |q: &str| {
            asked.push(q.to_string());
            x2 = block(trashed(&hub, &repo, "x"));
            Ok("y".to_string())
        };
        trash_empty(
            &paths.socket,
            &["x".into()],
            false,
            Consent::Ask(&mut yes_after_x2),
        )
        .await
        .unwrap();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("(x, box local"), "{}", asked[0]);
        assert!(asked[0].contains("Purge this item"), "{}", asked[0]);
        let mut want = vec![a.clone(), x2.clone()];
        want.sort();
        assert_eq!(in_trash(&hub).await, want);

        // --all: the items listed go; c, deleted while the question waits, stays.
        let mut asked = Vec::new();
        let mut c = String::new();
        let mut yes_after_c = |q: &str| {
            asked.push(q.to_string());
            c = block(trashed(&hub, &repo, "c"));
            Ok("y".to_string())
        };
        trash_empty(&paths.socket, &[], true, Consent::Ask(&mut yes_after_c))
            .await
            .unwrap();
        assert!(asked[0].contains("Purge these 2 items"), "{}", asked[0]);
        assert!(!asked[0].contains("(c, box"), "{}", asked[0]);
        assert_eq!(in_trash(&hub).await, [c.clone()]);
        hub.request_shutdown();
        server.await.unwrap().unwrap();
    }

    #[test]
    fn since_takes_windows_dates_and_times() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-11T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let at = |s: &str| parse_since(s, now).map(|t| t.to_rfc3339());
        assert_eq!(at("30m").unwrap(), "2026-10-11T11:30:00+00:00");
        assert_eq!(at("24h").unwrap(), "2026-10-10T12:00:00+00:00");
        assert_eq!(at(" 7d ").unwrap(), "2026-10-04T12:00:00+00:00");
        assert_eq!(at("2w").unwrap(), "2026-09-27T12:00:00+00:00");
        assert_eq!(at("2026-10-01").unwrap(), "2026-10-01T00:00:00+00:00");
        assert_eq!(
            at("2026-10-01T08:00:00+02:00").unwrap(),
            "2026-10-01T06:00:00+00:00"
        );
        for bad in ["", "7", "d", "7y", "-1d", "99999999999999d"] {
            assert!(at(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn spent_lines_tabulate_the_rows_and_the_total() {
        let u = json!({
            "since": "2026-10-04T12:00:00Z", "by": "project",
            "rows": [
                {"key": "api", "label": "api", "input": 1_200_000, "output": 30_000,
                 "cost": 3.4, "turns": 4, "costed_turns": 4},
                {"key": "web", "label": "web", "input": 9_000, "output": 1_000,
                 "cost": null, "turns": 1, "costed_turns": 0},
            ],
            "total": {"key": "total", "label": "total", "input": 1_209_000, "output": 31_000,
                      "cost": 3.4, "turns": 5, "costed_turns": 4},
        });
        let lines = spent_lines(&u);
        assert!(lines[0].starts_with("PROJECT  TURNS  IN"), "{lines:?}");
        assert!(
            lines[1].starts_with("api") && lines[1].ends_with("$3.40"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("web") && lines[2].ends_with('-'),
            "{lines:?}"
        );
        // A row whose turns reported no cost still counts its turns and tokens.
        assert!(
            lines[2].contains("  1  ") && lines[2].contains("9.0k"),
            "{lines:?}"
        );
        assert!(lines[3].starts_with("total"), "{lines:?}");
        assert!(
            lines[4].starts_with("5 turn(s) since 2026-10-04T12:00:00Z."),
            "{lines:?}"
        );
        let none = spent_lines(&json!({"since": null, "by": "thread", "rows": []}));
        assert_eq!(none, ["no turn reported usage"]);
    }

    #[test]
    fn job_commands_parse() {
        let c = Cli::try_parse_from([
            "twr",
            "job",
            "run",
            "--slots",
            "2",
            "--timeout",
            "30m",
            "--",
            "cargo",
            "test",
            "-q",
        ])
        .unwrap();
        match c.cmd {
            Some(Cmd::Job {
                cmd:
                    JobCmd::Run {
                        slots,
                        timeout,
                        argv,
                        no_wake,
                        ..
                    },
            }) => {
                assert_eq!(
                    (slots, timeout.as_deref(), no_wake),
                    (Some(2), Some("30m"), false)
                );
                assert_eq!(argv, ["cargo", "test", "-q"]);
            }
            other => panic!("{other:?}"),
        }
        assert!(Cli::try_parse_from(["twr", "job", "run"]).is_err());
        assert_eq!(parse_duration_secs("90s").unwrap(), 90);
        assert_eq!(parse_duration_secs("30m").unwrap(), 1800);
        assert_eq!(parse_duration_secs("2h").unwrap(), 7200);
        assert_eq!(parse_duration_secs("45").unwrap(), 45);
        for bad in ["", "0", "1d", "x", "99999999999999999999h"] {
            assert!(parse_duration_secs(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn trim_and_disk_cells() {
        assert_eq!(
            trim_outcome(
                &json!({"thread_name": "api", "deleted": ["target/", "node_modules/"],
                "freed": 7_600_000_000u64, "notes": []})
            ),
            "trimmed api: deleted target/, node_modules/ (7.1 GB)"
        );
        assert_eq!(
            trim_outcome(&json!({"thread_name": "web", "deleted": [], "freed": 0,
                "notes": ["its build output target/ stays at .twr-build-output.0/0"]})),
            "trimmed web: no build output to delete\n  its build output target/ stays at \
             .twr-build-output.0/0"
        );
        assert_eq!(disk_cell(&json!({})), "-");
        assert_eq!(
            disk_cell(
                &json!({"disk": {"bytes": 7_600_000_000u64, "build_bytes": 7_300_000_000u64}})
            ),
            "7.1 GB (6.8 GB build)"
        );
        assert_eq!(
            disk_cell(&json!({"disk": {"bytes": 2048, "build_bytes": 0, "partial": true}})),
            "≥2 KB"
        );
    }

    /// `twr thread list`'s SPENT column: tokens, and the cost when one was reported.
    #[test]
    fn thread_list_says_what_each_thread_spent() {
        assert_eq!(spent_cell(&json!({})), "-");
        assert_eq!(
            spent_cell(&json!({"spent": {"input": 1_150_000, "output": 50_000, "turns": 3}})),
            "1.2M tok"
        );
        assert_eq!(
            spent_cell(
                &json!({"spent": {"input": 9_000, "output": 1_000, "cost": 0.5,
                "turns": 2, "costed_turns": 2}})
            ),
            "10k tok $0.50"
        );
    }

    /// `twr thread list`'s last column is the thread's activity line, cut with `…`.
    #[test]
    fn thread_list_ends_with_the_activity() {
        let long = format!(
            "implementing scroll anchoring · {}",
            "running tests ".repeat(10)
        );
        let cell = activity_cell(&json!({ "activity": long }));
        assert!(cell.chars().count() <= ACTIVITY_CELL, "{cell}");
        assert!(
            cell.starts_with("implementing scroll anchoring · running"),
            "{cell}"
        );
        assert!(cell.ends_with('…'), "{cell}");
        assert_eq!(activity_cell(&json!({ "activity": "done" })), "done");
        // (A hub without activity lines: the empty cell.)
        assert_eq!(activity_cell(&json!({})), "-");
    }

    /// `twr thread list` leaves parked threads out unless --all, and names them in a footer.
    #[test]
    fn thread_list_hides_parked_threads() {
        let threads = vec![
            json!({"name": "api", "parked": false}),
            json!({"name": "old", "parked": true}),
            json!({"name": "web"}),
            json!({"name": "older", "parked": true}),
        ];
        let (shown, hidden) = listed(threads.clone(), false);
        let names: Vec<&str> = shown.iter().map(|t| str_of(&t["name"])).collect();
        assert_eq!(names, ["api", "web"]);
        assert_eq!(hidden, ["old", "older"]);
        assert_eq!(
            parked_footer(&hidden),
            "2 parked threads hidden: old, older (twr thread list --all)"
        );
        assert_eq!(
            parked_footer(&["old".to_string()]),
            "1 parked thread hidden: old (twr thread list --all)"
        );
        let (shown, hidden) = listed(threads, true);
        assert_eq!((shown.len(), hidden.len()), (4, 0));
    }

    /// `twr delete a b --yes` against a hub: one card for both, then both are gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn delete_deletes_several_threads() {
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let mut opts = HubOptions::new(paths.db_file(), Config::default());
        opts.adapters = Arc::new(|n: &str| {
            (n == "fake").then(|| {
                Arc::new(twr_adapters::FakeAdapter::new()) as Arc<dyn twr_core::adapter::Adapter>
            })
        });
        let listener = twr_hub::server::bind(&paths.socket).await.unwrap();
        let hub = twr_hub::Hub::open(opts).await.unwrap();
        let server = tokio::spawn(twr_hub::server::serve(
            hub.clone(),
            listener,
            paths.socket.clone(),
            false,
        ));
        let mut ids = Vec::new();
        for name in ["a", "b"] {
            let t = hub
                .new_thread(
                    twr_hub::NewThread {
                        project: dir.path().to_string_lossy().into_owned(),
                        runtime: Some("fake".into()),
                        name: Some(name.into()),
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            ids.push(t["id"].as_str().unwrap().to_string());
        }
        let threads = vec!["a".to_string(), "b".to_string()];
        delete(&paths.socket, &threads, Consent::Yes).await.unwrap();
        for id in &ids {
            assert!(hub.thread_view(id).is_err(), "{id} is gone");
        }
        assert!(hub.list_threads().is_empty());
        // Nothing is asked without a terminal or --yes: the card stays open.
        let t = hub
            .new_thread(
                twr_hub::NewThread {
                    project: dir.path().to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some("c".into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        delete(&paths.socket, &["c".to_string()], Consent::NoTerminal)
            .await
            .unwrap();
        assert!(hub.thread_view(t["id"].as_str().unwrap()).is_ok());
        assert_eq!(hub.attention_list(false).len(), 1);
        hub.request_shutdown();
        server.await.unwrap().unwrap();
    }

    #[test]
    fn table_aligns() {
        let t = table(
            &["A", "BB"],
            &[
                ["xxx".to_string(), "y".to_string()],
                ["z".to_string(), "".to_string()],
            ],
        );
        assert_eq!(t, "A    BB\nxxx  y\nz\n");
    }

    /// `twr log compact` says what moved, where to, how long the write lock was held at most,
    /// and what the `VACUUM` did; `twr log stats` the sizes, kinds, checkpoints, backlog and
    /// what the next run moves.
    #[test]
    fn log_texts() {
        let r = json!({
            "rows": 1200, "bytes": 5_000_000, "purged_threads": 2, "turns": 7, "blobs": 30,
            "blob_bytes": 9_000_000, "archived": true, "batches": 2, "longest_batch_ms": 14,
            "total_ms": 900, "freed_pages": 0, "checkpoints_dropped": 2,
            "vacuum": {"ms": 3100, "before": 400_000_000, "after": 90_000_000,
                       "incremental_set": true},
        });
        let t = log_compact_text(&r);
        assert!(t.contains("dropped 2 checkpoint(s)"), "{t}");
        assert!(
            t.contains(
                "1200 agent event(s), 5 MB (2 purged thread(s), 7 turn(s)), into archive.db"
            ),
            "{t}"
        );
        assert!(t.contains("the longest 14 ms"), "{t}");
        assert!(
            t.contains("VACUUM: twr.db 381 MB → 86 MB in 3100 ms"),
            "{t}"
        );
        let t = log_compact_text(&json!({"skipped": "no agent events to move", "archived": false}));
        assert_eq!(t, "moved nothing: no agent events to move\n");
        let st = json!({
            "db": {"bytes": 1000, "wal": 0, "free_pages": 3, "pages": 10,
                   "auto_vacuum": "none",
                   "kinds": {"agent.text": {"rows": 5, "bytes": 500}},
                   "blobs": {"count": 1, "bytes": 10}},
            "archive": null,
            "backlog": {"full": 40, "compacted": 9},
            "checkpoints": [{"id": 3, "age_secs": 600, "bytes": 2048, "events_after": 12}],
            "retention_days": 30, "archive_on": true,
            "next": {"rows": 5, "bytes": 500, "purged_threads": 0, "turns": 1,
                     "automatic": false},
        });
        let t = log_stats_text(&st);
        for want in [
            "auto_vacuum none",
            "archive.db   none yet",
            "agent.text",
            "control backlog: 40 item(s), 9 compacted",
            "checkpoint 3: 10 min old, 2 KB, 12 event(s) after it",
            "5 agent event(s), 500 bytes",
            "the hub waits for 2 000",
        ] {
            assert!(t.contains(want), "{want}: {t}");
        }
    }

    /// A socket that accepts connections but only answers after `quiet` has passed.
    fn slow_hub(sock: &Path, quiet: Duration) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::UnixListener::bind(sock).unwrap();
        let born = tokio::time::Instant::now();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    if born.elapsed() < quiet {
                        // Busy: hold the connection without answering.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        return;
                    }
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let resp = json!({"id": req["id"], "result": {}});
                        let _ = w.write_all(format!("{resp}\n").as_bytes()).await;
                    }
                });
            }
        });
    }

    /// A connect refused for a full backlog, or timed out, is a busy hub, not an absent one nor
    /// an error.
    #[test]
    fn a_full_backlog_is_a_busy_hub() {
        let full = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::WouldBlock))
            .context("connecting to the hub");
        assert!(backlog_full(&full) && !nothing_listens(&full));
        let refused =
            anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(!backlog_full(&refused) && nothing_listens(&refused));
    }
    #[tokio::test]
    async fn probes_tell_absent_from_busy() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hub.sock");
        assert_eq!(probe_hub(&sock).await.unwrap(), Probe::Absent);
        assert_eq!(ws_status(&sock, "default").await["hub"], "stopped");
        // A socket file nobody listens on: connection refused.
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert_eq!(probe_hub(&sock).await.unwrap(), Probe::Absent);
        assert_eq!(ws_status(&sock, "default").await["hub"], "stopped");
        std::fs::remove_file(&sock).unwrap();
        slow_hub(&sock, Duration::from_secs(60));
        assert_eq!(probe_hub(&sock).await.unwrap(), Probe::Busy);

        let sock2 = dir.path().join("hub2.sock");
        slow_hub(&sock2, Duration::ZERO);
        assert_eq!(probe_hub(&sock2).await.unwrap(), Probe::Answers);
    }

    /// Only a socket nothing listens on counts as no hub. One that can't be used for another
    /// reason (as another user's, which the client refuses) is an error that says why, for
    /// the probe (so no hub is started to fail there) and for `twr ws`, not a stopped hub.
    #[tokio::test]
    async fn probes_report_a_socket_they_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, "").unwrap();
        let long = dir.path().join("x".repeat(200));
        for (sock, why) in [
            (file.join("hub.sock"), "connecting to hub"),
            (long.join("hub.sock"), "Unix socket paths can be at most"),
        ] {
            let e = format!("{:#}", probe_hub(&sock).await.unwrap_err());
            assert!(e.contains(why), "{e}");
            let status = ws_status(&sock, "default").await;
            assert_eq!(status["hub"], "error", "{status}");
            assert!(status["error"].as_str().unwrap().contains(why), "{status}");
        }
    }

    /// A hub trusts a test token only from a regular file of its own user with no group or
    /// other permission bits: not one others may read or write, not a link to a private one,
    /// not another user's, not a directory.
    #[test]
    fn the_hub_trusts_only_a_private_token_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("user.token");
        std::fs::write(&file, " t0k\n").unwrap();
        let mode = |m| std::fs::set_permissions(&file, std::fs::Permissions::from_mode(m));
        let uid = std::fs::metadata(&file).unwrap().uid();
        mode(0o600).unwrap();
        assert_eq!(private_token_file(&file, uid), Ok("t0k".into()));
        mode(0o400).unwrap();
        assert_eq!(private_token_file(&file, uid), Ok("t0k".into()));
        for open in [0o640, 0o604, 0o620, 0o602, 0o644] {
            mode(open).unwrap();
            let e = private_token_file(&file, uid).unwrap_err();
            assert!(e.contains("make it 0600"), "{open:o}: {e}");
        }
        mode(0o600).unwrap();
        let e = private_token_file(&file, uid + 1).unwrap_err();
        assert!(e.contains("owned by uid"), "{e}");
        let link = dir.path().join("link.token");
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let e = private_token_file(&link, uid).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        let e = private_token_file(dir.path(), uid).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        assert!(private_token_file(&dir.path().join("none"), uid).is_err());
        std::fs::write(&file, "  \n").unwrap();
        let e = private_token_file(&file, uid).unwrap_err();
        assert!(e.contains("no token"), "{e}");
        // A FIFO is refused, and never blocks the hub (nothing writes to it here).
        let fifo = dir.path().join("fifo.token");
        let made = std::process::Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        let e = private_token_file(&fifo, uid).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
    }

    /// The local agent check is waived for a test's processes only with a token file a hub
    /// would trust: a debug build on a test home, naming a private file of this user.
    #[test]
    fn the_agent_check_is_waived_only_for_a_private_token_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("user.token");
        std::fs::write(&file, "t0k\n").unwrap();
        let mode = |m| std::fs::set_permissions(&file, std::fs::Permissions::from_mode(m));
        let uid = std::fs::metadata(&file).unwrap().uid();
        let named = Some(file.as_os_str());
        mode(0o600).unwrap();
        assert!(test_user_waived(true, named, uid));
        assert!(!test_user_waived(false, named, uid));
        assert!(!test_user_waived(true, None, uid));
        assert!(!test_user_waived(true, named, uid + 1));
        // Readable (or writable) by others: any agent could have written it.
        mode(0o644).unwrap();
        assert!(!test_user_waived(true, named, uid));
        mode(0o622).unwrap();
        assert!(!test_user_waived(true, named, uid));
    }

    #[tokio::test]
    async fn a_busy_hub_is_waited_for_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let sock = dir.path().join("hub.sock");
        // Silent for longer than one probe (which used to be enough to spawn a second hub).
        slow_hub(&sock, Duration::from_millis(2500));
        wait_for_hub(&sock, &paths).await.unwrap();
        // No hub was spawned: spawning creates the data dir and the hub log.
        assert!(!paths.hub_log().exists());
        assert!(!paths.data_dir.exists());
    }

    /// clap's own checks of the command definition (e.g. `twr update --version` next to the
    /// global `--version`, `twr hub` with an optional subcommand).
    #[test]
    fn the_command_line_definition_is_valid() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
        let hub = Cli::try_parse_from(["twr", "hub"]).unwrap();
        assert!(matches!(hub.cmd, Some(Cmd::Hub { cmd: None })));
        let stop = Cli::try_parse_from(["twr", "hub", "stop", "--timeout", "5"]).unwrap();
        assert!(matches!(
            stop.cmd,
            Some(Cmd::Hub {
                cmd: Some(HubCmd::Stop {
                    timeout: 5,
                    all: false
                })
            })
        ));
        assert!(Cli::try_parse_from(["twr", "hub", "stop", "--timeout", "0"]).is_err());
        let stop = Cli::try_parse_from(["twr", "hub", "stop", "--all"]).unwrap();
        assert!(matches!(
            stop.cmd,
            Some(Cmd::Hub {
                cmd: Some(HubCmd::Stop { all: true, .. })
            })
        ));
        for args in [
            vec!["twr", "--socket", "/tmp/hub.sock", "hub", "stop", "--all"],
            vec!["twr", "hub", "stop", "--all", "--socket", "/tmp/hub.sock"],
        ] {
            if let Ok(cli) = Cli::try_parse_from(args) {
                assert!(refuse_bulk_stop_socket(&cli).is_err());
            }
        }
        let update = Cli::try_parse_from(["twr", "update", "--version", "v0.2.0"]).unwrap();
        assert!(matches!(update.cmd, Some(Cmd::Update { version: Some(v) }) if v == "v0.2.0"));
    }

    /// `twr update` installs into the executable's own directory, and refuses cargo's bin
    /// directories and an executable the installer would not replace.
    #[test]
    fn update_installs_next_to_the_executable_but_not_over_cargo() {
        let cargo = vec![
            PathBuf::from("/home/u/.cargo/bin"),
            PathBuf::from("/opt/cargo/bin"),
        ];
        assert_eq!(
            install_dir(Path::new("/home/u/.local/bin/twr"), &cargo).unwrap(),
            PathBuf::from("/home/u/.local/bin")
        );
        for exe in ["/home/u/.cargo/bin/twr", "/opt/cargo/bin/twr"] {
            let e = install_dir(Path::new(exe), &cargo).unwrap_err().to_string();
            assert!(e.contains("cargo's bin directory"), "{e}");
            assert!(e.contains("cargo install --path crates/twr-cli"), "{e}");
            assert!(e.contains("cargo uninstall twr-cli"), "{e}");
        }
        let e = install_dir(Path::new("/usr/local/bin/twr-dev"), &cargo)
            .unwrap_err()
            .to_string();
        assert!(e.contains("/usr/local/bin/twr"), "{e}");

        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                vars.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert_eq!(
            cargo_bin_dirs(env(&[
                ("CARGO_HOME", "/nonexistent-twr/cargo"),
                ("HOME", "/nonexistent-twr/home")
            ])),
            vec![
                PathBuf::from("/nonexistent-twr/cargo/bin"),
                PathBuf::from("/nonexistent-twr/home/.cargo/bin")
            ]
        );
        assert_eq!(
            cargo_bin_dirs(env(&[("CARGO_HOME", ""), ("HOME", "/nonexistent-twr/h")])),
            vec![PathBuf::from("/nonexistent-twr/h/.cargo/bin")]
        );
    }

    /// Only a `#!/bin/sh` script is run; an empty body or an error page is refused.
    #[test]
    fn only_a_sh_script_counts_as_the_installer() {
        let url = "https://twr.tools/install.sh";
        check_script(url, b"#!/bin/sh\nset -eu\n").unwrap();
        check_script(url, b"#!/bin/sh -e\r\necho\n").unwrap();
        check_script(url, b"#!/bin/sh").unwrap();
        for bad in [
            &b""[..],
            b"<!doctype html><title>404</title>",
            b"#!/bin/shx\n",
            b"#!/bin/bash\n",
            b"\n#!/bin/sh\n",
        ] {
            let e = check_script(url, bad).unwrap_err().to_string();
            assert!(e.contains("nothing was run"), "{e}");
        }
    }

    /// Release files come over https or from a local file:/// path, never plain http.
    #[tokio::test]
    async fn release_files_are_fetched_over_https_or_from_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("install.sh");
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let url = format!("file://{}", script.display());
        assert_eq!(
            fetch_release_file(&url).await.unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        let url = format!("file://localhost{}", script.display());
        assert_eq!(
            fetch_release_file(&url).await.unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        let e = fetch_release_file("file://install.sh")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a local file:// URL"), "{e}");
        let e = fetch_release_file("http://twr.tools/install.sh")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("only downloads over https://"), "{e}");
        std::fs::write(&script, vec![b'#'; RELEASE_FILE_MAX + 1]).unwrap();
        let url = format!("file://{}", script.display());
        let e = fetch_release_file(&url).await.unwrap_err().to_string();
        assert!(e.contains("nothing was run"), "{e}");
        // A link to a regular file is read; a device (through a link too) and a FIFO are
        // refused at once, without reading them.
        std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
        let link = dir.path().join("link.sh");
        std::os::unix::fs::symlink(&script, &link).unwrap();
        let url = format!("file://{}", link.display());
        assert_eq!(
            fetch_release_file(&url).await.unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        let zero = dir.path().join("zero.sh");
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        let fifo = dir.path().join("fifo.sh");
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success());
        for path in [Path::new("/dev/zero"), &zero, &fifo] {
            let url = format!("file://{}", path.display());
            let e = tokio::time::timeout(Duration::from_secs(20), fetch_release_file(&url))
                .await
                .unwrap_or_else(|_| panic!("reading {url} hung"))
                .unwrap_err();
            assert!(
                format!("{e:#}").contains("not a regular file"),
                "{url}: {e:#}"
            );
        }
    }

    /// A read of a release file stuck past the timeout (a stalled mount) fails the update at
    /// the timeout, and nothing waits for the stuck read afterwards: the runtime the command
    /// ran on ends at once.
    #[test]
    fn a_stuck_release_file_read_does_not_hold_the_command() {
        let started = std::time::Instant::now();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(read_detached(Duration::from_millis(200), || {
            std::thread::sleep(Duration::from_secs(30));
            Ok(Vec::new())
        }));
        assert!(r.is_err(), "{r:?}");
        drop(rt);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the command waited {:?} for the stuck read",
            started.elapsed()
        );
        // A read that finishes in time gives its result.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let r = rt.block_on(read_detached(
            Duration::from_secs(20),
            || Ok(b"ok".to_vec()),
        ));
        assert_eq!(r.unwrap().unwrap(), b"ok");
    }

    /// The test key pair's files (crates/twr-cli/tests/fixtures/signing).
    fn signing_fixture(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/signing")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The test key pair, to sign with.
    fn test_signer() -> ring::signature::Ed25519KeyPair {
        let pem = signing_fixture("test-key.pem");
        let body: String = pem
            .lines()
            .filter(|l| !l.starts_with("-----"))
            .collect::<Vec<_>>()
            .concat();
        let der = twr_core::b64::decode(&body).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der).unwrap()
    }

    /// A key pair that is neither the release key nor the test key.
    fn other_signer() -> ring::signature::Ed25519KeyPair {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap()
    }

    /// The .sig file of `data`, as the release workflow writes it.
    fn sign(signer: &ring::signature::Ed25519KeyPair, data: &[u8]) -> Vec<u8> {
        twr_core::b64::encode(signer.sign(data).as_ref()).into_bytes()
    }

    /// The key built into twr is the one in release/signing-key.pub.
    #[test]
    fn the_built_in_release_key_is_the_committed_one() {
        let pem = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../release/signing-key.pub"),
        )
        .unwrap();
        assert_eq!(ed25519_public_key(&pem).unwrap(), RELEASE_KEY);
        // The test key is another one, and parses the same way.
        let test = ed25519_public_key(&signing_fixture("test-key.pub")).unwrap();
        assert_ne!(test, RELEASE_KEY);
        use ring::signature::KeyPair;
        assert_eq!(test_signer().public_key().as_ref(), &test[..]);
        for bad in [
            "",
            "-----BEGIN PUBLIC KEY-----\n-----END PUBLIC KEY-----",
            // An RSA key's start (another algorithm).
            "-----BEGIN PUBLIC KEY-----\nMCIwDQYJKoZIhvcNAQEBBQADEQAwDgIJAKs=\n-----END PUBLIC KEY-----",
            "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA==\n-----END PRIVATE KEY-----",
        ] {
            assert!(ed25519_public_key(bad).is_err(), "{bad}");
        }
    }

    /// Only a test build verifies with another key than the release key.
    #[test]
    fn only_a_test_build_takes_another_key() {
        let file =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signing/test-key.pub");
        let test = ed25519_public_key(&signing_fixture("test-key.pub")).unwrap();
        assert_eq!(release_key(true, Some(file.clone().into())).unwrap(), test);
        assert_eq!(release_key(false, Some(file.into())).unwrap(), RELEASE_KEY);
        assert_eq!(release_key(true, None).unwrap(), RELEASE_KEY);
        assert_eq!(release_key(true, Some("".into())).unwrap(), RELEASE_KEY);
        assert!(release_key(true, Some("/nonexistent/key.pub".into())).is_err());
        assert_eq!(
            release_key(false, Some("/nonexistent/key.pub".into())).unwrap(),
            RELEASE_KEY
        );
    }

    /// install.sh's release key block holds the committed public key, and its probe (the
    /// signature openssl must verify before install.sh trusts it) verifies with that key.
    #[test]
    fn install_sh_embeds_the_release_key_and_a_good_probe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = std::fs::read_to_string(root.join("install.sh")).unwrap();
        let block = script
            .split_once("\n# --- release key ---\n")
            .and_then(|(_, rest)| rest.split_once("\n# --- end release key ---\n"))
            .map(|(block, _)| block)
            .expect("install.sh has a release key block");
        let value = |name: &str| {
            block
                .split_once(&format!("\n{name}='"))
                .and_then(|(_, rest)| rest.split_once('\''))
                .map(|(v, _)| v)
                .unwrap_or_else(|| panic!("no {name} in the release key block"))
        };
        let pem = std::fs::read_to_string(root.join("release/signing-key.pub")).unwrap();
        assert_eq!(value("RELEASE_KEY"), pem.trim_end());
        assert_eq!(value("PROBE_MESSAGE"), "twr signature probe");
        let probe = value("PROBE_SIGNATURE");
        check_signature(
            &RELEASE_KEY,
            b"twr signature probe",
            probe.as_bytes(),
            "m",
            "s",
        )
        .unwrap();
    }

    /// A signature `openssl pkeyutl -sign -rawin` made (the release workflow's tool) verifies
    /// with ring, which `twr update` checks with.
    #[test]
    fn ring_verifies_an_openssl_signature() {
        let key = ed25519_public_key(&signing_fixture("test-key.pub")).unwrap();
        let sig = signing_fixture("probe.sig");
        check_signature(&key, b"twr signature probe", sig.as_bytes(), "m", "s").unwrap();
        let e = check_signature(&key, b"twr signature probe!", sig.as_bytes(), "m", "s")
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not match its signature"), "{e}");
        // ring signs as openssl does: Ed25519 signatures are deterministic.
        assert_eq!(
            sign(&test_signer(), b"twr signature probe"),
            sig.trim().as_bytes()
        );
    }

    /// Only a signature by the key, over exactly these bytes, passes.
    #[test]
    fn signatures_are_checked_against_the_key() {
        let key = ed25519_public_key(&signing_fixture("test-key.pub")).unwrap();
        let data = b"abc  twr-x86_64-unknown-linux-musl.tar.gz\n";
        let good = sign(&test_signer(), data);
        check_signature(&key, data, &good, "u", "s").unwrap();
        let mut newline = good.clone();
        newline.push(b'\n');
        check_signature(&key, data, &newline, "u", "s").unwrap();
        let mismatch = |data: &[u8], sig: &[u8]| {
            let e = check_signature(&key, data, sig, "u", "s")
                .unwrap_err()
                .to_string();
            assert!(e.contains("does not match its signature"), "{e}");
            assert!(e.contains("nothing was run"), "{e}");
        };
        mismatch(b"abd  twr-x86_64-unknown-linux-musl.tar.gz\n", &good);
        mismatch(data, &sign(&other_signer(), data));
        let e = check_signature(&RELEASE_KEY, data, &good, "u", "s")
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not match its signature"), "{e}");
        for bad in [&b""[..], b"not base64!", b"YWJj", &good[..80], b"\xff\xfe"] {
            let e = check_signature(&key, data, bad, "u", "s")
                .unwrap_err()
                .to_string();
            assert!(e.contains("s is not an Ed25519 signature"), "{e}");
        }
    }

    /// `--version` is read as install.sh reads `TWR_VERSION`, and only release tags go into a
    /// URL.
    #[test]
    fn release_versions_and_tags() {
        assert_eq!(release_version(""), None);
        assert_eq!(release_version("latest"), None);
        assert_eq!(release_version("v0.2.0").as_deref(), Some("v0.2.0"));
        assert_eq!(release_version("0.2.0").as_deref(), Some("v0.2.0"));
        assert_eq!(
            release_version("0.2.0-rc.1").as_deref(),
            Some("v0.2.0-rc.1")
        );
        for good in ["v0.2.0", "v10.20.30", "v0.2.0-rc.1", "v1.0.0-alpha-2.b"] {
            assert!(is_release_tag(good), "{good}");
        }
        for bad in [
            "",
            "v",
            "0.2.0",
            "v0.2",
            "v0.2.0.1",
            "v0..2",
            "v0.2.x",
            "v0.2.0-",
            "v0.2.0+build",
            "v0.2.0-rc/../x",
            "v0.2.0-rc 1",
            "V0.2.0",
            "v0.2.0\n",
        ] {
            assert!(!is_release_tag(bad), "{bad:?}");
        }
        assert_eq!(
            tag_of_release_url("https://github.com/ignaval/twr/releases/tag/v0.2.0").unwrap(),
            "v0.2.0"
        );
        assert_eq!(
            tag_of_release_url("/ignaval/twr/releases/tag/v0.2.0-rc.1").unwrap(),
            "v0.2.0-rc.1"
        );
        for bad in [
            "https://github.com/ignaval/twr/releases",
            "https://github.com/ignaval/twr/releases/tag/",
            "https://github.com/ignaval/twr/releases/tag/v0.2.0/../../x",
            "https://github.com/other/twr/releases/tag/v0.2.0",
            "https://example.com/ignaval/twr/releases/tag/v0.2.0",
        ] {
            assert!(tag_of_release_url(bad).is_err(), "{bad}");
        }
    }

    /// release.yml publishes only tags `twr update` installs: the same grammar, without build
    /// metadata (`v0.2.0+build.1`).
    #[test]
    fn releases_are_published_only_with_tags_update_installs() {
        let workflow = include_str!("../../../.github/workflows/release.yml");
        assert!(workflow.contains(r#"[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]"#));
        assert!(is_release_tag("v0.2.0-rc.1"));
        assert!(!is_release_tag("v0.2.0+build.1"));
    }

    /// release.yml writes sha256sums.txt as `twr update` reads it ([`release_of`]): first the
    /// line naming the release, and install.sh's checksum among the archives'.
    #[test]
    fn release_checksums_name_the_release_and_list_the_installer() {
        let workflow = include_str!("../../../.github/workflows/release.yml");
        assert!(workflow.contains(&format!(
            r#"printf '{RELEASE_LINE}%s\n' "$TAG" > sha256sums.txt"#
        )));
        assert!(workflow.contains(
            r#"echo "$(sha256sum dist/install.sh | awk '{ print $1 }')  install.sh" >> sha256sums.txt"#
        ));
        let script = include_str!("../../../install.sh");
        assert!(script.contains(&format!(r#"index($0, "{RELEASE_LINE}") == 1"#)));
        assert!(script.contains(&format!("substr($0, {})", RELEASE_LINE.len() + 1)));
    }

    /// The release is resolved once: an explicit version names the GitHub release directly
    /// (no lookup), and the archive comes from it; a mirror's directory holds everything,
    /// the archive included, with or without a version; a bad version is refused.
    #[tokio::test]
    async fn the_update_source_is_one_release() {
        let source = update_source(Some("0.2.0"), None).await.unwrap();
        assert_eq!(
            source,
            UpdateSource {
                tag: Some("v0.2.0".into()),
                script: "https://github.com/ignaval/twr/releases/download/v0.2.0/install.sh".into(),
                base: "https://github.com/ignaval/twr/releases/download/v0.2.0".into(),
                archives: None,
            }
        );
        let mirror = Some("https://example.com/twr/v0.2.0/install.sh".to_string());
        let source = update_source(Some("v0.2.0"), mirror.clone()).await.unwrap();
        assert_eq!(
            source,
            UpdateSource {
                tag: Some("v0.2.0".into()),
                script: "https://example.com/twr/v0.2.0/install.sh".into(),
                base: "https://example.com/twr/v0.2.0".into(),
                archives: Some("https://example.com/twr/v0.2.0".into()),
            }
        );
        // No version: no lookup of the latest on GitHub, the mirror's release is the one.
        for latest in [None, Some("latest"), Some("")] {
            let source = update_source(latest, mirror.clone()).await.unwrap();
            assert_eq!(source.tag, None);
            assert_eq!(
                source.archives.as_deref(),
                Some("https://example.com/twr/v0.2.0")
            );
        }
        let source = update_source(None, Some("file:///r/custom-installer.sh".into()))
            .await
            .unwrap();
        assert_eq!(source.script, "file:///r/custom-installer.sh");
        assert_eq!(source.base, "file:///r");
        assert_eq!(source.archives.as_deref(), Some("file:///r"));
        for bad in ["https://example.com", "https://example.com/", "install.sh"] {
            assert!(
                update_source(None, Some(bad.into())).await.is_err(),
                "{bad}"
            );
        }
        let e = update_source(Some("v0.2.0/../../x"), None)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a release version"), "{e}");
    }

    /// `twr update` gets the installer and the checksums only with good signatures: a changed
    /// file, a missing or malformed signature, or another key's signature fails the download.
    #[tokio::test]
    async fn only_signed_release_files_are_returned() {
        let dir = tempfile::tempdir().unwrap();
        let key = ed25519_public_key(&signing_fixture("test-key.pub")).unwrap();
        let script = b"#!/bin/sh\necho install\n".to_vec();
        let sums = b"0123  twr-x86_64-unknown-linux-musl.tar.gz\n".to_vec();
        let write = |name: &str, data: &[u8]| std::fs::write(dir.path().join(name), data).unwrap();
        let release = || {
            write("install.sh", &script);
            write("install.sh.sig", &sign(&test_signer(), &script));
            write("sha256sums.txt", &sums);
            write("sha256sums.txt.sig", &sign(&test_signer(), &sums));
        };
        let source = UpdateSource {
            tag: Some("v9.9.9".into()),
            script: format!("file://{}/install.sh", dir.path().display()),
            base: format!("file://{}", dir.path().display()),
            archives: None,
        };
        release();
        assert_eq!(
            fetch_verified(&source, &key).await.unwrap(),
            (script.clone(), sums.clone())
        );
        // The real release key did not sign these.
        let e = fetch_verified(&source, &RELEASE_KEY)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("does not match its signature"), "{e}");
        // A differently named installer is signed in `<name>.sig`, beside it; install.sh.sig
        // (here the signature of another script) is not looked at.
        let custom = b"#!/bin/sh\necho custom\n".to_vec();
        write("custom-installer.sh", &custom);
        write("custom-installer.sh.sig", &sign(&test_signer(), &custom));
        let custom_source = UpdateSource {
            script: format!("file://{}/custom-installer.sh", dir.path().display()),
            ..source.clone()
        };
        assert_eq!(
            fetch_verified(&custom_source, &key).await.unwrap(),
            (custom.clone(), sums.clone())
        );
        std::fs::remove_file(dir.path().join("custom-installer.sh.sig")).unwrap();
        let e = format!(
            "{:#}",
            fetch_verified(&custom_source, &key).await.unwrap_err()
        );
        assert!(e.contains("custom-installer.sh.sig: no signature"), "{e}");

        let gone = |name: &'static str| {
            let path = dir.path().join(name);
            move || std::fs::remove_file(&path).unwrap()
        };
        type Change<'a> = Box<dyn Fn() + 'a>;
        let cases: Vec<(&str, Change, String)> = vec![
            (
                "a changed installer",
                Box::new(|| write("install.sh", b"#!/bin/sh\necho evil\n")),
                "install.sh does not match its signature".into(),
            ),
            (
                "changed checksums",
                Box::new(|| {
                    write(
                        "sha256sums.txt",
                        b"4567  twr-x86_64-unknown-linux-musl.tar.gz\n",
                    )
                }),
                "sha256sums.txt does not match its signature".into(),
            ),
            (
                "another key's signature",
                Box::new(|| write("sha256sums.txt.sig", &sign(&other_signer(), &sums))),
                "sha256sums.txt does not match its signature".into(),
            ),
            (
                "the checksums' signature for the installer",
                Box::new(|| write("install.sh.sig", &sign(&test_signer(), &sums))),
                "install.sh does not match its signature".into(),
            ),
            (
                "a malformed signature",
                Box::new(|| write("sha256sums.txt.sig", b"signed!\n")),
                "sha256sums.txt.sig is not an Ed25519 signature".into(),
            ),
            (
                "no installer signature",
                Box::new(gone("install.sh.sig")),
                "install.sh.sig: no signature".into(),
            ),
            (
                "no checksums signature",
                Box::new(gone("sha256sums.txt.sig")),
                "sha256sums.txt.sig: no signature".into(),
            ),
        ];
        for (what, change, expected) in cases {
            release();
            change();
            let e = format!("{:#}", fetch_verified(&source, &key).await.unwrap_err());
            assert!(e.contains(&expected), "{what}: {e}");
            assert!(e.contains("nothing was run"), "{what}: {e}");
        }
        // A missing signature says what that means, and offers no way around it.
        release();
        gone("sha256sums.txt.sig")();
        let e = format!("{:#}", fetch_verified(&source, &key).await.unwrap_err());
        assert!(e.contains("Every twr release is signed"), "{e}");
        assert!(!e.contains("UNSIGNED") && !e.contains("curl"), "{e}");
        // A signature that can't be read for another reason is that error alone.
        release();
        std::fs::remove_file(dir.path().join("sha256sums.txt.sig")).unwrap();
        std::fs::create_dir(dir.path().join("sha256sums.txt.sig")).unwrap();
        let e = format!("{:#}", fetch_verified(&source, &key).await.unwrap_err());
        assert!(e.contains("sha256sums.txt.sig"), "{e}");
        assert!(
            !e.contains("no signature") && !e.contains("UNSIGNED"),
            "{e}"
        );
        std::fs::remove_dir(dir.path().join("sha256sums.txt.sig")).unwrap();
    }

    /// The verified checksums must name the release asked for, once, and list the installer
    /// that came with them: signed files of another release, or one release's installer with
    /// another's checksums, are refused; a mirror without `--version` installs the release its
    /// checksums name.
    #[test]
    fn the_checksums_name_the_release_and_list_the_installer() {
        let script = b"#!/bin/sh\necho install\n";
        let hash = |data: &[u8]| -> String {
            ring::digest::digest(&ring::digest::SHA256, data)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        };
        let sums = |head: &str, installer: &[u8]| {
            format!(
                "{head}{}  twr-x86_64-unknown-linux-musl.tar.gz\n{}  install.sh\n",
                "0".repeat(64),
                hash(installer)
            )
        };
        let source = |tag: Option<&str>| UpdateSource {
            tag: tag.map(String::from),
            script: "https://example.com/twr/install.sh".into(),
            base: "https://example.com/twr".into(),
            archives: Some("https://example.com/twr".into()),
        };
        let good = sums("# twr release v9.9.9\n", script);
        let asked = source(Some("v9.9.9"));
        assert_eq!(
            release_of(&asked, script, good.as_bytes()).unwrap(),
            "v9.9.9"
        );
        assert_eq!(
            release_of(&source(None), script, good.as_bytes()).unwrap(),
            "v9.9.9"
        );
        let refused = |source: &UpdateSource, script: &[u8], sums: &str, expected: &str| {
            let e = format!(
                "{:#}",
                release_of(source, script, sums.as_bytes()).unwrap_err()
            );
            assert!(e.contains(expected), "{sums}: {e}");
            assert!(e.contains("nothing was run"), "{sums}: {e}");
        };
        // Another release's checksums, signed as they are.
        refused(
            &asked,
            script,
            &sums("# twr release v9.9.8\n", script),
            "is the checksums of v9.9.8, not of v9.9.9",
        );
        // No release named, two, or one that is not a release tag.
        for head in [
            "",
            "# twr release v9.9.9\n# twr release v9.9.8\n",
            "# twr release latest\n",
        ] {
            refused(
                &source(None),
                script,
                &sums(head, script),
                "does not name one release",
            );
        }
        // Another installer than the one the checksums list, or none listed.
        refused(
            &asked,
            b"#!/bin/sh\necho older\n",
            &good,
            "is not the install.sh of v9.9.9",
        );
        refused(
            &asked,
            script,
            &format!("# twr release v9.9.9\n{}  twr.tar.gz\n", "0".repeat(64)),
            "lists no install.sh",
        );
    }

    /// Only a file that is not there counts as a missing signature: HTTP 404, or a `file://`
    /// path that does not exist; a server error or a timeout does not.
    #[test]
    fn a_missing_release_file_is_404_or_no_such_file() {
        let http = |code: u16| {
            anyhow::Error::from(HttpStatus {
                url: "https://example.com/x.sig".into(),
                status: reqwest::StatusCode::from_u16(code).unwrap(),
            })
            .context("fetching")
        };
        assert!(is_missing(&http(404)));
        for code in [403, 500, 502, 503] {
            assert!(!is_missing(&http(code)), "{code}");
        }
        assert_eq!(
            http(404).root_cause().to_string(),
            "downloading https://example.com/x.sig failed: HTTP 404 Not Found"
        );
        let io = |kind| anyhow::Error::from(std::io::Error::from(kind)).context("reading");
        assert!(is_missing(&io(std::io::ErrorKind::NotFound)));
        assert!(!is_missing(&io(std::io::ErrorKind::PermissionDenied)));
        assert!(!is_missing(&io(std::io::ErrorKind::TimedOut)));
        assert!(!is_missing(&anyhow!("dns error")));
    }
}

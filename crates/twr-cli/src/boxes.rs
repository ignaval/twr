//! `twr box`: the remote boxes, from the command line. `add` sets up a box the hub reaches over
//! SSH (`[boxes.<name>] transport = "ssh"`, in `boxes.toml`) or through a command (`transport
//! = "command"`, a container: `--command docker exec -i <container>`) and adds it to the
//! running hub (`box.add`); `list` and `status` show `hub.info`'s boxes; `install` puts a twr
//! build on a box the hub connects to (the hub's own, a release's for another OS or CPU, or a
//! file) and has the hub connect again (`box.install`); `restart` has the hub restart such a
//! box's daemon (`box.restart`); `remove` takes a box out of `boxes.toml`; `doctor` checks a
//! box and `login` starts an agent CLI's login on it ([`doctor`]); `project` finds (or clones)
//! a project's repository on a box and records where it is ([`project`]).
//!
//! Everything run on a box goes through its link ([`BoxLink`], [`here`]). The probes run ssh
//! in the user's terminal, with the hub's ssh options but without `BatchMode`
//! ([`twr_hub::ssh_box::interactive_ssh_args`]), so ssh can ask about an unknown host key or
//! for a passphrase. A command box's command runs as the hub runs it: in a session and process
//! group of its own, without a terminal (nothing there may prompt), its whole group killed
//! when it ends, and its probe checks that it passes this side's input on and gives the box no
//! terminal (the pipe check). The probe script and its parser are the hub's
//! ([`twr_hub::ssh_box::probe_script_for`], [`twr_hub::ssh_box::parse_probe_for`]). What needs a
//! terminal on the box (`login`, `project --clone`) runs ssh with one, or a command box's
//! `terminal_command`, in the user's terminal. `boxes.toml` is changed only under its lock
//! ([`BoxesFile::lock`]): read, changed, checked with `config.toml` and saved in one hold.

use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use serde_json::{json, Value};
use twr_core::client::Client;
use twr_core::config::{BoxConfig, BoxTransport, BoxesFile, Config};
use twr_core::git::human_bytes;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_hub::ssh_box::{self, BoxLink, Input};

use super::{
    ask_on_stderr, caller_is_agent, connect, consent, fetch_release_archive, is_release_tag,
    refuse_agent_action, str_of, table, wait_for_hub, workspace_hint, Consent, PrivateDir,
};

mod doctor;
mod project;
mod token;

/// How long `twr box add` and `twr box install` wait for the box to come online.
const ONLINE_WAIT: Duration = Duration::from_secs(90);

/// How long `twr box restart` waits for the box to come back on a new daemon: an attempt of
/// the hub's that is under way when the request lands (it is not cut short: a session it opens
/// sees the restart asked for and ends; the request's wake ends any backoff after it), then
/// the attempt that carries the restart out, each at most [`ssh_box::ATTEMPT_WORST`] (the
/// probe, an upload of the hub's build, the channel of a dial that replaces or restarts the
/// daemon: [`ssh_box::REPLACE_OPEN_TIMEOUT`], the relay's whole stop and start, and the
/// WebSocket's handshake), and [`ONLINE_WAIT`] for the rest (the welcome). About an hour and
/// twenty minutes at most; the box's notes, printed as they come, say what it waits for.
const RESTART_WAIT: Duration =
    Duration::from_secs(2 * ssh_box::ATTEMPT_WORST.as_secs() + ONLINE_WAIT.as_secs());

/// How long `twr box install` waits for a box that is online on another build to run the one
/// it installed: an attempt under way when it asks, the box quiet for 2 minutes
/// ([`ssh_box::SWITCH_QUIET`], looked at every [`ssh_box::SWITCH_CHECK`]), its runtimes given
/// up to [`ssh_box::SWITCH_DRAIN`] to exit, then the attempt that replaces the daemon, each
/// attempt at most [`ssh_box::ATTEMPT_WORST`], and [`ONLINE_WAIT`] for the rest. (How long the
/// box takes to be quiet is the user's: past that, the wait says how it goes.)
const SWITCH_WAIT: Duration = Duration::from_secs(
    2 * ssh_box::ATTEMPT_WORST.as_secs()
        + ssh_box::SWITCH_QUIET.as_secs()
        + ssh_box::SWITCH_CHECK.as_secs()
        + ssh_box::SWITCH_DRAIN.as_secs()
        + ONLINE_WAIT.as_secs(),
);

/// How often they look.
const ONLINE_POLL: Duration = Duration::from_millis(500);

/// How long `twr box install` takes a `needs_install` it reads for the state from before its
/// `box.install`, while the hub has not shown another one (and `twr box restart` an `error` or
/// `needs_install` for the state from before its `box.restart`: [`RestartWait`]).
const STALE_NEEDS_INSTALL: Duration = Duration::from_secs(10);

/// How long the check that ssh works without prompting may take.
const BATCH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub(super) enum BoxCmd {
    /// Add a box the hub reaches over SSH, or through a command (--command: a container):
    /// no token, listener or tunnel. Probes the box with ssh in this terminal (ssh may ask
    /// about its host key or for a passphrase), checks that ssh also works without prompting
    /// (the hub's never prompts), says what will happen and asks y/N (without a terminal, only
    /// with --yes). A command is probed as the hub runs it: without a terminal, so nothing it
    /// runs may prompt, and checked to pass the hub's input to the box (`-i`) without giving
    /// it a terminal (no `-t`). Then writes `[boxes.<name>]` to boxes.toml, next to
    /// config.toml, and adds the box to the running hub (starting one when none runs). The hub
    /// installs its own twr build on the box (same OS and CPU; else run `twr box install
    /// <name>`), connects and starts a twr box daemon there, which runs the box's threads.
    /// Waits up to 90 s for the box to come online. Boxes belong to the default workspace.
    ///
    /// With --dial-in --hub <url>: a box the hub can't reach, whose runner connects to the
    /// hub's listener (`[hub] listen`, which must be set). Writes a token box with a new token
    /// (32 random bytes) to boxes.toml, adds it to the running hub, and prints the command to
    /// run on the box (`twr runner install-token`) and, on a terminal only, the token to paste
    /// there; then, on a terminal, waits up to 10 minutes for its runner to connect.
    Add {
        /// The box's name: letters, digits, `.`, `_` or `-` (not starting with `.` or `-`; not `local`).
        name: String,
        /// The ssh destination (`me@host`, or a host from `~/.ssh/config`), with any ssh
        /// options before it (`-p 2222 me@host`): one argument or several words, joined with
        /// spaces. With --command, the command the hub runs to reach the box (`docker exec -i
        /// <container>`), its words passed as they are, not through a shell. None with
        /// --dial-in.
        #[arg(
            required_unless_present = "dial_in",
            conflicts_with = "dial_in",
            value_name = "SSH",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        words: Vec<String>,
        /// A dial-in box: its runner connects to the hub (`twr runner install-token` on the
        /// box). Given before any other word, with --hub.
        #[arg(long, conflicts_with = "command")]
        dial_in: bool,
        /// With --dial-in: the URL the box's runner dials to reach this hub's listener:
        /// `wss://` (Tailscale serve, or `[hub] tls`), or `ws://` to a loopback address (an ssh
        /// tunnel's end on the box).
        #[arg(long, requires = "dial_in", value_name = "URL")]
        hub: Option<String>,
        /// With --dial-in and a wss:// URL: the CA certificate (PEM) the box's runner trusts,
        /// by its absolute path on the box.
        #[arg(long, requires = "dial_in", value_name = "PEM ON THE BOX")]
        ca: Option<String>,
        /// The words are the command the hub runs to reach the box (`docker exec -i
        /// <container>`, `kubectl exec -i -n <ns> <pod> --`), with `sh -c <what ssh would run>`
        /// appended, instead of an ssh destination. Given before the words. For `docker`,
        /// `podman` and `nerdctl exec -i` and `kubectl exec -i`, the same command with `-t`
        /// becomes its terminal_command (for `twr box login` and `twr box project --clone`).
        #[arg(long)]
        command: bool,
        /// Add the box without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List the hub's remote boxes: name, transport (`ssh` or `command`: the hub connects to
    /// it; `token`: its runner connects to the hub), state, the twr version its runner
    /// reported, its OS and CPU (boxes the hub connects to), and its agent CLIs as it last
    /// reported them (`claude ✓`: logged in, `✗`: logged out, `?`: not known, `-`: not
    /// installed).
    #[command(alias = "ls")]
    List {
        /// Print the boxes of `hub.info` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show what the hub knows of a box: its state, the twr version and build it runs, its
    /// OS and CPU, the last error, its latest notes, a connection the hub refused, and its
    /// agent CLIs (path, version, login).
    Status {
        /// The box's name.
        name: String,
        /// Print the box's entry of `hub.info` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install twr on a box the hub connects to (ssh or command) and have the hub connect to
    /// it again. On a box of the hub's OS and CPU the hub installs its own build. On another
    /// one: with --binary, that file; with --from-box, a twr already on the box (built there
    /// with docs/tools/dev-install.sh, say); otherwise, when the hub runs a release, the release's
    /// archive for the box (its signed checksums verified as `twr update` verifies them),
    /// unpacked on the box. The build is recorded as the box's `build` in boxes.toml (a box of
    /// config.toml: the line to add is printed). Asks y/N before installing (without a
    /// terminal, only with --yes). Waits up to 90 s for the box to come online; a box online
    /// on another build is switched once it has been quiet for 2 minutes, and that is waited
    /// for too (at most about an hour and a half, printing the box's notes meanwhile).
    Install {
        /// The box's name.
        name: String,
        /// A twr executable built for the box's OS and CPU, to upload instead of a release's.
        #[arg(long, value_name = "PATH", conflicts_with = "from_box")]
        binary: Option<PathBuf>,
        /// A twr executable already on the box, by its absolute path there (on the box,
        /// `command -v twr` or `echo ~/.local/bin/twr` prints it), copied into the builds
        /// directory with its checks.
        #[arg(long, value_name = "PATH ON THE BOX")]
        from_box: Option<String>,
        /// Upload without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Restart the daemon of a box the hub connects to (ssh or command): the hub ends its
    /// connection to the box and connects again, having the box stop its daemon (and the
    /// agents it runs) and start a new one. On a Mac,
    /// after someone logged in at its screen, the new daemon runs in that GUI session, where
    /// Claude Code can read its keychain login. Asks y/N first (without a terminal, only with
    /// --yes), then waits for the box to come back online (as long as the hub's attempts may
    /// take: an hour and twenty minutes at most, when the old daemon has a repository change
    /// to finish and a build to upload, printing the box's notes meanwhile) and says how its
    /// daemon was started.
    Restart {
        /// The box's name.
        name: String,
        /// Restart without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove a box from boxes.toml (a box of config.toml: remove it there). The running hub
    /// keeps the box until it restarts; the box daemon and the box's worktrees stay on the box
    /// (the command that stops the daemon is printed), and a command box's container stays
    /// as it is (twr never stops or removes one). A token box's runner is no longer accepted
    /// from then on (the hub drops its connection), and how to stop it on the box is printed.
    #[command(alias = "rm")]
    Remove {
        /// The box's name.
        name: String,
    },
    /// Print a token box's token (from boxes.toml, or config.toml for a box configured there),
    /// and nothing else on stdout, a pipe included: `ssh me@hub twr box token gpu | twr runner
    /// install-token gpu --yes` on a box that can ssh to the hub. --rotate replaces it with a
    /// new one (asks y/N first; without a terminal, only with --yes): the running hub stops
    /// accepting the old one at once and drops the box's connection, until the new one is
    /// installed on the box. --reload has the running hub take the box's token from the files
    /// again (after an edit of config.toml).
    Token {
        /// The box's name.
        name: String,
        /// Replace the token with a new one (boxes.toml boxes only).
        #[arg(long, conflicts_with = "reload")]
        rotate: bool,
        /// Have the running hub read the box's token from the files again.
        #[arg(long)]
        reload: bool,
        /// Rotate without asking.
        #[arg(long, short = 'y', requires = "rotate")]
        yes: bool,
    },
    /// Check a box and say how to fix what is wrong: the hub's view of it (when a hub runs),
    /// ssh without prompting (as the hub runs it) and in this terminal (a command box: its
    /// command, and that it passes the hub's input without a terminal), the box's OS, CPU,
    /// data directory and twr builds, its box daemon, logind (Linux), its agent CLIs (path,
    /// version, login), git, free disk, and the projects configured for it. Works without a
    /// hub and with the box offline. The checks on the box run the hub's twr build when it is
    /// there, else the box's configured build, else the newest one there, and say which.
    /// Never shows account details. Exits 1 when a check failed (✗).
    Doctor {
        /// The box's name.
        name: String,
        /// Print the checks as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start an agent CLI's own login on a box, in this terminal: `claude auth login`, `codex
    /// login --device-auth` or `grok login --device-auth`, over ssh with a terminal (a
    /// command box: its terminal_command), run by the box's twr build (as `twr box doctor`
    /// picks it) on the program the box's runner would start. twr never sees what you type.
    /// Then says whether the agent is logged in (the box daemon's view through the hub when
    /// one runs, else the box's). Needs a terminal.
    Login {
        /// The box's name.
        name: String,
        /// The agent: claude, codex or grok.
        #[arg(value_parser = ["claude", "claude-code", "codex", "grok", "grok-build"])]
        agent: String,
    },
    /// Find a project's repository on a box, or clone it there, and record where it is
    /// (`[boxes.<box>.projects.<project>] repo`, in boxes.toml), so that `twr thread new
    /// <project> --box <box>` uses it. The project is a configured one, or a repository folder
    /// name under project_roots, as `twr thread new` finds it on this machine. Without
    /// options, the box's twr build (as `twr box doctor` picks it) looks at the path the
    /// repository has under your home here, under the box's home, then under the box's
    /// ~/code, ~/src, ... for a repository of that folder name, and reads each one's origin. When
    /// exactly one has the project's origin and the search was complete, it asks y/N (--yes:
    /// it doesn't) and records it; otherwise it lists them, and --path picks one. --path
    /// records the repository there once the box says it is one (its top level). --clone runs
    /// `git clone` of the project's origin on the box, in this terminal, with the box's own git
    /// credentials (no agent forwarding), then records the clone; it needs a terminal (and on
    /// a command box, its terminal_command). twr never fetches. A running hub takes the change
    /// at once.
    Project {
        /// The box's name.
        name: String,
        /// The project: a configured project, or a repository folder name under project_roots.
        project: String,
        /// The repository's absolute path on the box (its top level).
        #[arg(long, value_name = "PATH", conflicts_with_all = ["clone", "remove"])]
        path: Option<String>,
        /// Clone the project's origin on the box (https://, ssh:// or user@host:path, without
        /// credentials), in this terminal, then record the clone.
        #[arg(long, conflicts_with = "remove")]
        clone: bool,
        /// With --clone: the clone's own new directory on the box, as `git clone <url> <dir>`
        /// takes it (its parent must exist; its name: letters, digits, `-`, `_` and `.`).
        /// Default: where the repository is under your home here, under the box's home; else
        /// ~/code/<the repository's folder name>.
        #[arg(long, value_name = "DIR", requires = "clone")]
        into: Option<String>,
        /// Remove the project's entry for the box.
        #[arg(long)]
        remove: bool,
        /// Record (or clone) without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

/// Runs `twr box ...` for the hub on `socket` of `paths`'s workspace, whose config (with
/// boxes.toml) is `config`.
pub(super) async fn run(
    cmd: BoxCmd,
    config: &Config,
    paths: &Paths,
    socket: &Path,
) -> anyhow::Result<ExitCode> {
    match cmd {
        BoxCmd::Add {
            name,
            words,
            command,
            dial_in,
            hub,
            ca,
            yes,
        } => {
            let (words, yes) = destination(&words, yes);
            if dial_in {
                token::add(
                    config,
                    paths,
                    socket,
                    &name,
                    hub.as_deref(),
                    ca.as_deref(),
                    yes,
                )
                .await?
            } else {
                // (clap takes them without --dial-in when ssh words follow.)
                if hub.is_some() || ca.is_some() {
                    bail!("--hub and --ca are for a dial-in box: give --dial-in too");
                }
                refuse_misplaced_dial_in(&words)?;
                add(config, paths, socket, &name, command, words, yes).await?
            }
        }
        BoxCmd::List { json } => list(socket, json).await?,
        BoxCmd::Status { name, json } => status(config, socket, &name, json).await?,
        BoxCmd::Token {
            name,
            rotate,
            reload,
            yes,
        } => match (rotate, reload) {
            (true, _) => token::rotate(paths, socket, &name, yes).await?,
            (_, true) => token::reload(paths, socket, &name).await?,
            _ => token::show(paths, &name)?,
        },
        BoxCmd::Install {
            name,
            binary,
            from_box,
            yes,
        } => {
            let from = match (binary, from_box) {
                (Some(b), _) => Some(InstallFrom::Binary(b)),
                (None, Some(p)) => Some(InstallFrom::Box(p)),
                (None, None) => None,
            };
            install(config, paths, socket, &name, from, yes).await?
        }
        BoxCmd::Restart { name, yes } => restart(config, paths, socket, &name, yes).await?,
        BoxCmd::Remove { name } => remove(paths, socket, &name).await?,
        BoxCmd::Doctor { name, json } => {
            return doctor::doctor(config, paths, socket, &name, json).await
        }
        BoxCmd::Login { name, agent } => {
            return doctor::login(config, paths, socket, &name, &agent).await
        }
        BoxCmd::Project {
            name,
            project,
            path,
            clone,
            into,
            remove,
            yes,
        } => {
            let req = project::Request {
                name,
                project,
                path,
                clone,
                into,
                remove,
                yes,
            };
            return project::project(config, paths, socket, req).await;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `twr box add`'s words (an ssh destination, or with `--command` a command's argv, each word
/// kept apart), and whether `--yes` was given: a `--yes` after the destination lands among its
/// words (they take everything that follows) and is taken out (ssh has no long options, and a
/// command that needs that word can be written in boxes.toml).
fn destination(words: &[String], yes: bool) -> (Vec<String>, bool) {
    let given = words.iter().any(|w| w == "--yes");
    let kept = words.iter().filter(|w| *w != "--yes").cloned().collect();
    (kept, yes || given)
}

/// Refuses `twr box add`'s words when `--dial-in` or `--hub` is among them: given after a
/// destination, they land among its words (which take everything that follows), and a dial-in
/// box has no destination.
fn refuse_misplaced_dial_in(words: &[String]) -> anyhow::Result<()> {
    if words
        .iter()
        .any(|w| w == "--dial-in" || w == "--hub" || w.starts_with("--hub="))
    {
        bail!(
            "--dial-in and --hub go before any other word, and a dial-in box takes no ssh \
             destination or command: `twr box add <name> --dial-in --hub <url>`"
        );
    }
    Ok(())
}

/// The ssh destination of `twr box add`'s words ([`destination`]): joined with spaces.
fn ssh_destination(words: &[String]) -> String {
    words.join(" ").trim().to_string()
}

/// The terminal_command `twr box add` writes for command box `argv`: for `docker`, `podman`
/// or `nerdctl exec` and `kubectl exec` with a word `-i` (`--interactive`, `--stdin`) before
/// any `--`, the same argv with `-t` right after that word, every other one kept (`-u`, `-e
/// HOME=...`, `-w`). `None` for any other command: guessing at its flags would hide a guess in
/// every login and clone (`nsenter -t` is a pid).
fn default_terminal(argv: &[String]) -> Option<Vec<String>> {
    let program = Path::new(argv.first()?).file_name()?.to_str()?;
    if !matches!(program, "docker" | "podman" | "nerdctl" | "kubectl") {
        return None;
    }
    let end = argv.iter().position(|w| w == "--").unwrap_or(argv.len());
    let exec = argv[..end].iter().position(|w| w == "exec")?;
    let at = (exec + 1..end)
        .find(|&i| matches!(argv[i].as_str(), "-i" | "--interactive" | "--stdin"))?;
    let mut terminal = argv.to_vec();
    terminal.insert(at + 1, "-t".to_string());
    Some(terminal)
}

/// `argv` as the user reads it: each word shell-quoted when it needs it.
fn argv_label(argv: &[String]) -> String {
    argv.iter()
        .map(|w| twr_core::paths::sh_word(w))
        .collect::<Vec<_>>()
        .join(" ")
}

/// How twr reaches a box through `link`, as said to the user (the commands that refuse
/// agents): `over ssh (<destination>)`, or `through `<command>``.
fn reached(link: &BoxLink) -> String {
    reached_by(link, false)
}

/// [`reached`], said to an agent when `agent` ([`link_shown`]).
fn reached_by(link: &BoxLink, agent: bool) -> String {
    match link {
        BoxLink::Ssh(ssh) => format!("over ssh ({})", ssh.trim()),
        BoxLink::Command { .. } => format!("through `{}`", link_shown(link, agent)),
    }
}

/// `link` as a command that agents may run (`twr box doctor`) shows it: its label
/// ([`BoxLink::label`]), but to an agent (`agent`) a command box's program and how many words
/// its command has, never the words, as the config's Debug output shows it (one may hold a
/// secret, `-e KEY=value`).
fn link_shown(link: &BoxLink, agent: bool) -> String {
    match link {
        BoxLink::Command { command, .. } if agent => format!(
            "{} ({} words)",
            command.first().map(String::as_str).unwrap_or_default(),
            command.len()
        ),
        _ => link.label(),
    }
}

/// Why a probe through `link` may not finish: a question of ssh's (a host key, a passphrase);
/// a command has none (it runs without a terminal).
fn unanswered(link: &BoxLink) -> &'static str {
    match link {
        BoxLink::Ssh(_) => "a question of ssh's left unanswered, or a box that does not answer",
        BoxLink::Command { .. } => "a command or a box that does not answer",
    }
}

/// Why box `name` (through `link`) cannot run what needs a terminal there (`twr box login`,
/// `twr box project --clone`): a command box without terminal_command, refused before
/// anything runs; `instead`, what to do without one. `None` when it can.
pub(super) fn terminal_refusal(name: &str, link: &BoxLink, instead: &str) -> Option<String> {
    matches!(link, BoxLink::Command { terminal: None, .. }).then(|| {
        format!(
            "box {name} has no terminal_command: set one in [boxes.{name}] (its command with \
             `-t`: `docker exec -i -t <container>`), or {instead}"
        )
    })
}

/// Refuses a named workspace: boxes belong to the default workspace's hub.
fn default_workspace_only(paths: &Paths, cmd: &str) -> anyhow::Result<()> {
    match &paths.workspace {
        Some(w) => bail!(
            "boxes belong to the default workspace's hub, not workspace {w}'s: run `twr box \
             {cmd}` without -w"
        ),
        None => Ok(()),
    }
}

/// The hub's version and build: the running hub's (`hub.info`), else this twr's (the one a
/// hub started now runs). When whether a hub runs, or what it runs, is unknown (`unknown`),
/// so is its build (`None`, never this twr's: a hub that runs may run another).
struct HubBuild {
    /// The running hub's version, else this twr's (also when unknown: only `label` shows it,
    /// and says it is not known).
    version: String,
    build: Option<String>,
    /// The runner protocol the hub's boxes must speak: the running hub's (`hub.info`
    /// `runner_protocol`; `None` from a hub older than that field: not known), else this twr's
    /// (no hub runs: a hub started now speaks it); `None` too when whether a hub runs is
    /// unknown.
    protocol: Option<u32>,
    /// `hub.info` of the running hub, when one answered.
    info: Option<Value>,
    /// Why whether a hub runs, or what it says, is unknown: something answers on the socket
    /// (or the socket cannot be reached for another reason than its absence) but no
    /// `hub.info` came. `None`: a hub answered, or none runs.
    unknown: Option<String>,
}

impl HubBuild {
    /// What a hub answering on `socket` says, without starting one.
    async fn of_running(socket: &Path) -> HubBuild {
        let asked = match Client::connect(socket).await {
            Ok(mut c) => match c.hello(json!({ "client": "cli" })).await {
                Ok(hello) => match super::check_workspace(&hello, socket) {
                    Ok(()) => c
                        .call("hub.info", Value::Null)
                        .await
                        .map(Some)
                        .map_err(|e| format!("{e:#}")),
                    Err(e) => Err(format!("{e:#}")),
                },
                Err(e) => Err(format!("{e:#}")),
            },
            // Nothing there: no hub runs.
            Err(e) if no_hub(&e) => Ok(None),
            Err(e) => Err(format!("{e:#}")),
        };
        match asked {
            Ok(Some(i)) => HubBuild::of_info(i),
            Ok(None) => HubBuild::own(None),
            Err(why) => HubBuild::own(Some(why)),
        }
    }

    /// This twr's version and build (what a hub started now runs), `unknown` as given: then
    /// the build is not known (`None`), nor the protocol.
    fn own(unknown: Option<String>) -> HubBuild {
        HubBuild {
            version: twr_core::version().to_string(),
            build: unknown
                .is_none()
                .then(twr_core::build_id)
                .flatten()
                .map(str::to_string),
            protocol: unknown
                .is_none()
                .then_some(twr_core::runner::PROTOCOL_VERSION),
            info: None,
            unknown,
        }
    }

    /// What `hub.info` `info` says.
    fn of_info(info: Value) -> HubBuild {
        HubBuild {
            version: str_of(&info["version"]).to_string(),
            build: info["build"].as_str().map(str::to_string),
            protocol: info["runner_protocol"]
                .as_u64()
                .and_then(|p| u32::try_from(p).ok()),
            info: Some(info),
            unknown: None,
        }
    }

    /// `twr <version>, build <id>`; what is said of a hub whose build is unknown.
    fn label(&self) -> String {
        if self.unknown.is_some() {
            return "the running hub's, which it did not say".to_string();
        }
        match &self.build {
            Some(b) => format!("twr {}, build {b}", self.version),
            None => format!("twr {}", self.version),
        }
    }

    /// The running hub's entry of box `name`, if it has one.
    fn box_entry(&self, name: &str) -> Option<&Value> {
        self.info.as_ref().and_then(|i| find_box(i, name))
    }
}

/// Whether connecting to the hub's socket failed with `e` because nothing listens there (no
/// socket file, or nobody accepting on it): no hub runs.
fn no_hub(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
    })
}

/// Box `name` in `hub.info` `info`.
fn find_box<'a>(info: &'a Value, name: &str) -> Option<&'a Value> {
    info["boxes"]
        .as_array()?
        .iter()
        .find(|b| b["name"].as_str() == Some(name))
}

/// What the probe of a box found, as said: `Linux/x86_64`.
fn target_label(probe: &ssh_box::Probe) -> String {
    let os = ssh_box::os_name(&probe.os).unwrap_or(&probe.os);
    let arch = ssh_box::arch_name(&probe.arch).unwrap_or(&probe.arch);
    format!("{os}/{arch}")
}

/// Whether the probed box has the hub's OS and CPU (the hub runs on this machine).
fn same_target(probe: &ssh_box::Probe) -> bool {
    ssh_box::os_name(&probe.os) == Some(std::env::consts::OS)
        && ssh_box::arch_name(&probe.arch) == Some(std::env::consts::ARCH)
}

/// The release target of a box of OS `os` and CPU `arch` (Rust's names), for twr's release
/// archives (`twr-<target>.tar.gz`).
fn release_target(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// What [`here`] got from the box: the exit code (`None`: a signal), stdout, and the last lines
/// of stderr (shown already, cleaned and cut: [`show_stderr`]).
pub(super) struct Here {
    pub(super) code: Option<i32>,
    pub(super) out: String,
    pub(super) err: Vec<String>,
}

/// A process [`here`] runs: ssh in this terminal's process group, or a command in a process
/// group of its own ([`ssh_box::Ssh`]).
enum HereProc {
    Terminal(tokio::process::Child),
    Group(ssh_box::Ssh),
}

impl HereProc {
    fn child(&mut self) -> &mut tokio::process::Child {
        match self {
            HereProc::Terminal(c) => c,
            HereProc::Group(s) => &mut s.child,
        }
    }

    /// Waits for it to end; a group's leftovers are killed then ([`ssh_box::Ssh::finish`]).
    async fn finish(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            HereProc::Terminal(c) => c.wait().await,
            HereProc::Group(s) => s.finish().await,
        }
    }
}

/// Runs `line` on the box through `link` from here, `input` its stdin, stdout captured (at most
/// [`ssh_box::SSH_OUTPUT_MAX`]: more is an error, and the process is killed), stderr shown on
/// this one ([`show_stderr`]). ssh runs in this terminal, in its process group, so it can ask
/// about a host key or for a passphrase (killing that group would kill twr); only ssh itself
/// is killed on an error or when this is dropped (a caller's timeout). A command runs as the
/// hub runs it ([`BoxLink::background`], [`ssh_box::Ssh`]): in a session and process group of
/// its own, without a terminal (nothing there may prompt), and its whole group is killed when
/// it ends, on an error and when this is dropped, so no helper of a wrapper outlives it.
pub(super) async fn here(link: &BoxLink, line: &str, input: Input) -> anyhow::Result<Here> {
    let program = Path::new(&link.program()).display().to_string();
    let refused = |why: String| match link {
        BoxLink::Ssh(ssh) => anyhow!("ssh {ssh:?}: {why}"),
        BoxLink::Command { .. } => anyhow!("the box's command ({program}): {why}"),
    };
    let mut cmd = if link.own_session_here() {
        link.background(line).map_err(refused)?
    } else {
        let mut c = tokio::process::Command::new(link.program());
        c.args(link.here_args(line).map_err(refused)?)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    };
    let bytes = match input {
        Input::None => {
            cmd.stdin(Stdio::null());
            None
        }
        Input::File(path) => {
            let f = std::fs::File::open(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            cmd.stdin(Stdio::from(f));
            None
        }
        Input::Bytes(b) => {
            cmd.stdin(Stdio::piped());
            Some(b)
        }
    };
    let mut proc = if link.own_session_here() {
        HereProc::Group(ssh_box::Ssh::spawn(cmd).with_context(|| format!("running {program}"))?)
    } else {
        HereProc::Terminal(cmd.spawn().with_context(|| format!("running {program}"))?)
    };
    let child = proc.child();
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    let via = link.via();
    let what = format!("{via} (stdout)");
    // The first to fail (more output than allowed) ends the others; an error drops `proc`,
    // which kills it (a command's whole group). The wait for its end runs alongside the
    // reads: a helper that kept its stdout or stderr open (a wrapper's) is killed with the
    // group once the leader exits, which ends the reads.
    let (out, err, (), status) = tokio::try_join!(
        async {
            ssh_box::read_capped(stdout, ssh_box::SSH_OUTPUT_MAX, &what)
                .await
                .map_err(|e| anyhow!("{e}"))
        },
        async { Ok(show_stderr(stderr, &via).await) },
        async {
            // A box that does not read it shows in its answer: not an error here. (Closed
            // when dropped.)
            if let (Some(mut stdin), Some(b)) = (stdin, bytes) {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(&b).await;
            }
            Ok(())
        },
        async {
            proc.finish()
                .await
                .with_context(|| format!("running {program}"))
        },
    )?;
    Ok(Here {
        code: status.code(),
        out: String::from_utf8_lossy(&out).into_owned(),
        err,
    })
}

/// Why `what` (`ssh to box gpu`, `installing twr on box gpu`) through `link` to box `name`
/// failed with exit `code` and stderr `err` (shown above): a command's own failure when it is
/// one twr knows (no such container, a stopped one, an engine it cannot reach:
/// [`ssh_box::command_failure`], with its fix), else where to look. (ssh's own messages, above,
/// say what went wrong: [`ssh_box::ssh_failure`] speaks of the hub's ssh, which never prompts.)
pub(super) fn failed_here(
    link: &BoxLink,
    name: &str,
    what: &str,
    code: Option<i32>,
    err: &[String],
) -> String {
    if let BoxLink::Command { .. } = link {
        if let Some(why) = ssh_box::command_failure(name, code, err) {
            return why;
        }
    }
    let code = code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
    let via = link.via();
    format!("{what} failed ({code}): see {via}'s message above")
}

/// The most lines of the box's stderr [`here`] shows; the rest is read and dropped.
const STDERR_LINES_SHOWN: usize = 200;

/// The most of those lines [`here`] keeps, the last ones (to say what failed).
const STDERR_LINES_KEPT: usize = 20;

/// Shows stderr `stderr` of the program `via` names (ssh's messages, a command's, and what the
/// box prints there) on this one, a line at a time: each line cut at
/// [`ssh_box::NOTE_MAX_BYTES`] and its control characters escaped ([`crate::term::clean`],
/// through `errln!`), at most [`STDERR_LINES_SHOWN`] lines. The last [`STDERR_LINES_KEPT`]
/// lines shown, cleaned. ssh asks its questions (a host key, a passphrase) on the terminal
/// itself (`/dev/tty`), not here, so they keep working.
async fn show_stderr(stderr: tokio::process::ChildStderr, via: &str) -> Vec<String> {
    show_lines(stderr, via, &mut |line: &str| errln!("{line}")).await
}

/// [`show_stderr`]'s work: the lines of `r` to `show`, each cut and cleaned, at most
/// [`STDERR_LINES_SHOWN`] (then one saying the rest is not shown); the last ones shown.
async fn show_lines<R: tokio::io::AsyncRead + Unpin>(
    r: R,
    via: &str,
    show: &mut dyn FnMut(&str),
) -> Vec<String> {
    let mut r = tokio::io::BufReader::new(r);
    let mut shown = 0;
    let mut kept = std::collections::VecDeque::new();
    while let Ok(Some(line)) = ssh_box::read_line_capped(&mut r, ssh_box::NOTE_MAX_BYTES).await {
        shown += 1;
        if shown <= STDERR_LINES_SHOWN {
            let line = crate::term::clean(&line).into_owned();
            show(&line);
            if kept.len() == STDERR_LINES_KEPT {
                kept.pop_front();
            }
            kept.push_back(line);
        } else if shown == STDERR_LINES_SHOWN + 1 {
            show(&format!("({via} printed more: not shown)"));
        }
    }
    kept.into()
}

/// How long the probe of `twr box add` and `twr box install` may take, ssh's questions
/// included.
const PROBE_WAIT: Duration = Duration::from_secs(120);

/// Probes box `name` through `link` ([`here`], within [`PROBE_WAIT`]; a command box's pipe
/// check with it) and says what it found. Its data directory too (an error without one).
async fn probe(name: &str, link: &BoxLink) -> anyhow::Result<(ssh_box::Probe, String)> {
    outln!("probing box {name} {}...", reached(link));
    let remote = ssh_box::sh_c(&ssh_box::probe_script_for(link));
    let probing = here(link, &remote, link.probe_input());
    let ran = tokio::time::timeout(PROBE_WAIT, probing)
        .await
        .map_err(|_| {
            anyhow!(
                "probing box {name} did not finish within {} s ({}); nothing was changed",
                PROBE_WAIT.as_secs(),
                unanswered(link)
            )
        })??;
    if ran.code != Some(0) {
        let what = format!("{} to box {name}", link.via());
        bail!("{}", failed_here(link, name, &what, ran.code, &ran.err));
    }
    let probe =
        ssh_box::parse_probe_for(&ran.out, link).with_context(|| format!("probing box {name}"))?;
    // A command that does not pass the hub's input on, or gives the box a terminal, cannot
    // carry the hub's connection.
    if let Some(why) = probe.pipe_refusal(name, link) {
        bail!("{why}");
    }
    // Nothing is installed into a builds tree another user could write to.
    if let Some(why) = probe.tree_refusal(name) {
        bail!("{why}");
    }
    let Some(data) = probe.data.clone() else {
        bail!(
            "box {name} has neither XDG_DATA_HOME nor HOME set to an absolute path: twr keeps \
             its builds and data under it"
        );
    };
    for u in &probe.unsafe_paths {
        errln!("twr: warning: box {name}: not trusted, left out: {u}");
    }
    let builds = if probe.has.is_empty() {
        "none".to_string()
    } else {
        probe.has.iter().cloned().collect::<Vec<_>>().join(", ")
    };
    outln!(
        "box {name}: {} ({} {}), data directory {data}, twr builds there: {builds}",
        target_label(&probe),
        probe.os,
        probe.arch
    );
    Ok((probe, data))
}

/// Checks that ssh reaches box `name` without prompting (as the hub's does): `None` when it
/// does, else why not.
async fn batch_check(name: &str, ssh: &str) -> Option<String> {
    let cmd = match ssh_box::ssh_command(ssh, "true") {
        Ok(c) => c,
        Err(why) => return Some(why),
    };
    let run = async {
        // Its process group killed when it ends, or when this is dropped (a timeout).
        let mut ssh = ssh_box::Ssh::spawn(cmd).map_err(|e| e.to_string())?;
        let (stdout, stderr) = (
            ssh.child.stdout.take().expect("piped"),
            ssh.child.stderr.take().expect("piped"),
        );
        // Bounded: more than allowed ends the check (and drops `ssh`, which kills it). Its end
        // is waited for alongside the reads (a helper holding its output dies with the group).
        let max = ssh_box::SSH_OUTPUT_MAX;
        let (_, err, status) = tokio::try_join!(
            ssh_box::read_capped(stdout, max, "ssh (stdout)"),
            ssh_box::read_capped(stderr, max, "ssh (stderr)"),
            async { ssh.finish().await.map_err(|e| e.to_string()) },
        )?;
        let lines: Vec<String> = String::from_utf8_lossy(&err)
            .lines()
            .map(str::to_string)
            .collect();
        if status.success() {
            return Ok(());
        }
        let why = (status.code() == Some(255))
            .then(|| ssh_box::ssh_failure(name, &lines))
            .flatten()
            .or_else(|| {
                lines
                    .iter()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim().to_string())
            })
            .unwrap_or_else(|| format!("ssh exited with {status}"));
        Err(why)
    };
    match tokio::time::timeout(BATCH_CHECK_TIMEOUT, run).await {
        Ok(Ok(())) => None,
        Ok(Err(why)) => Some(why),
        Err(_) => Some(format!(
            "it did not finish within {} s",
            BATCH_CHECK_TIMEOUT.as_secs()
        )),
    }
}

/// Asks `question` on the terminal (`consent`): whether the user said yes. `--yes` is a yes;
/// without a terminal, an error that names `--yes`.
fn confirm(consent: Consent<'_>, question: &str, refusal: &str) -> anyhow::Result<bool> {
    match consent {
        Consent::Yes => Ok(true),
        Consent::Ask(ask) => {
            let reply = ask(question)?;
            Ok(matches!(
                reply.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        }
        Consent::NoTerminal => bail!("{refusal} (no terminal to ask on): pass --yes"),
    }
}

/// Checks that box `name` with settings `cfg` can be added to `boxes.toml` (`file`) next to
/// `config.toml` (`config_file`): neither file has it, and the config with every box of
/// both validates. Returns the file's boxes with it added.
fn with_added_box(
    config_file: &Path,
    file: &Path,
    name: &str,
    cfg: BoxConfig,
) -> anyhow::Result<BoxesFile> {
    let base = Config::load(config_file)?;
    let mut boxes = BoxesFile::load(file)?;
    if base.boxes.contains_key(name) {
        bail!(
            "box {name} is configured in {} already",
            config_file.display()
        );
    }
    if boxes.boxes.contains_key(name) {
        bail!(
            "box {name} is in {} already (`twr box remove {name}` removes it)",
            file.display()
        );
    }
    boxes.boxes.insert(name.to_string(), cfg);
    check_merged(&base, &boxes, file)?;
    Ok(boxes)
}

/// Checks the config `base` with the boxes of `boxes` (from `file`) merged in, as the hub will
/// load them ([`Config::load_with_boxes`]).
fn check_merged(base: &Config, boxes: &BoxesFile, file: &Path) -> anyhow::Result<()> {
    let mut merged = base.clone();
    for (name, b) in &boxes.boxes {
        if merged.boxes.insert(name.clone(), b.clone()).is_some() {
            bail!(
                "box {name} would be in both config.toml and {}",
                file.display()
            );
        }
    }
    merged
        .validate()
        .with_context(|| format!("in {}", file.display()))
}

/// `twr box add` (see [`BoxCmd::Add`]).
async fn add(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    command: bool,
    words: Vec<String>,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("add boxes")?;
    default_workspace_only(paths, "add")?;
    twr_core::runner::validate_box_name(name)?;
    if name == twr_hub::hub::LOCAL_BOX {
        bail!("box name local is the hub's own box: name the remote box otherwise");
    }
    let cfg = if command {
        if words.is_empty() {
            bail!(
                "give the command that reaches the box: `twr box add {name} --command docker \
                 exec -i <container>`"
            );
        }
        BoxConfig {
            transport: Some(BoxTransport::Command),
            terminal_command: default_terminal(&words),
            command: Some(words),
            ..BoxConfig::default()
        }
    } else {
        let ssh = ssh_destination(&words);
        if ssh.is_empty() {
            bail!("give the box's ssh destination: `twr box add {name} me@host`");
        }
        BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(ssh),
            ..BoxConfig::default()
        }
    };
    let file = paths.boxes_file();
    if config.boxes.contains_key(name) {
        bail!("there is a box {name} already (`twr box status {name}` shows it)");
    }
    // Checked now so a bad destination or command fails before anything runs.
    with_added_box(&paths.config_file, &file, name, cfg.clone())?;
    let Some(link) = BoxLink::of(&cfg) else {
        bail!("box {name} has no ssh destination or command");
    };
    // Nobody to confirm with: refused before anything runs.
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if matches!(consent, Consent::NoTerminal) {
        bail!("not adding box {name} without confirmation (no terminal to ask on): pass --yes");
    }
    let hub = HubBuild::of_running(socket).await;
    if hub.box_entry(name).is_some() {
        bail!(
            "the running hub still has a box {name} (removed from the files since it started): \
             restart it first (`twr{w} hub stop`, then `twr{w}`)",
            w = workspace_hint()
        );
    }
    // A hub that does not say what it has: its `box.add` says whether it has another box of
    // that name (a conflict) once the box is written.
    if let Some(why) = &hub.unknown {
        errln!("twr: warning: the running hub did not say which boxes it has ({why})");
    }

    let (probe, data) = probe(name, &link).await?;
    // (A command ran as the hub runs it, without a terminal: it needs no other check.)
    if let BoxLink::Ssh(ssh) = &link {
        if let Some(why) = batch_check(name, ssh).await {
            errln!(
                "twr: warning: ssh to box {name} does not work without prompting: {why}\n\
                 The hub runs ssh without prompting (BatchMode), so it cannot connect until it \
                 can: load the key into an ssh-agent the hub can use (or start the hub from a \
                 shell that has one), or use a key without a passphrase. The box can still be \
                 added."
            );
        }
    }
    let same = same_target(&probe);
    outln!();
    match &link {
        BoxLink::Ssh(_) => outln!("Adding box {name} ({}):", link.label()),
        BoxLink::Command { .. } => outln!("Adding box {name} (command: {}):", link.label()),
    }
    match (&hub.build, same) {
        (Some(id), true) => {
            let path = ssh_box::build_path(&data, id);
            if probe.has.contains(id) {
                outln!(
                    "- The hub's build ({}) is on the box already, at {path}.",
                    hub.label()
                );
            } else {
                outln!(
                    "- The hub installs its own build ({}) on the box, at {path}.",
                    hub.label()
                );
            }
        }
        (None, true) => outln!(
            "- The hub installs its own build ({}) on the box, under {}.",
            hub.label(),
            ssh_box::builds_dir(&data)
        ),
        (_, false) => outln!(
            "- The box is {} and the hub {}/{}: the hub cannot install its own build there. After \
             adding it, run `twr box install {name}`.",
            target_label(&probe),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
    }
    if probe.os == "Darwin" {
        for l in mac_needs(name, &hub.version) {
            outln!("{l}");
        }
    }
    for l in reach_lines(name, &link) {
        outln!("{l}");
    }
    outln!("- [boxes.{name}] goes into {}.", file.display());
    if !confirm(
        consent,
        &format!("Add box {name}? [y/N] "),
        &format!("not adding box {name} without confirmation"),
    )? {
        outln!("box {name} not added");
        return Ok(());
    }

    {
        let _lock = BoxesFile::lock(&file)?;
        let boxes = with_added_box(&paths.config_file, &file, name, cfg)?;
        boxes.save(&file)?;
    }
    outln!("added [boxes.{name}] to {}", file.display());
    wait_for_hub(socket, paths).await?;
    let mut c = connect(socket).await?;
    // A hub that `twr box add` started (none ran) read boxes.toml, with the box just written,
    // as it started: `already`, when the box it has is the one written (the hub compares).
    let added = c
        .call("box.add", json!({ "name": name }))
        .await
        .with_context(|| {
            format!(
                "box {name} is in {}, but the hub did not add it",
                file.display()
            )
        })?;
    if added["already"] == true {
        outln!("the hub has box {name} from {} already", file.display());
    }
    match wait_online(&mut c, name, false, None).await? {
        Online::Yes(b) => {
            online_note(name, &b);
            project_hints(config, name);
        }
        Online::NeedsInstall(b) if !same => {
            outln!(
                "box {name} needs a twr build for {}: run `twr box install {name}`",
                target_label(&probe)
            );
            print_reason(&b);
            project_hints(config, name);
        }
        Online::NeedsInstall(b) => {
            print_reason(&b);
            bail!("the hub could not install twr on box {name}");
        }
    }
    Ok(())
}

/// What `twr box add` says of how the hub reaches box `name` through `link`, and for a command
/// box, what runs in the user's terminal (its terminal_command) or that nothing does.
fn reach_lines(name: &str, link: &BoxLink) -> Vec<String> {
    let daemon = "starts a twr box daemon there, which runs the box's threads and keeps them \
                  running while the hub is away.";
    match link {
        BoxLink::Ssh(_) => vec![format!(
            "- The hub connects to the box over ssh and {daemon}"
        )],
        BoxLink::Command { terminal, .. } => vec![
            format!(
                "- The hub runs `{} sh -c ...` itself, from its own environment, and {daemon}",
                link.label()
            ),
            match terminal {
                Some(t) => format!(
                    "- For `twr box login` and `twr box project --clone`, twr runs `{}` in your \
                     terminal (terminal_command).",
                    argv_label(t)
                ),
                None => format!(
                    "- `twr box login` and `twr box project --clone` need a terminal_command in \
                     [boxes.{name}] (the command with `-t`); without one they are refused."
                ),
            },
        ],
    }
}

/// What `twr box add` says a Mac (box `name`) needs, the hub running twr `version`: someone
/// logged in at its screen (the box daemon runs in that GUI session, where Claude Code can
/// read its keychain login), a twr build of the hub's commit for macOS, and the agents logged
/// in at the Mac itself.
fn mac_needs(name: &str, version: &str) -> Vec<String> {
    let build = match dev_commit(version) {
        Some(commit) => format!(
            "  - a twr build of the hub's commit ({commit}) for macOS: the hub runs a development \
             build, so build it on the Mac (docs/tools/dev-install.sh in a checkout of that \
             commit), then `twr box install {name} --from-box <its absolute path on the Mac>`;"
        ),
        None => format!(
            "  - a twr build for macOS: `twr box install {name}` downloads release v{version}'s \
             for the Mac and installs it there;"
        ),
    };
    vec![
        "- The box is a Mac. It needs:".to_string(),
        "  - someone logged in at the Mac's screen: the box daemon runs as a launchd job in \
         that GUI session, where Claude Code can read its keychain login (from ssh alone it \
         can't). Turn on automatic login if the Mac may restart with no one there;"
            .to_string(),
        build,
        format!(
            "  - its agents logged in at the Mac itself (Claude Code keeps its login in the \
             keychain); `twr box doctor {name}` checks all of this once the box is added."
        ),
    ]
}

/// What `twr box add` says of the projects configured for box `name` (`[projects.<p>] box`):
/// how to find each on the box and record where it is.
fn project_hints(config: &Config, name: &str) {
    let lines = project_hint_lines(config, name);
    if lines.is_empty() {
        return;
    }
    outln!(
        "projects configured for box {name}: find each one's repository on the box and record \
         it with"
    );
    for l in lines {
        outln!("  {l}");
    }
}

/// [`project_hints`]'s commands.
fn project_hint_lines(config: &Config, name: &str) -> Vec<String> {
    use twr_core::paths::sh_word;
    config
        .projects
        .iter()
        .filter(|(_, p)| p.box_name.as_deref() == Some(name))
        .map(|(p, _)| format!("twr box project {} {}", sh_word(name), sh_word(p)))
        .collect()
}

/// How waiting for a box ended.
enum Online {
    /// It is online (its `hub.info` entry).
    Yes(Value),
    /// It needs an install (its entry).
    NeedsInstall(Value),
}

/// Waits for box `name` to come online (at most [`ONLINE_WAIT`], looking every
/// [`ONLINE_POLL`]), printing each new state and last error once. `after_install`: the hub was
/// just asked to try again, so a `needs_install` counts only once it has shown another state
/// or after [`STALE_NEEDS_INSTALL`]. `want`: the build just installed, which the box must run
/// (`running_build`) to count as online; while it runs another one (`switch_pending`, then
/// `switching`, as the hub switches it once quiet) the wait goes on, at most [`SWITCH_WAIT`],
/// printing the box's notes that come meanwhile (the relay's, as it stops and starts the
/// daemon). An error on timeout or when the hub's box key is unusable (state `error`), after
/// printing the state, the last error and the notes.
async fn wait_online(
    c: &mut Client,
    name: &str,
    after_install: bool,
    want: Option<&str>,
) -> anyhow::Result<Online> {
    let start = tokio::time::Instant::now();
    let limit = if want.is_some() {
        SWITCH_WAIT
    } else {
        ONLINE_WAIT
    };
    let mut shown_state = String::new();
    let mut shown_error: Option<String> = None;
    let mut shown_running: Option<String> = None;
    let mut moved = !after_install;
    // (The notes until the first look: only those that follow are news.)
    let mut notes: Option<Vec<String>> = None;
    let last = loop {
        let info = c.call("hub.info", Value::Null).await?;
        let Some(b) = find_box(&info, name).cloned() else {
            bail!("the hub has no box {name}");
        };
        if want.is_some() {
            let now = notes_of(&b);
            if let Some(before) = &notes {
                for n in new_notes(before, &now) {
                    outln!("  {n}");
                }
            }
            notes = Some(now);
        }
        let state = str_of(&b["state"]).to_string();
        if state != shown_state {
            outln!("box {name}: {state}");
            shown_state = state.clone();
        }
        let error = b["last_error"].as_str().map(str::to_string);
        if error.is_some() && error != shown_error {
            outln!("  last error: {}", error.as_deref().unwrap_or_default());
            shown_error = error;
        }
        moved |= state != "needs_install";
        let running = b["running_build"].as_str().map(str::to_string);
        let on_want = want.is_none_or(|w| running.as_deref() == Some(w));
        if state == "online" && !on_want && running.is_some() && running != shown_running {
            outln!(
                "box {name} runs build {}, not build {}: {}",
                running.as_deref().unwrap_or_default(),
                want.unwrap_or_default(),
                if b["switch_pending"] == true {
                    "switch pending (the hub switches it once it has been quiet for 2 minutes)"
                } else {
                    "waiting for the hub to switch it"
                }
            );
            shown_running = running.clone();
        }
        match state.as_str() {
            "online" if on_want => return Ok(Online::Yes(b)),
            "needs_install" if moved || start.elapsed() >= STALE_NEEDS_INSTALL => {
                return Ok(Online::NeedsInstall(b))
            }
            "error" => {
                print_reason(&b);
                bail!("the hub cannot connect to box {name}");
            }
            _ => {}
        }
        if start.elapsed() >= limit {
            break b;
        }
        tokio::time::sleep(ONLINE_POLL).await;
    };
    print_reason(&last);
    if let (Some(w), Some(running)) = (want, last["running_build"].as_str()) {
        bail!(
            "box {name} still runs build {running} after {} s: the hub switches it to build {w} \
             once it has been quiet for 2 minutes (no turn running, nothing queued or waiting \
             on an answer); `twr box status {name}` shows how it goes",
            limit.as_secs()
        );
    }
    bail!(
        "box {name} is not online after {} s (it is {}); `twr box status {name}` shows how it \
         goes",
        limit.as_secs(),
        str_of(&last["state"])
    )
}

/// Prints box entry `b`'s state, last error and notes (why it is not online).
fn print_reason(b: &Value) {
    errln!("state:      {}", str_of(&b["state"]));
    if let Some(e) = b["last_error"].as_str() {
        errln!("last error: {e}");
    }
    let notes = b["notes"].as_array().cloned().unwrap_or_default();
    if !notes.is_empty() {
        errln!("notes (newest last):");
        for n in &notes {
            errln!("  {}", str_of(n));
        }
    }
}

/// What `add` and `install` print once box `name` (entry `b`) is online.
fn online_note(name: &str, b: &Value) {
    let version = b["version"].as_str().unwrap_or("of an unknown version");
    let build = b["build"]
        .as_str()
        .map(|id| format!(", build {id}"))
        .unwrap_or_default();
    outln!("box {name} is online: twr {version}{build}");
    outln!("start a thread on it: twr thread new <project> --box {name}");
}

/// `twr box list`.
async fn list(socket: &Path, json: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let info = c.call("hub.info", Value::Null).await?;
    let boxes = info["boxes"].as_array().cloned().unwrap_or_default();
    if json {
        outln!("{}", serde_json::to_string_pretty(&boxes)?);
        return Ok(());
    }
    if boxes.is_empty() {
        outln!("no remote boxes (`twr box add <name> <ssh destination>` adds one)");
        return Ok(());
    }
    out!("{}", list_text(&boxes));
    Ok(())
}

/// `twr box list`'s table of `hub.info` boxes `boxes`.
fn list_text(boxes: &[Value]) -> String {
    let rows: Vec<[String; 6]> = boxes
        .iter()
        .map(|b| {
            [
                str_of(&b["name"]).to_string(),
                str_of(&b["transport"]).to_string(),
                str_of(&b["state"]).to_string(),
                b["version"].as_str().unwrap_or("-").to_string(),
                os_arch(b).unwrap_or_else(|| "-".into()),
                agents_cell(&box_agents(b)),
            ]
        })
        .collect();
    table(
        &["NAME", "TRANSPORT", "STATE", "VERSION", "OS/ARCH", "AGENTS"],
        &rows,
    )
}

/// The agents of a `hub.info` box entry (or of `box.agents`) `b`, as the box last reported
/// them: none when it has not.
fn box_agents(b: &Value) -> Vec<twr_core::runner::AgentInfo> {
    serde_json::from_value(b["agents"].clone()).unwrap_or_default()
}

/// The command an agent runtime is known by (`claude-code`: `claude`), else the runtime.
fn agent_name(runtime: &str) -> &str {
    twr_adapters::resolve::BUILTIN_AGENTS
        .iter()
        .find(|(r, ..)| *r == runtime)
        .map_or(runtime, |(_, command, _)| command)
}

/// `twr box list`'s AGENTS cell: each agent's command and a mark (`claude ✓ codex ✗ grok ?`:
/// logged in, logged out, not known; `-`: not installed), or `-` when the box reported none.
fn agents_cell(agents: &[twr_core::runner::AgentInfo]) -> String {
    if agents.is_empty() {
        return "-".into();
    }
    agents
        .iter()
        .map(|a| format!("{} {}", agent_name(&a.runtime), login_mark(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// [`agents_cell`]'s mark of agent `a`.
fn login_mark(a: &twr_core::runner::AgentInfo) -> &'static str {
    use twr_core::runner::AgentLogin;
    match (a.found, a.login) {
        (false, _) => "-",
        (true, AgentLogin::LoggedIn) => "✓",
        (true, AgentLogin::LoggedOut) => "✗",
        (true, AgentLogin::Unknown) => "?",
    }
}

/// Agent `a` in a sentence (`logged in (claude.ai), 2.1.3 (Claude Code), at /usr/bin/claude`;
/// `not installed (not found on PATH)`), without its name. Only what the probe keeps: no
/// account detail.
fn agent_said(a: &twr_core::runner::AgentInfo) -> String {
    use twr_core::runner::AgentLogin;
    if !a.found {
        return match &a.note {
            Some(n) => format!("not installed ({n})"),
            None => "not installed".to_string(),
        };
    }
    let mut parts = vec![match (a.login, &a.method) {
        (AgentLogin::LoggedIn, Some(m)) => format!("logged in ({m})"),
        (AgentLogin::LoggedIn, None) => "logged in".to_string(),
        (AgentLogin::LoggedOut, _) => "logged out".to_string(),
        (AgentLogin::Unknown, _) => match &a.note {
            Some(n) => format!("login not known ({n})"),
            None => "login not known".to_string(),
        },
    }];
    parts.extend(a.version.clone());
    parts.extend(a.path.as_ref().map(|p| format!("at {p}")));
    parts.join(", ")
}

/// A box entry's OS and CPU as Rust names them where twr knows them (`linux/x86_64`).
fn os_arch(b: &Value) -> Option<String> {
    let os = b["os"].as_str()?;
    let arch = b["arch"].as_str().unwrap_or("?");
    Some(format!(
        "{}/{}",
        ssh_box::os_name(os).unwrap_or(os),
        ssh_box::arch_name(arch).unwrap_or(arch)
    ))
}

/// `twr box status`, `config` the loaded one (whether a token box is a dial-in one).
async fn status(config: &Config, socket: &Path, name: &str, json: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let info = c.call("hub.info", Value::Null).await?;
    let Some(b) = find_box(&info, name) else {
        bail!("the hub has no box {name} (`twr box list` lists its boxes)");
    };
    if json {
        outln!("{}", serde_json::to_string_pretty(b)?);
    } else {
        let dial_in = config.boxes.get(name).is_some_and(is_dial_in);
        out!("{}", status_text(b, dial_in));
    }
    Ok(())
}

/// Whether box `cfg` is a dial-in box: a token box without `ssh`, which twr can't reach (its
/// runner connects to the hub; `ssh` would only be for `twr attach` and the checks).
pub(super) fn is_dial_in(cfg: &BoxConfig) -> bool {
    cfg.transport() == BoxTransport::Token && BoxLink::of(cfg).is_none()
}

/// `twr box status`'s text for box entry `b` (`dial_in`: a token box without `ssh`).
fn status_text(b: &Value, dial_in: bool) -> String {
    let mut out = String::new();
    let mut line = |k: &str, v: &str| out.push_str(&format!("{k:<15} {v}\n"));
    line("box:", str_of(&b["name"]));
    line(
        "transport:",
        match str_of(&b["transport"]) {
            "ssh" => "ssh (the hub connects to the box over ssh)",
            "command" => "command (the hub reaches the box through its command, a container's)",
            _ if dial_in => "token, dial-in (its runner connects to the hub's listener)",
            _ => "token (its runner connects to the hub's listener, [hub] listen)",
        },
    );
    line("state:", str_of(&b["state"]));
    line("online:", if b["online"] == true { "yes" } else { "no" });
    line(
        "version:",
        b["version"].as_str().unwrap_or("- (never connected)"),
    );
    if let Some(id) = b["build"].as_str() {
        line("build:", &format!("{id} (launched by the hub)"));
    }
    if let Some(id) = b["running_build"]
        .as_str()
        .filter(|r| Some(*r) != b["build"].as_str())
    {
        line(
            "running build:",
            &format!("{id} (what the box's daemon runs)"),
        );
    }
    if b["switch_pending"] == true {
        line(
            "switch:",
            "pending: the box runs another build of the same protocol than the launched or \
             newly installed one; the hub switches it once it has been quiet for 2 minutes",
        );
    }
    if let Some(oa) = os_arch(b) {
        line("os/arch:", &oa);
    }
    // (Only while the box is connected is it its daemon's word: the verdict says so.)
    if b["launch"].is_string() {
        let (_, said, fix) = doctor::launch_verdict(str_of(&b["name"]), b, b["os"] == "Darwin");
        match fix {
            Some(fix) => line("launch:", &format!("{said}: {fix}")),
            None => line("launch:", &said),
        }
    }
    if let Some(e) = b["last_error"].as_str() {
        line("last error:", e);
    }
    if let Some(r) = b["refused"].as_object() {
        line(
            "refused:",
            &format!("{}: {}", super::trash_time(&r["at"]), str_of(&r["reason"])),
        );
    }
    let agents = box_agents(b);
    if agents.is_empty() {
        line("agents:", "- (not reported yet)");
    } else {
        out.push_str("agents:         (as the box last reported them)\n");
        for a in &agents {
            out.push_str(&format!(
                "  {}: {}\n",
                agent_name(&a.runtime),
                agent_said(a)
            ));
        }
    }
    let notes = b["notes"].as_array().cloned().unwrap_or_default();
    if !notes.is_empty() {
        out.push_str("notes:          (newest last)\n");
        for n in &notes {
            out.push_str(&format!("  {}\n", str_of(n)));
        }
    }
    out
}

/// What `twr box install` installs on a box of another OS or CPU, when not the release's.
enum InstallFrom {
    /// A file here, uploaded (`--binary`).
    Binary(PathBuf),
    /// A file on the box, by its absolute path there (`--from-box`).
    Box(String),
}

/// `twr box install` (see [`BoxCmd::Install`]).
async fn install(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    from: Option<InstallFrom>,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("install twr on boxes")?;
    default_workspace_only(paths, "install")?;
    twr_core::runner::validate_box_name(name)?;
    let Some(loaded) = config.boxes.get(name) else {
        bail!(
            "there is no box {name} in config.toml or boxes.toml (`twr box add {name} <ssh \
             destination>` adds one)"
        );
    };
    let file = paths.boxes_file();
    // One snapshot of the box, read from its file now, drives both the probe below and the
    // record's comparison.
    let (cfg, in_boxes_file) = box_snapshot(&paths.config_file, &file, name, loaded)?;
    let cfg = &cfg;
    let link = cfg.transport().dialed().then(|| BoxLink::of(cfg)).flatten();
    let Some(link) = link else {
        let hub = HubBuild::of_running(socket).await;
        bail!(
            "box {name} is a token box: its runner connects to the hub, and twr can't reach it \
             to install twr. On the box: {}; then restart its runner ({})",
            token::update_hint(&hub.version),
            token::restart_hint(name)
        );
    };
    let link = &link;
    match &from {
        // Read now: a path that is wrong fails before anything runs on the box.
        Some(InstallFrom::Binary(b)) => {
            std::fs::metadata(b).with_context(|| format!("reading {}", b.display()))?;
        }
        Some(InstallFrom::Box(p)) => check_box_path(name, p)?,
        None => {}
    }
    let (probe, data) = probe(name, link).await?;
    if same_target(&probe) {
        if let Some(f) = &from {
            bail!(
                "box {name} is {} like the hub: the hub runs its own build there, and installs \
                 it itself (run `twr box install {name}` without {})",
                target_label(&probe),
                match f {
                    InstallFrom::Binary(_) => "--binary",
                    InstallFrom::Box(_) => "--from-box",
                }
            );
        }
        outln!("box {name} has the hub's OS and CPU: the hub installs its own build there");
        return reconnect(socket, paths, name, None).await;
    }
    let Some(target) = ssh_box::os_name(&probe.os)
        .zip(ssh_box::arch_name(&probe.arch))
        .and_then(|(os, arch)| release_target(os, arch))
    else {
        bail!(
            "twr has no build for box {name}'s OS and CPU ({} {})",
            probe.os,
            probe.arch
        );
    };
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if matches!(consent, Consent::NoTerminal) {
        bail!(
            "not installing twr on box {name} without confirmation (no terminal to ask on): \
             pass --yes"
        );
    }
    // The running hub's version, else this twr's (a hub started later runs it).
    let hub = HubBuild::of_running(socket).await;
    if let Some(why) = &hub.unknown {
        bail!(
            "the hub on {} did not say which twr it runs ({why}): the build for box {name} must \
             be of its version",
            socket.display()
        );
    }
    let reference = hub_reference(paths, name)?;
    let id = match &from {
        Some(InstallFrom::Binary(path)) => {
            install_binary(name, link, &data, path, &hub, consent, reference.as_deref()).await?
        }
        Some(InstallFrom::Box(path)) => {
            if let Some(why) = home_here(path, &data) {
                errln!("twr: warning: {why}");
            }
            install_from_box(name, link, &data, path, &hub, consent, reference.as_deref()).await?
        }
        None => {
            install_release(
                name,
                link,
                &data,
                target,
                &hub,
                consent,
                reference.as_deref(),
            )
            .await?
        }
    };
    let Some(id) = id else {
        outln!("nothing installed on box {name}");
        return Ok(());
    };
    if !in_boxes_file {
        outln!(
            "box {name} is configured in {}, which twr does not edit: add this line to its \
             [boxes.{name}] there, then restart the hub (`twr hub stop`, then `twr`) so it runs \
             that build:",
            paths.config_file.display()
        );
        outln!("build = \"{id}\"");
        return Ok(());
    }
    record_installed_build(&paths.config_file, &file, name, cfg, &id)?;
    outln!(
        "recorded build = \"{id}\" for box {name} in {}",
        file.display()
    );
    reconnect(socket, paths, name, Some(&id)).await
}

/// Box `name` as its file has it now (boxes.toml, else config.toml): the one snapshot a
/// command that changes it (`twr box install`, `twr box project`) works from, what it runs on
/// the box and its write both, and whether it is in boxes.toml (else in config.toml, which twr
/// does not edit). Refused when the box's connection settings there
/// ([`BoxConfig::connection`]) are not those of `loaded` (the box as the command's config was
/// loaded, earlier), or the box is in neither file now.
pub(super) fn box_snapshot(
    config_file: &Path,
    file: &Path,
    name: &str,
    loaded: &BoxConfig,
) -> anyhow::Result<(BoxConfig, bool)> {
    let (start, in_file) = match BoxesFile::load(file)?.boxes.get(name) {
        Some(b) => (b.clone(), true),
        None => match Config::load(config_file)?.boxes.get(name) {
            Some(b) => (b.clone(), false),
            None => bail!(
                "box {name} was removed from {} meanwhile: nothing was changed",
                file.display()
            ),
        },
    };
    if start.connection() != loaded.connection() {
        bail!(
            "box {name} changed in {} while twr read its config (its ssh destination, command \
             or settings): nothing was changed; run the command again",
            if in_file { file } else { config_file }.display()
        );
    }
    Ok((start, in_file))
}

/// Records build `id`, just installed on box `name` as configured by `used`, as the
/// box's `build` in boxes.toml (`file`, next to `config_file`), under the file's lock: only
/// while the box there is still the one installed to (its connection settings,
/// [`BoxConfig::connection`], as in `used`: a box removed and added again meanwhile, with
/// another destination say, would get a build of another machine; a project recorded
/// meanwhile, `twr box project`, is no other box). Refused otherwise, with the file left as it
/// is.
fn record_installed_build(
    config_file: &Path,
    file: &Path,
    name: &str,
    used: &BoxConfig,
    id: &str,
) -> anyhow::Result<()> {
    let _lock = BoxesFile::lock(file)?;
    let mut boxes = BoxesFile::load(file)?;
    let Some(b) = boxes.boxes.get_mut(name) else {
        bail!(
            "box {name} was removed from {} meanwhile; build {id} stays on the box",
            file.display()
        );
    };
    if b.connection() != used.connection() {
        bail!(
            "box {name} changed in {} while twr was installing on it (its ssh destination, \
             command or settings): build {id} was not recorded and stays on the box it went to; \
             run `twr box install {name}` again",
            file.display()
        );
    }
    b.build = Some(id.to_string());
    check_merged(&Config::load(config_file)?, &boxes, file)?;
    boxes.save(file)
}

/// Asks the hub to try box `name` again now (`box.install`, starting the hub when none runs)
/// and waits for it to come online, on build `want` when given (the build just installed and
/// recorded: a box online on another one is switched to it by the hub once quiet).
async fn reconnect(
    socket: &Path,
    paths: &Paths,
    name: &str,
    want: Option<&str>,
) -> anyhow::Result<()> {
    wait_for_hub(socket, paths).await?;
    let mut c = connect(socket).await?;
    if let Err(e) = c.call("box.install", json!({ "name": name })).await {
        let not_found = e
            .downcast_ref::<RpcError>()
            .is_some_and(|r| r.code == codes::NOT_FOUND);
        if not_found {
            bail!(
                "the running hub has no box {name}: add it to the running hub with `twr box add`, \
                 or restart the hub (`twr hub stop`, then `twr`)"
            );
        }
        return Err(e);
    }
    match wait_online(&mut c, name, true, want).await? {
        Online::Yes(b) => {
            online_note(name, &b);
            Ok(())
        }
        Online::NeedsInstall(b) => {
            print_reason(&b);
            bail!("box {name} still needs an install");
        }
    }
}

/// `twr box restart` (see [`BoxCmd::Restart`]): asks first, then `box.restart` (starting the
/// hub when none runs), and waits for the box to come back on a newer connection
/// ([`restarted`]), saying how its new daemon was started.
async fn restart(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("restart box daemons")?;
    default_workspace_only(paths, "restart")?;
    twr_core::runner::validate_box_name(name)?;
    let Some(cfg) = config.boxes.get(name) else {
        bail!("there is no box {name} in config.toml or boxes.toml (`twr box list` lists them)");
    };
    if !cfg.transport().dialed() {
        bail!(
            "box {name} is a token box: its runner connects to the hub; restart that `twr \
             runner` on the box yourself: {} (as `twr runner install-token` set it up)",
            token::restart_hint(name)
        );
    }
    let mut ask = ask_on_stderr;
    let consent = consent(yes, &mut ask);
    if matches!(consent, Consent::NoTerminal) {
        bail!(
            "not restarting box {name}'s daemon without confirmation (no terminal to ask on): \
             pass --yes"
        );
    }
    outln!(
        "This stops box {name}'s daemon and the agents it runs, and starts a new one. Its \
         threads' runtimes end as when a box's runner restarts: each such thread shows a crash, \
         a turn running then is cut short, and the thread's next prompt resumes its session."
    );
    if !confirm(
        consent,
        &format!("Restart box {name}'s daemon? [y/N] "),
        &format!("not restarting box {name}'s daemon without confirmation"),
    )? {
        outln!("box {name} not restarted");
        return Ok(());
    }
    wait_for_hub(socket, paths).await?;
    let mut c = connect(socket).await?;
    // (The notes until now: only those that follow are news.)
    let info = c.call("hub.info", Value::Null).await?;
    let before = find_box(&info, name);
    let watch = RestartWait::new(before);
    let before = before.map(notes_of).unwrap_or_default();
    let asked = c.call("box.restart", json!({ "box": name })).await;
    let replaced = match asked {
        Ok(v) => v["epoch"].as_u64(),
        Err(e) => {
            let not_found = e
                .downcast_ref::<RpcError>()
                .is_some_and(|r| r.code == codes::NOT_FOUND);
            if not_found {
                bail!(
                    "the running hub has no box {name}: add it with `twr box add`, or restart \
                     the hub (`twr hub stop`, then `twr`)"
                );
            }
            return Err(e);
        }
    };
    outln!(
        "restarting box {name}'s daemon: the hub connects again, and the relay on the box stops \
         the daemon (a repository change it runs is finished first: up to {} s) and starts a new \
         one. Waiting up to {} minutes; the box's notes follow.",
        twr_runner::relay::STOP_TIMEOUT.as_secs(),
        RESTART_WAIT.as_secs().div_ceil(60)
    );
    let b = wait_restarted(&mut c, name, replaced, before, watch).await?;
    let version = b["version"].as_str().unwrap_or("of an unknown version");
    outln!("box {name} is back online with a new daemon: twr {version}");
    let (_, said, fix) = doctor::launch_verdict(name, &b, b["os"] == "Darwin");
    match fix {
        Some(fix) => outln!("{said}: {fix}"),
        None => outln!("{said}"),
    }
    Ok(())
}

/// Whether box entry `b` of `hub.info` is online on a connection made after a restart asked
/// for while connection `replaced` was up (`None`: none was): none pending, a newer epoch.
fn restarted(b: &Value, replaced: Option<u64>) -> bool {
    b["state"] == "online"
        && b["restart_pending"] != true
        && b["epoch"]
            .as_u64()
            .is_some_and(|e| replaced.is_none_or(|r| e > r))
}

/// Whether the `error` or `needs_install` that `twr box restart` sees is the outcome of the
/// restart it asked for, not the state from before it (a box repaired since, say, which the
/// restart is to bring back): only once the hub has shown another state since the request
/// (it wakes the supervisor, whose attempt is `connecting` first), or another last error than
/// the one before it, or after [`STALE_NEEDS_INSTALL`] (an attempt that failed between two
/// looks).
struct RestartWait {
    /// The box's last error before the request.
    error_before: Option<String>,
    /// The hub has shown that it went on since the request.
    moved: bool,
}

impl RestartWait {
    /// Before a restart is asked for, with box entry `before` of `hub.info` (`None`: the hub
    /// has none).
    fn new(before: Option<&Value>) -> RestartWait {
        RestartWait {
            error_before: before.and_then(|b| b["last_error"].as_str().map(str::to_string)),
            moved: false,
        }
    }

    /// Whether box entry `b`, seen `elapsed` after the restart was asked for, ends the wait
    /// with a failure: it is `error` or `needs_install`, as the outcome of the restart.
    fn failed(&mut self, b: &Value, elapsed: Duration) -> bool {
        let failure = matches!(str_of(&b["state"]), "error" | "needs_install");
        self.moved |= !failure || b["last_error"].as_str() != self.error_before.as_deref();
        failure && (self.moved || elapsed >= STALE_NEEDS_INSTALL)
    }
}

/// The notes of box entry `b` of `hub.info`, oldest first.
fn notes_of(b: &Value) -> Vec<String> {
    b["notes"]
        .as_array()
        .map(|n| n.iter().map(|n| str_of(n).to_string()).collect())
        .unwrap_or_default()
}

/// Of a box's notes `now`, those that came after `before` (the notes when last looked at):
/// the hub keeps only the latest, so `now` starts with the end of `before` (as much of it as
/// is still kept), and what follows that is new. All of `now` when nothing of `before` is left.
fn new_notes<'a>(before: &[String], now: &'a [String]) -> &'a [String] {
    let kept = (0..=before.len().min(now.len()))
        .rev()
        .find(|&k| before[before.len() - k..] == now[..k])
        .unwrap_or(0);
    &now[kept..]
}

/// Waits for box `name` to be [`restarted`] (at most [`RESTART_WAIT`], looking every
/// [`ONLINE_POLL`]), printing each new state and last error once, and the box's notes that
/// follow `notes` (those of the relay as it stops and starts the daemon): its entry. An error
/// on timeout, when the hub cannot connect to it (`error`) or it needs a twr build
/// (`needs_install`) as the outcome of the restart (`watch`), after printing the state, the
/// last error and the notes.
async fn wait_restarted(
    c: &mut Client,
    name: &str,
    replaced: Option<u64>,
    mut notes: Vec<String>,
    mut watch: RestartWait,
) -> anyhow::Result<Value> {
    let start = tokio::time::Instant::now();
    let mut shown_state = String::new();
    let mut shown_error: Option<String> = None;
    let last = loop {
        let info = c.call("hub.info", Value::Null).await?;
        let Some(b) = find_box(&info, name).cloned() else {
            bail!("the hub has no box {name}");
        };
        let now = notes_of(&b);
        for n in new_notes(&notes, &now) {
            outln!("  {n}");
        }
        notes = now;
        if restarted(&b, replaced) {
            return Ok(b);
        }
        // (Still on the connection being replaced: not news.)
        let state = match str_of(&b["state"]) {
            "online" => "restarting",
            s => s,
        }
        .to_string();
        if state != shown_state {
            outln!("box {name}: {state}");
            shown_state = state.clone();
        }
        let error = b["last_error"].as_str().map(str::to_string);
        if error.is_some() && error != shown_error {
            outln!("  last error: {}", error.as_deref().unwrap_or_default());
            shown_error = error;
        }
        if watch.failed(&b, start.elapsed()) {
            print_reason(&b);
            if state == "needs_install" {
                bail!("box {name} needs a twr build: `twr box install {name}`");
            }
            bail!("the hub cannot connect to box {name}");
        }
        if start.elapsed() >= RESTART_WAIT {
            break b;
        }
        tokio::time::sleep(ONLINE_POLL).await;
    };
    print_reason(&last);
    bail!(
        "box {name} is not back online after {} s (it is {}); `twr box status {name}` shows how \
         it goes",
        RESTART_WAIT.as_secs(),
        str_of(&last["state"])
    )
}

/// The hub's builds reference for box `name` ([`ssh_box::ref_name`]), when the hub has its box
/// key: an install adds the build to it, so that no hub's prune removes the build before this
/// hub runs it (a switch may wait for the box to be quiet).
fn hub_reference(paths: &Paths, name: &str) -> anyhow::Result<Option<String>> {
    let key_file = paths.data_dir.join(ssh_box::BOX_KEY_FILE);
    if !key_file.exists() {
        return Ok(None);
    }
    let key = twr_core::sealed::HubKey::load_or_create(&key_file)?;
    Ok(Some(ssh_box::ref_name(&key.id(), name)))
}

/// Uploads `path` (a twr built for box `name`'s OS and CPU) to the box under `data` and returns
/// its build id, once the user agreed (`consent`); `None` when they did not. The build is
/// added to the hub's builds reference `reference` ([`hub_reference`]).
#[allow(clippy::too_many_arguments)]
async fn install_binary(
    name: &str,
    link: &BoxLink,
    data: &str,
    path: &Path,
    hub: &HubBuild,
    consent: Consent<'_>,
    reference: Option<&str>,
) -> anyhow::Result<Option<String>> {
    let id = twr_core::build_id_of(path).with_context(|| format!("reading {}", path.display()))?;
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    outln!(
        "{} is build {id} ({}); it goes to {} on box {name}, and the hub runs it there",
        path.display(),
        human_bytes(size),
        ssh_box::build_path(data, &id)
    );
    if !confirm(
        consent,
        &format!("Install it on box {name}? [y/N] "),
        &format!("not installing twr on box {name} without confirmation"),
    )? {
        return Ok(None);
    }
    let tag = upload_tag()?;
    let mut script = ssh_box::install_script(data, &id, size, &tag);
    if let Some(r) = reference {
        // (Run only once the script got to its end: installed.)
        script.push_str(&ssh_box::install_ref_script(data, r, &id));
    }
    let ran = here(
        link,
        &ssh_box::sh_c(&script),
        Input::File(path.to_path_buf()),
    )
    .await?;
    let (answer, version) = ssh_box::upload_answer(&ran.out);
    upload_result(link, name, data, &ran, answer.as_deref())?;
    version_note(name, version.as_deref(), hub);
    Ok(Some(id))
}

/// Refuses `--from-box` path `path` for box `name` unless it is absolute, printable and on one
/// line: the box's script quotes it, and the user reads it back.
fn check_box_path(name: &str, path: &str) -> anyhow::Result<()> {
    if path.chars().any(char::is_control) {
        bail!("--from-box: the path holds a control character");
    }
    if !path.starts_with('/') {
        bail!(
            "--from-box {path:?}: give the twr executable's absolute path on box {name}: on the \
             box, `command -v twr` prints it (or `echo ~/.local/bin/twr`, for the one \
             docs/tools/dev-install.sh installs)"
        );
    }
    Ok(())
}

/// Why `--from-box` path `path` may be a path of this machine rather than of the box, whose
/// data directory is `data`: it is under this machine's home (a `~` the shell expanded here,
/// say) and the box's data directory is not.
fn home_here(path: &str, data: &str) -> Option<String> {
    let home = std::env::var("HOME").ok().filter(|h| h.starts_with('/'))?;
    let home = format!("{}/", home.trim_end_matches('/'));
    (path.starts_with(&home) && !data.starts_with(&home)).then(|| {
        format!(
            "{path} is under your home here ({home}), which is not the box's: a `~` you typed \
             was expanded by your shell here. Give the box's absolute path: on the box, \
             `command -v twr` prints it (or `echo ~/.local/bin/twr`)"
        )
    })
}

/// Copies the twr executable at `path` on box `name` into its builds directory under `data`
/// ([`ssh_box::from_box_install_script`]: the same checks as an upload, its SHA-256 and size
/// compared with the copy's) and returns its build id, once the user agreed (`consent`);
/// `None` when they did not. Its version is shown, and compared with the hub's (`hub`). The
/// build is added to the hub's builds reference `reference` ([`hub_reference`]).
#[allow(clippy::too_many_arguments)]
async fn install_from_box(
    name: &str,
    link: &BoxLink,
    data: &str,
    path: &str,
    hub: &HubBuild,
    consent: Consent<'_>,
    reference: Option<&str>,
) -> anyhow::Result<Option<String>> {
    outln!(
        "{path} on box {name} is copied to {}/<its build id>/twr, checked, and the hub runs it \
         there",
        ssh_box::builds_dir(data)
    );
    if !confirm(
        consent,
        &format!("Install it on box {name}? [y/N] "),
        &format!("not installing twr on box {name} without confirmation"),
    )? {
        return Ok(None);
    }
    let mut script = ssh_box::from_box_install_script(data, path, &upload_tag()?);
    if let Some(r) = reference {
        // (Run only once the script got to its end: `$id` is what it installed.)
        script.push_str(&ssh_box::install_ref_script(data, r, "\"$id\""));
    }
    let ran = here(link, &ssh_box::sh_c(&script), Input::None).await?;
    let (answer, version) = ssh_box::upload_answer(&ran.out);
    match (ran.code, answer.as_deref()) {
        (Some(0), Some(a)) if a.starts_with("ok ") => {}
        (Some(ssh_box::UPLOAD_MISMATCH), Some(why)) => bail!(
            "box {name} refused to install {path}: {why}; nothing was installed{}",
            upload_hints(link, name, data, Some(why), &ran.err)
        ),
        (code, _) => {
            let what = format!("installing {path} on box {name}");
            bail!(
                "{}{}",
                failed_here(link, name, &what, code, &ran.err),
                upload_hints(link, name, data, None, &ran.err)
            )
        }
    }
    let id = answer
        .as_deref()
        .and_then(|a| a.strip_prefix("ok "))
        .filter(|id| twr_core::valid_build_id(id))
        .ok_or_else(|| anyhow!("box {name} did not say which build it installed"))?
        .to_string();
    outln!(
        "installed build {id} on box {name}, at {}",
        ssh_box::build_path(data, &id)
    );
    version_note(name, version.as_deref(), hub);
    Ok(Some(id))
}

/// What `twr box install` says of the version a build installed on box `name` gave
/// (`version`), against the hub's (`hub`): the box connects only if both speak the same
/// runner protocol, which the handshake decides.
fn version_note(name: &str, version: Option<&str>, hub: &HubBuild) {
    let want = format!("twr {}", hub.version);
    match version {
        Some(v) if v == want => outln!("box {name} runs it as {v}"),
        Some(v) => {
            outln!("box {name} runs it as {v}");
            errln!(
                "twr: warning: the hub runs {want}: the box connects only if both speak the same \
                 runner protocol (the handshake decides)"
            );
        }
        None => {}
    }
}

/// Downloads the release archive of the hub's version for `target`, verified, uploads it to
/// box `name` under `data`, where `twr` is unpacked, checked and moved into place, and
/// returns its build id, once the user agreed; `None` when they did not. A development hub
/// has no release: refused, with how to build one. The build is added to the hub's builds
/// reference `reference` ([`hub_reference`]).
#[allow(clippy::too_many_arguments)]
async fn install_release(
    name: &str,
    link: &BoxLink,
    data: &str,
    target: &str,
    hub: &HubBuild,
    consent: Consent<'_>,
    reference: Option<&str>,
) -> anyhow::Result<Option<String>> {
    if let Some(commit) = dev_commit(&hub.version) {
        bail!("{}", dev_refusal(name, &hub.version, commit, target));
    }
    let tag = format!("v{}", hub.version);
    if !is_release_tag(&tag) {
        bail!(
            "the hub runs twr {}, which is no release version: build twr for {target} and \
             install it with `twr box install {name} --binary <path>`",
            hub.version
        );
    }
    outln!(
        "the hub runs release {tag}: its archive for {target} is downloaded, its signed \
         checksums verified, and twr unpacked from it on box {name}, under {}",
        ssh_box::builds_dir(data)
    );
    if !confirm(
        consent,
        &format!("Download twr {tag} for {target} and install it on box {name}? [y/N] "),
        &format!("not installing twr on box {name} without confirmation"),
    )? {
        return Ok(None);
    }
    let (archive, sha256) = fetch_release_archive(&tag, target).await?;
    outln!(
        "downloaded twr-{target}.tar.gz ({}), its signature and checksum verified",
        human_bytes(archive.len() as u64)
    );
    let tmp = PrivateDir::create("twr-box-install")?;
    let file = tmp.write("twr.tar.gz", &archive)?;
    let mut script = ssh_box::release_install_script(
        data,
        archive.len() as u64,
        &sha256,
        &hub.version,
        &upload_tag()?,
    );
    if let Some(r) = reference {
        // (Run only once the script got to its end: `$id` is what it installed.)
        script.push_str(&ssh_box::install_ref_script(data, r, "\"$id\""));
    }
    let ran = here(link, &ssh_box::sh_c(&script), Input::File(file)).await?;
    drop(tmp);
    let (answer, _) = ssh_box::upload_answer(&ran.out);
    upload_result(link, name, data, &ran, answer.as_deref())?;
    let id = answer
        .as_deref()
        .and_then(|a| a.strip_prefix("ok "))
        .filter(|id| twr_core::valid_build_id(id))
        .ok_or_else(|| anyhow!("box {name} did not say which build it installed"))?
        .to_string();
    outln!(
        "installed twr {} (build {id}) on box {name}, at {}",
        hub.version,
        ssh_box::build_path(data, &id)
    );
    Ok(Some(id))
}

/// A random tag for an upload's temporary file on the box.
fn upload_tag() -> anyhow::Result<String> {
    Ok(twr_core::runner::new_nonce()?[..16].to_string())
}

/// Whether an upload to box `name` through `link`, under data directory `data`, that ended
/// as `ran` with answer `answer` succeeded (an error saying why not, with [`upload_hints`]).
fn upload_result(
    link: &BoxLink,
    name: &str,
    data: &str,
    ran: &Here,
    answer: Option<&str>,
) -> anyhow::Result<()> {
    match (ran.code, answer) {
        (Some(0), Some(a)) if a == "ok" || a.starts_with("ok ") => Ok(()),
        (Some(ssh_box::UPLOAD_MISMATCH), Some(why)) => bail!(
            "box {name} refused the upload: {why}; nothing was installed{}",
            upload_hints(link, name, data, Some(why), &ran.err)
        ),
        (code, _) => {
            let what = format!("installing twr on box {name}");
            bail!(
                "{}{}",
                failed_here(link, name, &what, code, &ran.err),
                upload_hints(link, name, data, None, &ran.err)
            )
        }
    }
}

/// What an upload's error adds (`. <hint>`, or nothing), as the hub's own upload does: when
/// the box refused the build with `refusal` and that reads like a C library the build lacks,
/// [`ssh_box::glibc_hint`]; when a command box's stderr `err` says permission was denied (an
/// exec as another user, who kept the container's HOME), [`ssh_box::home_hint`] for `data`.
fn upload_hints(
    link: &BoxLink,
    name: &str,
    data: &str,
    refusal: Option<&str>,
    err: &[String],
) -> String {
    let hint = match refusal {
        Some(why) => ssh_box::glibc_hint(why),
        None => (link.pipe_checked()
            && err
                .iter()
                .any(|l| l.to_ascii_lowercase().contains("permission denied")))
        .then(|| ssh_box::home_hint(name, data)),
    };
    hint.map_or_else(String::new, |h| format!(". {h}"))
}

/// The commit part of a development version (`0.2.0+dev.<commit>`).
fn dev_commit(version: &str) -> Option<&str> {
    version.split_once("+dev.").map(|(_, c)| c)
}

/// Why a development hub (version `version`, of commit `commit`) cannot install twr on box
/// `name` of release target `target`, and how to build one for it.
fn dev_refusal(name: &str, version: &str, commit: &str, target: &str) -> String {
    let how = if target.ends_with("-linux-musl") {
        format!(
            "in a checkout of that commit, `TWR_BUILD={commit} cargo zigbuild --release \
             --locked -p twr-cli --target {target}` builds it (target/{target}/release/twr); \
             then run `twr box install {name} --binary <path>`"
        )
    } else {
        format!(
            "on the Mac, in a checkout of that commit, docs/tools/dev-install.sh builds it and \
             installs it as ~/.local/bin/twr; then run `twr box install {name} --from-box \
             <its absolute path on the Mac>`"
        )
    };
    format!(
        "the hub runs a development build (twr {version}), which has no release to download for \
         box {name} ({target}). Build twr of the same commit for {target}: {how}"
    )
}

/// `twr box remove` (see [`BoxCmd::Remove`]): run before the config is loaded (`main`), so
/// it can repair the files when they do not load: a box in both files, or one whose settings
/// twr refuses, comes out of boxes.toml ([`BoxesFile::remove_box`]). `socket`: the hub's, as
/// far as config.toml alone says.
pub(super) async fn remove(paths: &Paths, socket: &Path, name: &str) -> anyhow::Result<()> {
    refuse_agent_action("remove boxes")?;
    default_workspace_only(paths, "remove")?;
    let file = paths.boxes_file();
    let removed = {
        let _lock = BoxesFile::lock(&file)?;
        match BoxesFile::remove_box(&file, name)? {
            Some(b) => b,
            None => {
                // (Read loosely: a config.toml twr cannot load still says where the box is.)
                if BoxesFile::names_box(&paths.config_file, name) {
                    bail!(
                        "box {name} is configured in {}, which twr does not edit: remove its \
                         [boxes.{name}] there",
                        paths.config_file.display()
                    );
                }
                bail!("there is no box {name} in {}", file.display());
            }
        }
    };
    outln!("removed box {name} from {}", file.display());
    outln!("The running hub keeps the box until it restarts (`twr hub stop`, then `twr`).");
    // A token box: the running hub takes it out of what it accepts at once (`box.token`).
    // Asked whatever the removed table says its transport is (edited since the hub read it,
    // or misspelled, which the removal repairs): the hub decides from the box it runs.
    let outcome = token::call_box_token(paths, socket, name)
        .await
        .unwrap_or_else(|e| token::Outcome::Refused(format!("{e:#}")));
    let hub = HubBuild::of_running(socket).await;
    let live = hub.box_entry(name).and_then(|b| b["transport"].as_str());
    let transport = removed.get("transport").map(|t| t.as_str());
    if token::removed_token_box(&outcome, live, transport) {
        for l in token::removal_lines(name, &outcome, &paths.config_file) {
            outln!("{l}");
        }
        let done = format!("box {name} is out of {}", file.display());
        if let Some(why) = token::not_taken(name, &outcome, &done, "its runner") {
            bail!("{why}");
        }
        return Ok(());
    }
    let Ok(removed) = removed.try_into::<BoxConfig>() else {
        outln!("(Its settings were not valid, so twr cannot say how to stop its box daemon.)");
        return Ok(());
    };
    let link = removed
        .transport()
        .dialed()
        .then(|| BoxLink::of(&removed))
        .flatten();
    let Some(link) = link else {
        return Ok(());
    };
    if let BoxLink::Command { .. } = link {
        outln!("twr never stops or removes box {name}'s container: it stays as it is.");
    }
    let key_file = paths.data_dir.join(ssh_box::BOX_KEY_FILE);
    if !key_file.exists() {
        return Ok(());
    }
    let key = twr_core::sealed::HubKey::load_or_create(&key_file)?;
    // Only a build id: what the hub or the files say goes into a command to paste.
    let valid = |b: Option<String>| b.filter(|b| twr_core::valid_build_id(b));
    let build = valid(
        hub.box_entry(name)
            .and_then(|b| b["build"].as_str().map(str::to_string)),
    )
    .or_else(|| valid(removed.build.clone()))
    .or_else(|| valid(hub.build.clone()));
    let Some(build) = build else {
        return Ok(());
    };
    outln!(
        "The box daemon and the box's worktrees stay on the box. This stops the daemon (and the \
         agents it runs), once the hub no longer connects to the box:"
    );
    let stop = stop_command(&link, &build, name, &key.public_hex())?;
    outln!("  {stop}");
    Ok(())
}

/// The command that stops box `name`'s daemon of build `build` for the hub key `key`
/// ([`ssh_box::stop_script`]), through `link` as `twr box add` runs it: over ssh, the program
/// the hub runs, every option it forces and its refusal of words that forward anything
/// ([`ssh_box::interactive_ssh_args`]); through a command, its words then `sh -c <line>`, as
/// the hub runs it ([`BoxLink::here_args`]). Each word shell-quoted, so it can be pasted as
/// shown.
fn stop_command(link: &BoxLink, build: &str, name: &str, key: &str) -> anyhow::Result<String> {
    let remote =
        ssh_box::sh_c(&ssh_box::stop_script(build, name, key).map_err(|e| anyhow!("{e}"))?);
    let args = link.here_args(&remote).map_err(|why| match link {
        BoxLink::Ssh(ssh) => anyhow!("[boxes.{name}] ssh = {ssh:?}: {why}"),
        BoxLink::Command { .. } => anyhow!("[boxes.{name}] command: {why}"),
    })?;
    let program = Path::new(&link.program()).display().to_string();
    Ok(std::iter::once(program)
        .chain(args)
        .map(|w| twr_core::paths::sh_word(&w))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An upload's error says what the hub's own would: the C library hint when the box
    /// refused the build for its glibc, the HOME hint when a command box's user may not write
    /// under its data directory (not on an SSH box, where that is no `docker exec -u`).
    #[test]
    fn upload_errors_carry_the_hubs_hints() {
        let cmd = BoxLink::Command {
            command: vec!["docker".into(), "exec".into(), "-i".into(), "c".into()],
            terminal: None,
        };
        let ssh = BoxLink::Ssh("me@b".into());
        let denied = Here {
            code: Some(1),
            out: String::new(),
            err: vec!["mkdir: cannot create directory '/root/.local': Permission denied".into()],
        };
        let e = format!(
            "{:#}",
            upload_result(&cmd, "sb", "/root/.local/share", &denied, None).unwrap_err()
        );
        assert!(
            e.contains("installing twr on box sb failed (exit 1)")
                && e.contains(". box sb's HOME is /root, which its user can't write to"),
            "{e}"
        );
        let e = format!(
            "{:#}",
            upload_result(&ssh, "sb", "/root/.local/share", &denied, None).unwrap_err()
        );
        assert!(!e.contains("HOME"), "{e}");
        let why = "it does not run: twr: /lib/x86_64-linux-gnu/libc.so.6: version `GLIBC_2.39' \
                   not found";
        let refused = Here {
            code: Some(ssh_box::UPLOAD_MISMATCH),
            out: String::new(),
            err: Vec::new(),
        };
        let e = format!(
            "{:#}",
            upload_result(&cmd, "sb", "/home/box/.local/share", &refused, Some(why)).unwrap_err()
        );
        assert!(e.contains("nothing was installed"), "{e}");
        assert_eq!(
            e.contains("a development build of twr needs a box or container"),
            cfg!(all(target_os = "linux", target_env = "gnu")),
            "{e}"
        );
    }

    /// No socket is no hub; a socket that answers but says nothing is a hub whose boxes and
    /// build are unknown, not no hub.
    #[tokio::test]
    async fn a_hub_that_does_not_answer_is_unknown_not_absent() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let none = HubBuild::of_running(&socket).await;
        assert!(none.info.is_none() && none.unknown.is_none());
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let mute = tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                drop(s);
            }
        });
        let hub = HubBuild::of_running(&socket).await;
        assert!(hub.info.is_none());
        assert!(hub.unknown.is_some(), "a mute hub is no absent one");
        // Nor is its build this twr's: it may run another.
        assert_eq!(hub.build, None);
        assert_eq!(hub.protocol, None);
        assert!(hub.label().contains("did not say"), "{}", hub.label());
        mute.abort();
    }

    /// What ssh and the box print on stderr reaches the terminal cleaned (no escape sequence
    /// gets through), each line cut, and only so many lines.
    #[tokio::test]
    async fn ssh_stderr_is_cleaned_and_bounded() {
        let mut input = String::from("\x1b]0;owned\x07hello \x1b[2J\n");
        input.push_str(&"x".repeat(5 * ssh_box::NOTE_MAX_BYTES));
        input.push('\n');
        for i in 0..STDERR_LINES_SHOWN {
            input.push_str(&format!("line {i}\n"));
        }
        let mut shown = Vec::new();
        let kept = show_lines(input.as_bytes(), "ssh", &mut |l: &str| {
            shown.push(l.to_string())
        })
        .await;
        assert_eq!(shown[0], "\\u001b]0;owned\\u0007hello \\u001b[2J");
        assert!(shown.iter().all(|l| !l.contains('\x1b')));
        assert_eq!(shown[1].len(), ssh_box::NOTE_MAX_BYTES);
        assert_eq!(shown.len(), STDERR_LINES_SHOWN + 1);
        assert_eq!(shown.last().unwrap(), "(ssh printed more: not shown)");
        // The last lines shown are kept, as shown, to say what failed.
        assert_eq!(kept.len(), STDERR_LINES_KEPT);
        assert_eq!(
            kept[..],
            shown[STDERR_LINES_SHOWN - STDERR_LINES_KEPT..STDERR_LINES_SHOWN]
        );
        let kept = show_lines(&b"a\nb\n"[..], "docker", &mut |_: &str| {}).await;
        assert_eq!(kept, ["a", "b"]);
    }

    fn words(w: &[&str]) -> Vec<String> {
        w.iter().map(|s| s.to_string()).collect()
    }

    /// Whether process `pid` runs (exists and is no zombie).
    fn alive(pid: i32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|s| {
                s.rsplit_once(')')
                    .map(|(_, r)| !r.trim_start().starts_with('Z'))
            })
            .unwrap_or(false)
    }

    /// Waits (at most 10 s) for every pid in `pid_file` to be gone.
    fn all_gone(pid_file: &Path) {
        let pids: Vec<i32> = std::fs::read_to_string(pid_file)
            .unwrap()
            .split_whitespace()
            .map(|p| p.parse().unwrap())
            .collect();
        assert_eq!(pids.len(), 2, "{pids:?}");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        for pid in pids {
            while alive(pid) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "process {pid} is still running"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    /// A command link's run from here ([`here`]) leaves nothing of its process group behind,
    /// as the hub's does: a helper its wrapper started (`sleep 300 &`) is gone after a run to
    /// its end, after a caller's timeout with the command hanging (the wrapper's own process
    /// too), after more output than twr reads, and when the helper holds the command's stdout
    /// and stderr (the run still ends with the leader). (Only the group's kill reaches the
    /// helper.)
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_command_run_here_leaves_nothing_of_its_group_behind() {
        let dir = tempfile::tempdir().unwrap();
        // `$0`: where the helper's pid and the wrapper's go; then `env`, and `sh -c <line>`.
        let wrapper = "sleep 300 </dev/null >/dev/null 2>&1 & echo \"$! $$\" > \"$0\"; exec \"$@\"";
        let link = |pid_file: &Path| BoxLink::Command {
            command: words(&["sh", "-c", wrapper, pid_file.to_str().unwrap(), "env"]),
            terminal: None,
        };
        // To its end: its answer, the helper gone.
        let pids = dir.path().join("end.pids");
        let ran = here(&link(&pids), "echo said; exit 3", Input::None)
            .await
            .unwrap();
        assert_eq!((ran.code, ran.out.as_str()), (Some(3), "said\n"));
        all_gone(&pids);
        // A caller's timeout: the wrapper's process and the helper gone.
        let pids = dir.path().join("timeout.pids");
        let l = link(&pids);
        let hung = tokio::time::timeout(Duration::from_secs(1), here(&l, "sleep 300", Input::None));
        assert!(hung.await.is_err(), "it did not hang");
        all_gone(&pids);
        // More output than twr reads: the error, and nothing left.
        let pids = dir.path().join("output.pids");
        let l = link(&pids);
        let line = format!(
            "head -c {} /dev/zero; sleep 300",
            2 * ssh_box::SSH_OUTPUT_MAX
        );
        let e = tokio::time::timeout(Duration::from_secs(20), here(&l, &line, Input::None))
            .await
            .expect("an output past the limit ends the run")
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("more than twr reads"), "{e:#}");
        all_gone(&pids);
        // A helper that keeps the command's stdout and stderr open: the run ends with the
        // leader (its group killed then, which closes them), not with the helper.
        let pids = dir.path().join("inherit.pids");
        let inherits = "sleep 300 </dev/null & echo \"$! $$\" > \"$0\"; exec \"$@\"";
        let l = BoxLink::Command {
            command: words(&["sh", "-c", inherits, pids.to_str().unwrap(), "env"]),
            terminal: None,
        };
        let ran = tokio::time::timeout(
            Duration::from_secs(10),
            here(&l, "echo said; exit 3", Input::None),
        )
        .await
        .expect("the run ends with its leader")
        .unwrap();
        assert_eq!((ran.code, ran.out.as_str()), (Some(3), "said\n"));
        all_gone(&pids);
    }

    /// A command link's run from here gets what it is given on stdin (the pipe check's token,
    /// or a file), and its stderr's last lines are kept to say what failed: a command's known
    /// failure with its fix.
    #[tokio::test]
    async fn a_command_run_here_gets_its_input_and_says_its_failure() {
        let link = BoxLink::Command {
            command: words(&["env"]),
            terminal: None,
        };
        let ran = here(&link, "cat", link.probe_input()).await.unwrap();
        assert_eq!((ran.code, ran.out.as_str()), (Some(0), "twr-pipe-ok\n"));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("in");
        std::fs::write(&file, "from a file").unwrap();
        let ran = here(&link, "cat", Input::File(file)).await.unwrap();
        assert_eq!(ran.out, "from a file");
        let ran = here(
            &link,
            "echo 'Error response from daemon: No such container: nosuch' >&2; exit 1",
            Input::None,
        )
        .await
        .unwrap();
        assert_eq!(ran.code, Some(1));
        assert_eq!(
            failed_here(&link, "sb", "env to box sb", ran.code, &ran.err),
            "box sb's container does not exist (Error response from daemon: No such container: \
             nosuch): create and start it (docs/runners.md#container-boxes)"
        );
        let ran = here(&link, "echo other >&2; exit 2", Input::None)
            .await
            .unwrap();
        assert_eq!(
            failed_here(&link, "sb", "env to box sb", ran.code, &ran.err),
            "env to box sb failed (exit 2): see env's message above"
        );
        // ssh's own messages say why: never the hub's reading of them.
        let ssh = BoxLink::Ssh("me@gpu".into());
        let err = words(&["me@gpu: Permission denied (publickey)."]);
        assert_eq!(
            failed_here(&ssh, "gpu", "ssh to box gpu", Some(255), &err),
            "ssh to box gpu failed (exit 255): see ssh's message above"
        );
    }

    /// `twr box add <name> --dial-in --hub <url>` parses next to add's trailing words: the
    /// flags before any word (in any order, `--yes` and `--ca` too), no ssh words with
    /// `--dial-in`, `--hub` and `--ca` only with it; given after a destination they land among
    /// its words, which `run` refuses.
    #[test]
    fn dial_in_parses_next_to_the_trailing_words() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            crate::Cli::try_parse_from([&["twr", "box", "add"][..], args].concat()).map(|c| c.cmd)
        };
        let add = |args: &[&str]| match parse(args) {
            Ok(Some(crate::Cmd::Boxes {
                cmd:
                    BoxCmd::Add {
                        name,
                        words,
                        dial_in,
                        hub,
                        ca,
                        yes,
                        command,
                    },
            })) => (name, words, dial_in, hub, ca, yes, command),
            other => panic!("{args:?}: {other:?}"),
        };
        let (name, words, dial_in, hub, ca, yes, command) =
            add(&["gpu", "--dial-in", "--hub", "wss://h:7878"]);
        assert_eq!(
            (
                name.as_str(),
                words.len(),
                dial_in,
                hub.as_deref(),
                ca,
                yes,
                command
            ),
            ("gpu", 0, true, Some("wss://h:7878"), None, false, false)
        );
        let (_, words, dial_in, hub, ca, yes, _) = add(&[
            "gpu",
            "--hub=wss://h:7878",
            "--yes",
            "--ca",
            "/etc/ca.pem",
            "--dial-in",
        ]);
        assert!(words.is_empty() && dial_in && yes, "{words:?}");
        assert_eq!(hub.as_deref(), Some("wss://h:7878"));
        assert_eq!(ca.as_deref(), Some("/etc/ca.pem"));
        // `--dial-in` without --hub parses: the command says which URLs fit.
        assert!(add(&["gpu", "--dial-in"]).2);
        // An ssh box as before, `--yes` among its words.
        let (_, words, dial_in, hub, ..) = add(&["gpu", "-p", "2222", "me@host", "--yes"]);
        assert_eq!(words, ["-p", "2222", "me@host", "--yes"]);
        assert!(!dial_in && hub.is_none());
        // Refused: no words and no --dial-in; words with it; --hub or --ca without it;
        // --dial-in with --command.
        for bad in [
            &["gpu"][..],
            &["gpu", "--dial-in", "--hub", "wss://h:1", "me@host"],
            &["gpu", "--hub", "wss://h:1"],
            &["gpu", "--dial-in", "--command", "docker", "exec", "-i", "c"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        // With ssh words, clap takes --hub and --ca without --dial-in: `run` refuses them.
        let (_, _, dial_in, hub, ..) = add(&["gpu", "--hub", "wss://h:1", "me@host"]);
        assert!(!dial_in && hub.is_some());
        // After a destination, they are its words: refused before anything runs.
        let (_, words, dial_in, ..) = add(&["gpu", "me@host", "--dial-in", "--hub", "wss://h:1"]);
        assert!(!dial_in);
        let e = refuse_misplaced_dial_in(&words).unwrap_err();
        assert!(e.to_string().contains("go before any other word"), "{e}");
        let (_, words, ..) = add(&["gpu", "me@host", "--hub=wss://h:1"]);
        assert!(refuse_misplaced_dial_in(&words).is_err());
        assert!(refuse_misplaced_dial_in(&words[..1]).is_ok());
    }

    /// `twr box token <name>` with `--rotate` (`--yes` only with it) or `--reload`, not both.
    #[test]
    fn box_token_parses() {
        use clap::Parser;
        let parse = |args: &[&str]| {
            crate::Cli::try_parse_from([&["twr", "box", "token"][..], args].concat()).map(|c| c.cmd)
        };
        let token = |args: &[&str]| match parse(args) {
            Ok(Some(crate::Cmd::Boxes {
                cmd:
                    BoxCmd::Token {
                        name,
                        rotate,
                        reload,
                        yes,
                    },
            })) => (name, rotate, reload, yes),
            other => panic!("{args:?}: {other:?}"),
        };
        assert_eq!(token(&["gpu"]), ("gpu".into(), false, false, false));
        assert_eq!(
            token(&["gpu", "--rotate", "--yes"]),
            ("gpu".into(), true, false, true)
        );
        assert_eq!(
            token(&["gpu", "--reload"]),
            ("gpu".into(), false, true, false)
        );
        for bad in [&["gpu", "--rotate", "--reload"][..], &["gpu", "--yes"], &[]] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    /// `twr box add`'s words: an ssh destination joined with spaces, a command's kept apart;
    /// `--yes` among them taken out of both.
    #[test]
    fn the_destination_takes_its_words_and_leaves_yes() {
        let ssh = |w: &[&str], yes: bool| {
            let (w, yes) = destination(&words(w), yes);
            (ssh_destination(&w), yes)
        };
        assert_eq!(ssh(&["me@host"], false), ("me@host".into(), false));
        assert_eq!(
            ssh(&["-p", "2222", "me@host", "--yes"], false),
            ("-p 2222 me@host".into(), true)
        );
        assert_eq!(
            ssh(&["-p 2222 me@host"], true),
            ("-p 2222 me@host".into(), true)
        );
        // A command's words, as given: one with a space stays one.
        assert_eq!(
            destination(&words(&["docker", "exec", "-i", "--yes", "my box"]), false),
            (words(&["docker", "exec", "-i", "my box"]), true)
        );
        assert_eq!(
            destination(&words(&["docker", "exec", "-i", "c"]), false),
            (words(&["docker", "exec", "-i", "c"]), false)
        );
    }

    /// The terminal_command `twr box add` derives: `-t` right after the `-i` (`--interactive`,
    /// `--stdin`) of `docker`, `podman` and `nerdctl exec` and `kubectl exec`, every other word
    /// kept (`-u`, `-e HOME=...`, `-w`); none for another program, or without such a word
    /// before `--`.
    #[test]
    fn a_container_engine_s_exec_gets_its_terminal_command() {
        let t = |argv: &[&str]| default_terminal(&words(argv));
        assert_eq!(
            t(&["docker", "exec", "-i", "twr-sandbox"]),
            Some(words(&["docker", "exec", "-i", "-t", "twr-sandbox"]))
        );
        assert_eq!(
            t(&["/usr/bin/podman", "exec", "--interactive", "c"]),
            Some(words(&[
                "/usr/bin/podman",
                "exec",
                "--interactive",
                "-t",
                "c"
            ]))
        );
        assert_eq!(
            t(&["nerdctl", "--namespace", "k8s.io", "exec", "-i", "c"]),
            Some(words(&[
                "nerdctl",
                "--namespace",
                "k8s.io",
                "exec",
                "-i",
                "-t",
                "c"
            ]))
        );
        assert_eq!(
            t(&["kubectl", "exec", "-i", "-n", "ns", "pod", "--"]),
            Some(words(&[
                "kubectl", "exec", "-i", "-t", "-n", "ns", "pod", "--"
            ]))
        );
        assert_eq!(
            t(&["kubectl", "exec", "--stdin", "pod", "--"]),
            Some(words(&["kubectl", "exec", "--stdin", "-t", "pod", "--"]))
        );
        // A container that runs as root, exec'd as its user (R6): every other word kept.
        assert_eq!(
            t(&[
                "docker",
                "exec",
                "-i",
                "-u",
                "box",
                "-e",
                "HOME=/home/box",
                "-w",
                "/home/box",
                "c"
            ]),
            Some(words(&[
                "docker",
                "exec",
                "-i",
                "-t",
                "-u",
                "box",
                "-e",
                "HOME=/home/box",
                "-w",
                "/home/box",
                "c"
            ]))
        );
        for none in [
            &["docker", "exec", "c"][..],
            &["docker", "-i", "exec", "c"],
            &["kubectl", "exec", "pod", "--", "-i"],
            &["lxc", "exec", "-i", "c"],
            &["nsenter", "-t", "1", "-i"],
            &["env"],
        ] {
            assert_eq!(t(none), None, "{none:?}");
        }
    }

    /// What `twr box add` says of how the hub reaches the box, and of what runs in your
    /// terminal on a command box.
    #[test]
    fn box_add_says_how_the_hub_reaches_the_box() {
        let ssh = reach_lines("gpu", &BoxLink::Ssh("me@gpu".into())).join("\n");
        assert_eq!(
            ssh,
            "- The hub connects to the box over ssh and starts a twr box daemon there, which \
             runs the box's threads and keeps them running while the hub is away."
        );
        let sb = BoxLink::Command {
            command: words(&["docker", "exec", "-i", "twr-sandbox"]),
            terminal: Some(words(&["docker", "exec", "-i", "-t", "twr-sandbox"])),
        };
        assert_eq!(
            reach_lines("sb", &sb),
            [
                "- The hub runs `docker exec -i twr-sandbox sh -c ...` itself, from its own \
                 environment, and starts a twr box daemon there, which runs the box's threads \
                 and keeps them running while the hub is away.",
                "- For `twr box login` and `twr box project --clone`, twr runs `docker exec -i \
                 -t twr-sandbox` in your terminal (terminal_command)."
            ]
        );
        let bare = BoxLink::Command {
            command: words(&["my-exec", "c"]),
            terminal: None,
        };
        assert_eq!(
            reach_lines("sb", &bare)[1],
            "- `twr box login` and `twr box project --clone` need a terminal_command in \
             [boxes.sb] (the command with `-t`); without one they are refused."
        );
        assert_eq!(reached(&sb), "through `docker exec -i twr-sandbox`");
        assert_eq!(
            reached(&BoxLink::Ssh(" me@gpu ".into())),
            "over ssh (me@gpu)"
        );
    }

    #[test]
    fn release_targets_of_the_boxes_twr_builds_for() {
        assert_eq!(
            release_target("linux", "x86_64"),
            Some("x86_64-unknown-linux-musl")
        );
        assert_eq!(
            release_target("linux", "aarch64"),
            Some("aarch64-unknown-linux-musl")
        );
        assert_eq!(
            release_target("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            release_target("macos", "x86_64"),
            Some("x86_64-apple-darwin")
        );
        assert_eq!(release_target("freebsd", "x86_64"), None);
    }

    #[test]
    fn a_development_hub_says_how_to_build_for_the_box() {
        assert_eq!(dev_commit("0.2.0+dev.abc1234.dirty"), Some("abc1234.dirty"));
        assert_eq!(dev_commit("0.2.0"), None);
        let linux = dev_refusal(
            "gpu",
            "0.2.0+dev.abc1234",
            "abc1234",
            "aarch64-unknown-linux-musl",
        );
        assert!(
            linux.contains(
                "`TWR_BUILD=abc1234 cargo zigbuild --release --locked -p twr-cli --target \
                 aarch64-unknown-linux-musl`"
            ),
            "{linux}"
        );
        assert!(
            linux.contains("`twr box install gpu --binary <path>`"),
            "{linux}"
        );
        let mac = dev_refusal(
            "mac",
            "0.2.0+dev.abc1234",
            "abc1234",
            "aarch64-apple-darwin",
        );
        assert!(mac.contains("docs/tools/dev-install.sh"), "{mac}");
        assert!(mac.contains("`twr box install mac --from-box "), "{mac}");
    }

    #[test]
    fn list_and_status_show_the_boxes() {
        let ssh = json!({"name": "gpu", "transport": "ssh", "state": "online", "online": true,
        "version": "0.2.0", "build": "0123456789abcdef", "os": "Linux", "arch": "x86_64",
        "last_error": null, "notes": ["twr-box: started the box daemon 0123456789abcdef"],
        "switch_pending": false, "refused": null,
        "agents": [
            {"runtime": "claude-code", "found": true, "path": "/home/me/.local/bin/claude",
             "version": "2.1.0 (Claude Code)", "login": "logged_in", "method": "claude.ai"},
            {"runtime": "codex", "found": true, "path": "/usr/bin/codex",
             "login": "logged_out"},
            {"runtime": "grok-build", "found": false, "login": "unknown",
             "note": "not found on PATH"}
        ]});
        let token = json!({"name": "mac", "transport": "token", "state": "offline",
            "online": false, "version": null, "os": null, "arch": null, "last_error": null,
            "notes": null, "switch_pending": null,
            "refused": {"at": "2026-10-16T09:30:12.345Z", "reason": "a bad proof"},
            "agents": []});
        let mut command = ssh.clone();
        command["name"] = json!("sb");
        command["transport"] = json!("command");
        command["agents"] = json!([]);
        assert_eq!(
            list_text(&[ssh.clone(), token.clone(), command.clone()]),
            "NAME  TRANSPORT  STATE    VERSION  OS/ARCH       AGENTS\n\
             gpu   ssh        online   0.2.0    linux/x86_64  claude ✓ codex ✗ grok -\n\
             mac   token      offline  -        -             -\n\
             sb    command    online   0.2.0    linux/x86_64  -\n"
        );
        let c = status_text(&command, false);
        assert!(
            c.contains(
                "transport:      command (the hub reaches the box through its command, a \
                 container's)\n"
            ),
            "{c}"
        );
        assert!(
            c.contains("build:          0123456789abcdef (launched by the hub)\n"),
            "{c}"
        );
        assert!(status_text(&ssh, false)
            .contains("transport:      ssh (the hub connects to the box over ssh)\n"));
        // A login twr does not know is `?`.
        let unknown = json!([{"runtime": "grok-build", "found": true, "login": "unknown"},
            {"runtime": "other", "found": true, "login": "something-new"}]);
        assert_eq!(
            agents_cell(&box_agents(&json!({ "agents": unknown }))),
            "grok ? other ?"
        );
        let s = status_text(&ssh, false);
        assert!(s.contains("state:          online\n"), "{s}");
        assert!(
            s.contains("build:          0123456789abcdef (launched by the hub)\n"),
            "{s}"
        );
        assert!(s.contains("os/arch:        linux/x86_64\n"), "{s}");
        assert!(!s.contains("launch:"), "{s}");
        // How the daemon was started, as it says.
        let mut systemd = ssh.clone();
        systemd["launch"] = json!("systemd twr-box-0123-abcd.service");
        assert!(
            status_text(&systemd, false).contains(
                "launch:         the box daemon runs as the systemd user service \
                 twr-box-0123-abcd.service\n"
            ),
            "{}",
            status_text(&systemd, false)
        );
        let mut mac = ssh.clone();
        mac["os"] = json!("Darwin");
        mac["launch"] = json!("detached");
        let m = status_text(&mac, false);
        assert!(
            m.contains(
                "launch:         the daemon runs from ssh, so Claude Code can't read its keychain \
                 login: log in at the Mac's screen (or turn on automatic login), then `twr box \
                 restart gpu`\n"
            ),
            "{m}"
        );
        // Offline: what the last daemon said is no longer known to be so.
        let mut gone = mac.clone();
        gone["state"] = json!("offline");
        let g = status_text(&gone, false);
        assert!(
            g.contains("launch:         not known: the box is not connected (it is offline)\n"),
            "{g}"
        );
        assert!(
            s.contains(
                "agents:         (as the box last reported them)\n  \
                 claude: logged in (claude.ai), 2.1.0 (Claude Code), at \
                 /home/me/.local/bin/claude\n  \
                 codex: logged out, at /usr/bin/codex\n  \
                 grok: not installed (not found on PATH)\n"
            ),
            "{s}"
        );
        assert!(
            s.ends_with("notes:          (newest last)\n  twr-box: started the box daemon 0123456789abcdef\n"),
            "{s}"
        );
        let t = status_text(&token, false);
        assert!(
            t.contains(
                "transport:      token (its runner connects to the hub's listener, [hub] listen)\n"
            ),
            "{t}"
        );
        // A token box without ssh: a dial-in one.
        let d = status_text(&token, true);
        assert!(
            d.contains(
                "transport:      token, dial-in (its runner connects to the hub's listener)\n"
            ),
            "{d}"
        );
        assert!(
            t.contains("refused:        2026-10-16 09:30 UTC: a bad proof\n"),
            "{t}"
        );
        assert!(t.contains("version:        - (never connected)\n"), "{t}");
        assert!(t.contains("agents:         - (not reported yet)\n"), "{t}");
    }

    #[test]
    fn the_stop_command_runs_the_relay_with_stop() {
        let ssh = |w: &str| BoxLink::Ssh(w.into());
        let stop = stop_command(&ssh("-p 2222 me@host"), "0123456789abcdef", "gpu", "ab").unwrap();
        // Every word as the builder makes it, the forced options among them.
        let program = Path::new(&ssh_box::ssh_program()).display().to_string();
        let want: Vec<String> = std::iter::once(program)
            .chain(
                ssh_box::interactive_ssh_args(
                    "-p 2222 me@host",
                    &ssh_box::sh_c(&ssh_box::stop_script("0123456789abcdef", "gpu", "ab").unwrap()),
                )
                .unwrap(),
            )
            .collect();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n' {stop}"))
            .output()
            .unwrap();
        let got: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(got, want);
        assert!(!stop.contains('\n'), "one line to paste: {stop}");
        assert!(stop.contains(" -o ClearAllForwardings=yes "), "{stop}");
        assert!(stop.contains(" me@host -a -x -k -- "), "{stop}");
        // The remote script, run here: the data directory as the probe finds it.
        let script = ssh_box::stop_script("0123456789abcdef", "gpu", "ab").unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script.replace("exec ", "printf '%s\\n' "))
            .env("HOME", "/home/u")
            .env_remove("XDG_DATA_HOME")
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            "/home/u/.local/share/twr/runner/builds/0123456789abcdef/twr\nrunner\n--stdio\n\
             --box=gpu\n--hub-key\nab\n--stop\n"
        );
        // Words that would forward something are refused here too, and a build that is no
        // build id (what the hub or the files said) or a name that is no box name.
        assert!(stop_command(&ssh("-A me@host"), "0123456789abcdef", "gpu", "ab").is_err());
        assert!(stop_command(&ssh("me@host"), "x; rm -rf ~", "gpu", "ab").is_err());
        assert!(stop_command(&ssh("me@host"), "0123456789abcdef", "-gpu", "ab").is_err());
    }

    /// A command box's stop command is its command, then `sh -c` and the same line ssh would
    /// run, each word shell-quoted (a word with a space stays one); it runs the relay's
    /// `--stop` as pasted.
    #[test]
    fn a_command_box_s_stop_command_runs_its_command() {
        let link = BoxLink::Command {
            command: words(&["docker", "exec", "-i", "-e", "A=b c", "twr-sandbox"]),
            terminal: None,
        };
        let stop = stop_command(&link, "0123456789abcdef", "sb", "ab").unwrap();
        let line = ssh_box::sh_c(&ssh_box::stop_script("0123456789abcdef", "sb", "ab").unwrap());
        assert_eq!(
            stop,
            format!(
                "docker exec -i -e 'A=b c' twr-sandbox sh -c {}",
                twr_core::paths::sh_word(&line)
            )
        );
        assert!(!stop.contains('\n'), "one line to paste: {stop}");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '%s\\n' {stop}"))
            .output()
            .unwrap();
        let got: Vec<String> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        let mut want = words(&[
            "docker",
            "exec",
            "-i",
            "-e",
            "A=b c",
            "twr-sandbox",
            "sh",
            "-c",
        ]);
        want.extend(line.lines().map(str::to_string));
        assert_eq!(got, want);
        assert!(stop_command(&link, "x; rm -rf ~", "sb", "ab").is_err());
    }

    /// An install's build is recorded only for the box it went to: one whose destination (or
    /// anything else but its build) changed meanwhile is refused and the file left alone.
    #[test]
    fn an_installed_build_is_recorded_only_for_the_same_box() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config);
        std::fs::write(&config, "").unwrap();
        let ssh = |dest: &str, build: Option<&str>| BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(dest.into()),
            build: build.map(str::to_string),
            ..BoxConfig::default()
        };
        let save = |c: BoxConfig| {
            let mut b = BoxesFile::default();
            b.boxes.insert("gpu".into(), c);
            b.save(&file).unwrap();
        };
        let used = ssh("me@gpu", Some("0000000000000001"));
        save(used.clone());
        record_installed_build(&config, &file, "gpu", &used, "0000000000000002").unwrap();
        let now = BoxesFile::load(&file).unwrap();
        assert_eq!(now.boxes["gpu"], ssh("me@gpu", Some("0000000000000002")));
        // Removed and added again with another destination while installing.
        save(ssh("me@other", None));
        let e =
            record_installed_build(&config, &file, "gpu", &used, "0000000000000003").unwrap_err();
        assert!(format!("{e:#}").contains("changed in"), "{e:#}");
        assert_eq!(
            BoxesFile::load(&file).unwrap().boxes["gpu"],
            ssh("me@other", None)
        );
        // A project recorded meanwhile (`twr box project`): the same box, recorded, the
        // project kept.
        let mut with_project = ssh("me@gpu", Some("0000000000000002"));
        with_project.projects.insert(
            "api".into(),
            twr_core::config::BoxProjectConfig {
                repo: "/h/api".into(),
                runtime: None,
            },
        );
        save(with_project.clone());
        record_installed_build(&config, &file, "gpu", &used, "0000000000000004").unwrap();
        let now = BoxesFile::load(&file).unwrap().boxes["gpu"].clone();
        assert_eq!(now.build.as_deref(), Some("0000000000000004"));
        assert_eq!(now.projects, with_project.projects);
        // Removed: refused too.
        BoxesFile::default().save(&file).unwrap();
        let e =
            record_installed_build(&config, &file, "gpu", &used, "0000000000000003").unwrap_err();
        assert!(format!("{e:#}").contains("was removed"), "{e:#}");
    }

    /// `twr box install`'s snapshot is the file's box now, refused when its connection
    /// settings moved since the config was loaded (a project or a build alone does not).
    #[test]
    fn the_install_snapshot_is_the_file_s_box_now() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config);
        std::fs::write(&config, "").unwrap();
        let ssh = |dest: &str| BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(dest.into()),
            ..BoxConfig::default()
        };
        let save = |c: &BoxConfig| {
            let mut b = BoxesFile::default();
            b.boxes.insert("gpu".into(), c.clone());
            b.save(&file).unwrap();
        };
        let loaded = ssh("me@a");
        let mut more = loaded.clone();
        more.build = Some("0123456789abcdef".into());
        more.projects.insert(
            "api".into(),
            twr_core::config::BoxProjectConfig {
                repo: "/h/api".into(),
                runtime: None,
            },
        );
        save(&more);
        assert_eq!(
            box_snapshot(&config, &file, "gpu", &loaded).unwrap(),
            (more, true)
        );
        save(&ssh("me@b"));
        let e = box_snapshot(&config, &file, "gpu", &loaded).unwrap_err();
        assert!(format!("{e:#}").contains("changed in"), "{e:#}");
        BoxesFile::default().save(&file).unwrap();
        std::fs::write(
            &config,
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@a\"\n",
        )
        .unwrap();
        assert_eq!(
            box_snapshot(&config, &file, "gpu", &loaded).unwrap(),
            (loaded, false)
        );
    }

    #[test]
    fn box_add_names_the_projects_configured_for_the_box() {
        let config: Config = Config::from_toml_str(
            "[projects.api]\nrepo = \"/h/api\"\nbox = \"gpu\"\n\
             [projects.web]\nrepo = \"/h/web\"\n\
             [projects.\"my app\"]\nrepo = \"/h/app\"\nbox = \"gpu\"\n",
        )
        .unwrap();
        assert_eq!(
            project_hint_lines(&config, "gpu"),
            ["twr box project gpu api", "twr box project gpu 'my app'"]
        );
        assert!(project_hint_lines(&config, "other").is_empty());
    }

    #[test]
    fn a_box_is_added_once_and_checked_with_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        let file = BoxesFile::path_for(&config);
        std::fs::write(&config, "[boxes.mac]\ntoken = \"0123456789abcdef0123\"\n").unwrap();
        let ssh = |dest: &str| BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some(dest.into()),
            ..BoxConfig::default()
        };
        let e = with_added_box(&config, &file, "mac", ssh("me@mac")).unwrap_err();
        assert!(format!("{e:#}").contains("configured in"), "{e:#}");
        let boxes = with_added_box(&config, &file, "gpu", ssh("me@gpu")).unwrap();
        boxes.save(&file).unwrap();
        let e = with_added_box(&config, &file, "gpu", ssh("me@gpu")).unwrap_err();
        assert!(format!("{e:#}").contains("twr box remove gpu"), "{e:#}");
        // A destination that is only spaces is no destination.
        let e = with_added_box(&config, &file, "x", ssh("  ")).unwrap_err();
        assert!(format!("{e:#}").contains("needs `ssh`"), "{e:#}");
        let merged = Config::load_with_boxes(&config).unwrap();
        assert_eq!(merged.boxes["gpu"].ssh.as_deref(), Some("me@gpu"));
    }

    /// The notes printed while a restart is waited for are those that came since the last
    /// look, the hub keeping only the latest; and the restart's wait (and an install's for a
    /// switch) covers an attempt under way when it asks, then the one that does what it asked:
    /// each a probe, an upload and the hub's deadline for a replacing channel (the relay's
    /// whole stop and start), and the WebSocket's handshake; the switch's quiet and drain
    /// besides.
    #[test]
    fn a_restart_prints_the_new_notes_and_waits_long_enough() {
        let v = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(new_notes(&v(&["a", "b"]), &v(&["a", "b", "c"])), v(&["c"]));
        assert_eq!(
            new_notes(&v(&["a", "b", "c"]), &v(&["b", "c", "d"])),
            v(&["d"])
        );
        assert!(new_notes(&v(&["a", "b"]), &v(&["a", "b"])).is_empty());
        assert_eq!(new_notes(&v(&["a"]), &v(&["x", "y"])), v(&["x", "y"]));
        assert_eq!(new_notes(&[], &v(&["x"])), v(&["x"]));
        assert_eq!(new_notes(&v(&["w", "w"]), &v(&["w", "w", "w"])), v(&["w"]));
        let attempt = ssh_box::PROBE_TIMEOUT
            + ssh_box::UPLOAD_TIMEOUT
            + ssh_box::REPLACE_OPEN_TIMEOUT
            + ssh_box::WS_TIMEOUT;
        assert_eq!(ssh_box::ATTEMPT_WORST, attempt);
        assert!(ssh_box::REPLACE_OPEN_TIMEOUT >= ssh_box::PLAIN_OPEN_TIMEOUT);
        assert!(RESTART_WAIT >= 2 * attempt);
        assert!(RESTART_WAIT > 4 * twr_runner::relay::STOP_TIMEOUT);
        assert!(
            SWITCH_WAIT
                >= 2 * attempt
                    + ssh_box::SWITCH_QUIET
                    + ssh_box::SWITCH_CHECK
                    + ssh_box::SWITCH_DRAIN
        );
        // What the help and the docs say: about an hour and twenty minutes, and an hour and a
        // half at most.
        assert_eq!(RESTART_WAIT.as_secs().div_ceil(60), 81);
        assert_eq!(SWITCH_WAIT.as_secs().div_ceil(60), 86);
    }

    /// A restart is over once the box is online on a newer connection with none pending.
    #[test]
    fn a_restart_is_over_on_a_newer_connection() {
        let b = |state: &str, epoch: Option<u64>, pending: bool| json!({"state": state, "epoch": epoch, "restart_pending": pending});
        assert!(restarted(&b("online", Some(4), false), Some(3)));
        assert!(!restarted(&b("online", Some(3), false), Some(3)));
        assert!(!restarted(&b("online", Some(4), true), Some(3)));
        assert!(!restarted(&b("offline", None, false), Some(3)));
        assert!(!restarted(&b("connecting", Some(4), false), Some(3)));
        // None was up when it was asked for: any connection that carried it out.
        assert!(restarted(&b("online", Some(1), false), None));
        assert!(!restarted(&b("online", Some(1), true), None));
    }

    /// Restarting a box repaired since it failed: the `error` or `needs_install` from before
    /// the request does not end the wait, until the hub has gone on (another state, another
    /// last error) or for longer than a stale state lasts; one the restart's attempt ends in
    /// does.
    #[test]
    fn a_restart_fails_only_on_its_own_attempt() {
        let b = |state: &str, error: &str| json!({"state": state, "last_error": error});
        let soon = Duration::from_millis(500);
        for state in ["error", "needs_install"] {
            // The state from before, then the restart's attempt, which fails again.
            let before = b(state, "the old reason");
            let mut w = RestartWait::new(Some(&before));
            assert!(!w.failed(&before, Duration::ZERO), "{state}");
            assert!(!w.failed(&before, soon), "{state}");
            assert!(
                !w.failed(&b("connecting", "the old reason"), soon),
                "{state}"
            );
            assert!(w.failed(&b(state, "the old reason"), soon), "{state}");
            // Repaired: the attempt is connecting, then online (not a failure).
            let mut w = RestartWait::new(Some(&before));
            assert!(!w.failed(&before, Duration::ZERO), "{state}");
            assert!(
                !w.failed(&b("connecting", "the old reason"), soon),
                "{state}"
            );
            assert!(!w.failed(&b("online", "the old reason"), soon), "{state}");
            // An attempt that failed between two looks, for another reason.
            let mut w = RestartWait::new(Some(&before));
            assert!(w.failed(&b(state, "a new reason"), soon), "{state}");
            // For the same one: once the state from before would have changed.
            let mut w = RestartWait::new(Some(&before));
            assert!(!w.failed(&before, soon), "{state}");
            assert!(w.failed(&before, STALE_NEEDS_INSTALL), "{state}");
        }
        // Online when asked for: a failure after that is the restart's.
        let mut w = RestartWait::new(Some(&json!({"state": "online", "last_error": null})));
        assert!(!w.failed(&json!({"state": "online", "last_error": null}), soon));
        assert!(w.failed(&b("error", "ssh failed"), soon));
    }

    /// What `twr box add` says a Mac needs, for a release hub and a development one.
    #[test]
    fn a_mac_box_is_told_what_it_needs() {
        let release = mac_needs("mac", "0.2.0").join("\n");
        assert!(
            release.contains("logged in at the Mac's screen"),
            "{release}"
        );
        assert!(release.contains("automatic login"), "{release}");
        assert!(
            release.contains("`twr box install mac` downloads release v0.2.0's"),
            "{release}"
        );
        assert!(release.contains("logged in at the Mac itself"), "{release}");
        let dev = mac_needs("mac", "0.2.0+dev.abc1234").join("\n");
        assert!(dev.contains("docs/tools/dev-install.sh"), "{dev}");
        assert!(dev.contains("`twr box install mac --from-box "), "{dev}");
        assert!(dev.contains("(abc1234)"), "{dev}");
    }

    /// `--from-box` takes an absolute path of the box's, and warns about one under this
    /// machine's home (a `~` the shell expanded here).
    #[test]
    fn from_box_paths_are_the_boxs() {
        assert!(check_box_path("mac", "/Users/me/.local/bin/twr").is_ok());
        let e = check_box_path("mac", "~/.local/bin/twr").unwrap_err();
        assert!(
            format!("{e:#}").contains("absolute path on box mac")
                && format!("{e:#}").contains("on the box, `command -v twr` prints it"),
            "{e:#}"
        );
        assert!(check_box_path("mac", "/a\nb").is_err());
        let home = std::env::var("HOME").unwrap();
        let here = format!("{}/.local/bin/twr", home.trim_end_matches('/'));
        let w = home_here(&here, "/Users/someone/.local/share").unwrap();
        assert!(
            w.contains("expanded by your shell here")
                && w.contains("`command -v twr` prints it")
                && !w.contains("quote"),
            "{w}"
        );
        assert_eq!(
            home_here(&here, &format!("{home}/.local/share")),
            None,
            "the box shares this home"
        );
        assert_eq!(home_here("/opt/twr", "/Users/someone/.local/share"), None);
    }
}

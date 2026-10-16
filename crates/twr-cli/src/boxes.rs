//! `twr box`: the remote boxes, from the command line. `add` sets up a box the hub reaches over
//! SSH (`[boxes.<name>] transport = "ssh"`, in `boxes.toml`) and adds it to the running hub
//! (`box.add`); `list` and `status` show `hub.info`'s boxes; `install` puts a twr build on an
//! SSH box (the hub's own, a release's for another OS or CPU, or a file) and has the hub
//! connect again (`box.install`); `remove` takes a box out of `boxes.toml`.
//!
//! The probes run ssh in the user's terminal, with the hub's ssh options but without
//! `BatchMode` ([`twr_hub::ssh_box::interactive_ssh_args`]), so ssh can ask about an unknown
//! host key or for a passphrase; the probe script and its parser are the hub's
//! ([`twr_hub::ssh_box::probe_script`], [`twr_hub::ssh_box::parse_probe`]). `boxes.toml` is
//! changed only under its lock ([`BoxesFile::lock`]): read, changed, checked with
//! `config.toml` and saved in one hold.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use clap::Subcommand;
use serde_json::{json, Value};
use twr_core::client::Client;
use twr_core::config::{BoxConfig, BoxTransport, BoxesFile, Config};
use twr_core::git::human_bytes;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_hub::ssh_box;

use super::{
    ask_on_stderr, connect, consent, fetch_release_archive, is_release_tag, refuse_agent_action,
    str_of, table, wait_for_hub, workspace_hint, Consent, PrivateDir,
};

/// How long `twr box add` and `twr box install` wait for the box to come online.
const ONLINE_WAIT: Duration = Duration::from_secs(90);

/// How long `twr box install` waits for a box that is online on another build to run the one
/// it installed: the hub switches it once it has been quiet for 2 minutes
/// ([`ssh_box::SWITCH_QUIET`]), looking every 30 s, and gives its runtimes up to 2 minutes to
/// exit.
const SWITCH_WAIT: Duration = Duration::from_secs(360);

/// How often they look.
const ONLINE_POLL: Duration = Duration::from_millis(500);

/// How long `twr box install` takes a `needs_install` it reads for the state from before its
/// `box.install`, while the hub has not shown another one.
const STALE_NEEDS_INSTALL: Duration = Duration::from_secs(10);

/// How long the check that ssh works without prompting may take.
const BATCH_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Subcommand)]
pub(super) enum BoxCmd {
    /// Add a box the hub reaches over SSH: no token, listener or tunnel. Probes the box with
    /// ssh in this terminal (ssh may ask about its host key or for a passphrase), checks that
    /// ssh also works without prompting (the hub's never prompts), says what will happen and
    /// asks y/N (without a terminal, only with --yes). Then writes `[boxes.<name>]` to
    /// boxes.toml, next to config.toml, and adds the box to the running hub (starting one when
    /// none runs). The hub installs its own twr build on the box (same OS and CPU; else run
    /// `twr box install <name>`), connects over ssh and starts a twr box daemon there, which
    /// runs the box's threads. Waits up to 90 s for the box to come online. Boxes belong to
    /// the default workspace.
    Add {
        /// The box's name: letters, digits, `.`, `_` or `-` (not starting with `.` or `-`; not `local`).
        name: String,
        /// The ssh destination (`me@host`, or a host from `~/.ssh/config`), with any ssh
        /// options before it (`-p 2222 me@host`): one argument or several words, joined with
        /// spaces.
        #[arg(
            required = true,
            value_name = "SSH",
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        ssh: Vec<String>,
        /// Add the box without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List the hub's remote boxes: name, transport (`ssh`: the hub connects to it; `token`:
    /// its runner connects to the hub), state, the twr version its runner reported, and its OS
    /// and CPU (SSH boxes).
    #[command(alias = "ls")]
    List {
        /// Print the boxes of `hub.info` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show what the hub knows of a box: its state, the twr version and build it runs, its
    /// OS and CPU, the last error, its latest notes, and a connection the hub refused.
    Status {
        /// The box's name.
        name: String,
        /// Print the box's entry of `hub.info` as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Install twr on an SSH box and have the hub connect to it again. On a box of the hub's OS
    /// and CPU the hub installs its own build. On another one: with --binary, that file;
    /// otherwise, when the hub runs a release, the release's archive for the box (its signed
    /// checksums verified as `twr update` verifies them), unpacked on the box. The build is
    /// recorded as the box's `build` in boxes.toml (a box of config.toml: the line to add is
    /// printed). Asks y/N before uploading (without a terminal, only with --yes). Waits up to
    /// 90 s for the box to come online.
    Install {
        /// The box's name.
        name: String,
        /// A twr executable built for the box's OS and CPU, to upload instead of a release's.
        #[arg(long, value_name = "PATH")]
        binary: Option<PathBuf>,
        /// Upload without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Remove a box from boxes.toml (a box of config.toml: remove it there). The running hub
    /// keeps the box until it restarts; the box daemon and the box's worktrees stay on the box
    /// (the command that stops the daemon is printed).
    #[command(alias = "rm")]
    Remove {
        /// The box's name.
        name: String,
    },
}

/// Runs `twr box ...` for the hub on `socket` of `paths`'s workspace, whose config (with
/// boxes.toml) is `config`.
pub(super) async fn run(
    cmd: BoxCmd,
    config: &Config,
    paths: &Paths,
    socket: &Path,
) -> anyhow::Result<()> {
    match cmd {
        BoxCmd::Add { name, ssh, yes } => {
            let (ssh, yes) = destination(&ssh, yes);
            add(config, paths, socket, &name, &ssh, yes).await
        }
        BoxCmd::List { json } => list(socket, json).await,
        BoxCmd::Status { name, json } => status(socket, &name, json).await,
        BoxCmd::Install { name, binary, yes } => {
            install(config, paths, socket, &name, binary.as_deref(), yes).await
        }
        BoxCmd::Remove { name } => remove(paths, socket, &name).await,
    }
}

/// The ssh destination of `twr box add`'s words, joined with spaces, and whether `--yes` was
/// given: a `--yes` after the destination lands among its words (they take everything that
/// follows) and is taken out (ssh has no long options).
fn destination(words: &[String], yes: bool) -> (String, bool) {
    let given = words.iter().any(|w| w == "--yes");
    let ssh: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|w| *w != "--yes")
        .collect();
    (ssh.join(" ").trim().to_string(), yes || given)
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
/// hub started now runs).
struct HubBuild {
    version: String,
    build: Option<String>,
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

    /// This twr's version and build (what a hub started now runs), `unknown` as given.
    fn own(unknown: Option<String>) -> HubBuild {
        HubBuild {
            version: twr_core::version().to_string(),
            build: twr_core::build_id().map(str::to_string),
            info: None,
            unknown,
        }
    }

    /// What `hub.info` `info` says.
    fn of_info(info: Value) -> HubBuild {
        HubBuild {
            version: str_of(&info["version"]).to_string(),
            build: info["build"].as_str().map(str::to_string),
            info: Some(info),
            unknown: None,
        }
    }

    /// `twr <version>, build <id>`.
    fn label(&self) -> String {
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

/// The ssh program the hub runs too (`TWR_SSH`, else `ssh`).
fn ssh_program_name() -> String {
    Path::new(&ssh_box::ssh_program()).display().to_string()
}

/// Runs `remote` on the box `ssh` names, with ssh in this terminal: its prompts and messages
/// on stderr, `input` (when given) as its stdin, stdout captured (at most
/// [`ssh_box::SSH_OUTPUT_MAX`]: more is an error, and ssh is killed). The exit code (`None`:
/// a signal) and stdout.
async fn ssh_here(
    ssh: &str,
    remote: &str,
    input: Option<&Path>,
) -> anyhow::Result<(Option<i32>, String)> {
    let mut cmd = tokio::process::Command::new(ssh_box::ssh_program());
    cmd.args(
        ssh_box::interactive_ssh_args(ssh, remote).map_err(|why| anyhow!("ssh {ssh:?}: {why}"))?,
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    match input {
        Some(path) => {
            let f =
                std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
            cmd.stdin(Stdio::from(f));
        }
        None => {
            cmd.stdin(Stdio::null());
        }
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", ssh_program_name()))?;
    let stdout = child.stdout.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    // (An error drops `child`, which kills ssh.)
    let (out, ()) = tokio::join!(
        ssh_box::read_capped(stdout, ssh_box::SSH_OUTPUT_MAX, "ssh (stdout)"),
        show_stderr(stderr),
    );
    let out = out.map_err(|e| anyhow!("{e}"))?;
    let status = child
        .wait()
        .await
        .with_context(|| format!("running {}", ssh_program_name()))?;
    Ok((status.code(), String::from_utf8_lossy(&out).into_owned()))
}

/// The most lines of ssh's stderr (its messages and the box's) [`ssh_here`] shows; the rest
/// is read and dropped.
const STDERR_LINES_SHOWN: usize = 200;

/// Shows ssh's stderr `stderr` (ssh's messages, and what the box prints there) on this one, a
/// line at a time: each line cut at [`ssh_box::NOTE_MAX_BYTES`] and its control characters
/// escaped ([`crate::term::clean`], through `errln!`), at most [`STDERR_LINES_SHOWN`] lines.
/// ssh asks its questions (a host key, a passphrase) on the terminal itself (`/dev/tty`), not
/// here, so they keep working.
async fn show_stderr(stderr: tokio::process::ChildStderr) {
    show_lines(stderr, &mut |line: &str| errln!("{line}")).await;
}

/// [`show_stderr`]'s work: the lines of `r` to `show`, each cut and cleaned, at most
/// [`STDERR_LINES_SHOWN`] (then one saying the rest is not shown).
async fn show_lines<R: tokio::io::AsyncRead + Unpin>(r: R, show: &mut dyn FnMut(&str)) {
    let mut r = tokio::io::BufReader::new(r);
    let mut shown = 0;
    while let Ok(Some(line)) = ssh_box::read_line_capped(&mut r, ssh_box::NOTE_MAX_BYTES).await {
        shown += 1;
        if shown <= STDERR_LINES_SHOWN {
            show(&crate::term::clean(&line));
        } else if shown == STDERR_LINES_SHOWN + 1 {
            show("(ssh printed more: not shown)");
        }
    }
}

/// How long the probe of `twr box add` and `twr box install` may take, ssh's questions
/// included.
const PROBE_WAIT: Duration = Duration::from_secs(120);

/// Probes box `name` over ssh in this terminal ([`ssh_here`], within [`PROBE_WAIT`]) and says
/// what it found. Its data directory too (an error without one).
async fn probe(name: &str, ssh: &str) -> anyhow::Result<(ssh_box::Probe, String)> {
    outln!("probing box {name} over ssh ({ssh})...");
    let remote = ssh_box::sh_c(&ssh_box::probe_script());
    let probing = ssh_here(ssh, &remote, None);
    let (code, out) = tokio::time::timeout(PROBE_WAIT, probing)
        .await
        .map_err(|_| {
            anyhow!(
                "probing box {name} did not finish within {} s (a question of ssh's left \
                 unanswered, or a box that does not answer); nothing was changed",
                PROBE_WAIT.as_secs()
            )
        })??;
    if code != Some(0) {
        let code = code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
        bail!("ssh to box {name} failed ({code}): see ssh's message above");
    }
    let probe = ssh_box::parse_probe(&out).with_context(|| format!("probing box {name}"))?;
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
        // Bounded: more than allowed ends the check (and drops `ssh`, which kills it).
        let max = ssh_box::SSH_OUTPUT_MAX;
        let (_, err) = tokio::try_join!(
            ssh_box::read_capped(stdout, max, "ssh (stdout)"),
            ssh_box::read_capped(stderr, max, "ssh (stderr)"),
        )?;
        let status = ssh.finish().await.map_err(|e| e.to_string())?;
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
    ssh: &str,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("add boxes")?;
    default_workspace_only(paths, "add")?;
    twr_core::runner::validate_box_name(name)?;
    if name == twr_hub::hub::LOCAL_BOX {
        bail!("box name local is the hub's own box: name the remote box otherwise");
    }
    if ssh.is_empty() {
        bail!("give the box's ssh destination: `twr box add {name} me@host`");
    }
    let file = paths.boxes_file();
    if config.boxes.contains_key(name) {
        bail!("there is a box {name} already (`twr box status {name}` shows it)");
    }
    let cfg = BoxConfig {
        transport: Some(BoxTransport::Ssh),
        ssh: Some(ssh.to_string()),
        ..BoxConfig::default()
    };
    // Checked now so a bad destination fails before any ssh.
    with_added_box(&paths.config_file, &file, name, cfg.clone())?;
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

    let (probe, data) = probe(name, ssh).await?;
    if let Some(why) = batch_check(name, ssh).await {
        errln!(
            "twr: warning: ssh to box {name} does not work without prompting: {why}\n\
             The hub runs ssh without prompting (BatchMode), so it cannot connect until it \
             can: load the key into an ssh-agent the hub can use (or start the hub from a \
             shell that has one), or use a key without a passphrase. The box can still be \
             added."
        );
    }
    let same = same_target(&probe);
    outln!();
    outln!("Adding box {name} (ssh {ssh}):");
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
    outln!(
        "- The hub connects to the box over ssh and starts a twr box daemon there, which runs \
         the box's threads and keeps them running while the hub is away."
    );
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
        Online::Yes(b) => online_note(name, &b),
        Online::NeedsInstall(b) if !same => {
            outln!(
                "box {name} needs a twr build for {}: run `twr box install {name}`",
                target_label(&probe)
            );
            print_reason(&b);
        }
        Online::NeedsInstall(b) => {
            print_reason(&b);
            bail!("the hub could not install twr on box {name}");
        }
    }
    Ok(())
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
/// `switching`, as the hub switches it once quiet) the wait goes on, at most [`SWITCH_WAIT`].
/// An error on timeout or when the hub's box key is unusable (state `error`), after printing
/// the state, the last error and the notes.
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
    let last = loop {
        let info = c.call("hub.info", Value::Null).await?;
        let Some(b) = find_box(&info, name).cloned() else {
            bail!("the hub has no box {name}");
        };
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
    let rows: Vec<[String; 5]> = boxes
        .iter()
        .map(|b| {
            [
                str_of(&b["name"]).to_string(),
                str_of(&b["transport"]).to_string(),
                str_of(&b["state"]).to_string(),
                b["version"].as_str().unwrap_or("-").to_string(),
                os_arch(b).unwrap_or_else(|| "-".into()),
            ]
        })
        .collect();
    table(&["NAME", "TRANSPORT", "STATE", "VERSION", "OS/ARCH"], &rows)
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

/// `twr box status`.
async fn status(socket: &Path, name: &str, json: bool) -> anyhow::Result<()> {
    let mut c = connect(socket).await?;
    let info = c.call("hub.info", Value::Null).await?;
    let Some(b) = find_box(&info, name) else {
        bail!("the hub has no box {name} (`twr box list` lists its boxes)");
    };
    if json {
        outln!("{}", serde_json::to_string_pretty(b)?);
    } else {
        out!("{}", status_text(b));
    }
    Ok(())
}

/// `twr box status`'s text for box entry `b`.
fn status_text(b: &Value) -> String {
    let mut out = String::new();
    let mut line = |k: &str, v: &str| out.push_str(&format!("{k:<15} {v}\n"));
    let ssh = b["transport"] == "ssh";
    line("box:", str_of(&b["name"]));
    line(
        "transport:",
        if ssh {
            "ssh (the hub connects to the box over ssh)"
        } else {
            "token (its runner connects to the hub's listener, [hub] listen)"
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
    if let Some(e) = b["last_error"].as_str() {
        line("last error:", e);
    }
    if let Some(r) = b["refused"].as_object() {
        line(
            "refused:",
            &format!("{}: {}", super::trash_time(&r["at"]), str_of(&r["reason"])),
        );
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

/// `twr box install` (see [`BoxCmd::Install`]).
async fn install(
    config: &Config,
    paths: &Paths,
    socket: &Path,
    name: &str,
    binary: Option<&Path>,
    yes: bool,
) -> anyhow::Result<()> {
    refuse_agent_action("install twr on boxes")?;
    default_workspace_only(paths, "install")?;
    twr_core::runner::validate_box_name(name)?;
    let Some(cfg) = config.boxes.get(name) else {
        bail!(
            "there is no box {name} in config.toml or boxes.toml (`twr box add {name} <ssh \
             destination>` adds one)"
        );
    };
    if cfg.transport() != BoxTransport::Ssh {
        bail!(
            "box {name} is a token box: its runner connects to the hub, and you install twr on \
             it yourself (docs/runners.md)"
        );
    }
    let ssh = cfg.ssh.clone().unwrap_or_default();
    let file = paths.boxes_file();
    let in_boxes_file = BoxesFile::load(&file)?.boxes.contains_key(name);
    if let Some(b) = binary {
        // Read now: a path that is wrong fails before any ssh.
        std::fs::metadata(b).with_context(|| format!("reading {}", b.display()))?;
    }
    let (probe, data) = probe(name, &ssh).await?;
    if same_target(&probe) {
        if binary.is_some() {
            bail!(
                "box {name} is {} like the hub: the hub runs its own build there, and installs \
                 it itself (run `twr box install {name}` without --binary)",
                target_label(&probe)
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
    let id = match binary {
        Some(path) => {
            install_binary(name, &ssh, &data, path, &hub, consent, reference.as_deref()).await?
        }
        None => {
            install_release(
                name,
                &ssh,
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

/// Records build `id`, just installed over ssh to box `name` as configured by `used`, as the
/// box's `build` in boxes.toml (`file`, next to `config_file`), under the file's lock: only
/// while the box there is still the one installed to (everything of it but its `build` as in
/// `used`: a box removed and added again meanwhile, with another destination say, would get a
/// build of another machine). Refused otherwise, with the file left as it is.
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
    let without_build = |c: &BoxConfig| BoxConfig {
        build: None,
        ..c.clone()
    };
    if without_build(b) != without_build(used) {
        bail!(
            "box {name} changed in {} while twr was installing on it (its ssh destination or \
             settings): build {id} was not recorded and stays on the box it went to; run `twr \
             box install {name}` again",
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
    ssh: &str,
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
    let (code, out) = ssh_here(ssh, &ssh_box::sh_c(&script), Some(path)).await?;
    let (answer, version) = ssh_box::upload_answer(&out);
    upload_result(name, code, answer.as_deref())?;
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
    Ok(Some(id))
}

/// Downloads the release archive of the hub's version for `target`, verified, uploads it to
/// box `name` under `data`, where `twr` is unpacked, checked and moved into place, and
/// returns its build id, once the user agreed; `None` when they did not. A development hub
/// has no release: refused, with how to build one. The build is added to the hub's builds
/// reference `reference` ([`hub_reference`]).
#[allow(clippy::too_many_arguments)]
async fn install_release(
    name: &str,
    ssh: &str,
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
    let (code, out) = ssh_here(ssh, &ssh_box::sh_c(&script), Some(&file)).await?;
    drop(tmp);
    let (answer, _) = ssh_box::upload_answer(&out);
    upload_result(name, code, answer.as_deref())?;
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

/// Whether an upload to box `name` that exited `code` with answer `answer` succeeded (an
/// error saying why not).
fn upload_result(name: &str, code: Option<i32>, answer: Option<&str>) -> anyhow::Result<()> {
    match (code, answer) {
        (Some(0), Some(a)) if a == "ok" || a.starts_with("ok ") => Ok(()),
        (Some(ssh_box::UPLOAD_MISMATCH), Some(why)) => {
            bail!("box {name} refused the upload: {why}; nothing was installed")
        }
        (code, _) => {
            let code = code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
            bail!("installing twr on box {name} failed ({code}): see ssh's message above")
        }
    }
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
             --locked -p twr-cli --target {target}` builds it (target/{target}/release/twr)"
        )
    } else {
        "on the Mac, build it from that commit with docs/tools/dev-install.sh and copy the twr \
         binary here"
            .to_string()
    };
    format!(
        "the hub runs a development build (twr {version}), which has no release to download for \
         box {name} ({target}). Build twr of the same commit for {target}: {how}; then run `twr \
         box install {name} --binary <path>`"
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
    let Ok(removed) = removed.try_into::<BoxConfig>() else {
        outln!("(Its settings were not valid, so twr cannot say how to stop its box daemon.)");
        return Ok(());
    };
    if removed.transport() != BoxTransport::Ssh {
        return Ok(());
    }
    let key_file = paths.data_dir.join(ssh_box::BOX_KEY_FILE);
    if !key_file.exists() {
        return Ok(());
    }
    let key = twr_core::sealed::HubKey::load_or_create(&key_file)?;
    let hub = HubBuild::of_running(socket).await;
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
    let stop = stop_command(
        removed.ssh.as_deref().unwrap_or_default(),
        &build,
        name,
        &key.public_hex(),
    )?;
    outln!("  {stop}");
    Ok(())
}

/// The command that stops box `name`'s daemon of build `build` for the hub key `key`
/// ([`ssh_box::stop_script`]), over ssh to `ssh` as `twr box add` runs it: the program the hub
/// runs, every option it forces and its refusal of words that forward anything
/// ([`ssh_box::interactive_ssh_args`]), each word shell-quoted, so it can be pasted as shown.
fn stop_command(ssh: &str, build: &str, name: &str, key: &str) -> anyhow::Result<String> {
    let remote =
        ssh_box::sh_c(&ssh_box::stop_script(build, name, key).map_err(|e| anyhow!("{e}"))?);
    let args = ssh_box::interactive_ssh_args(ssh, &remote)
        .map_err(|why| anyhow!("[boxes.{name}] ssh = {ssh:?}: {why}"))?;
    Ok(std::iter::once(ssh_program_name())
        .chain(args)
        .map(|w| twr_core::paths::sh_word(&w))
        .collect::<Vec<_>>()
        .join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        show_lines(input.as_bytes(), &mut |l: &str| shown.push(l.to_string())).await;
        assert_eq!(shown[0], "\\u001b]0;owned\\u0007hello \\u001b[2J");
        assert!(shown.iter().all(|l| !l.contains('\x1b')));
        assert_eq!(shown[1].len(), ssh_box::NOTE_MAX_BYTES);
        assert_eq!(shown.len(), STDERR_LINES_SHOWN + 1);
        assert_eq!(shown.last().unwrap(), "(ssh printed more: not shown)");
    }

    #[test]
    fn the_destination_takes_its_words_and_leaves_yes() {
        let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            destination(&words(&["me@host"]), false),
            ("me@host".into(), false)
        );
        assert_eq!(
            destination(&words(&["-p", "2222", "me@host", "--yes"]), false),
            ("-p 2222 me@host".into(), true)
        );
        assert_eq!(
            destination(&words(&["-p 2222 me@host"]), true),
            ("-p 2222 me@host".into(), true)
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
    }

    #[test]
    fn list_and_status_show_the_boxes() {
        let ssh = json!({"name": "gpu", "transport": "ssh", "state": "online", "online": true,
            "version": "0.2.0", "build": "0123456789abcdef", "os": "Linux", "arch": "x86_64",
            "last_error": null, "notes": ["twr-box: started the box daemon 0123456789abcdef"],
            "switch_pending": false, "refused": null});
        let token = json!({"name": "mac", "transport": "token", "state": "offline",
            "online": false, "version": null, "os": null, "arch": null, "last_error": null,
            "notes": null, "switch_pending": null,
            "refused": {"at": "2026-10-16T09:30:12.345Z", "reason": "a bad proof"}});
        assert_eq!(
            list_text(&[ssh.clone(), token.clone()]),
            "NAME  TRANSPORT  STATE    VERSION  OS/ARCH\n\
             gpu   ssh        online   0.2.0    linux/x86_64\n\
             mac   token      offline  -        -\n"
        );
        let s = status_text(&ssh);
        assert!(s.contains("state:          online\n"), "{s}");
        assert!(
            s.contains("build:          0123456789abcdef (launched by the hub)\n"),
            "{s}"
        );
        assert!(s.contains("os/arch:        linux/x86_64\n"), "{s}");
        assert!(
            s.ends_with("notes:          (newest last)\n  twr-box: started the box daemon 0123456789abcdef\n"),
            "{s}"
        );
        let t = status_text(&token);
        assert!(
            t.contains("its runner connects to the hub's listener"),
            "{t}"
        );
        assert!(
            t.contains("refused:        2026-10-16 09:30 UTC: a bad proof\n"),
            "{t}"
        );
        assert!(t.contains("version:        - (never connected)\n"), "{t}");
    }

    #[test]
    fn the_stop_command_runs_the_relay_with_stop() {
        let stop = stop_command("-p 2222 me@host", "0123456789abcdef", "gpu", "ab").unwrap();
        // Every word as the builder makes it, the forced options among them.
        let want: Vec<String> = std::iter::once(ssh_program_name())
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
        assert!(stop_command("-A me@host", "0123456789abcdef", "gpu", "ab").is_err());
        assert!(stop_command("me@host", "x; rm -rf ~", "gpu", "ab").is_err());
        assert!(stop_command("me@host", "0123456789abcdef", "-gpu", "ab").is_err());
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
        // Removed: refused too.
        BoxesFile::default().save(&file).unwrap();
        let e =
            record_installed_build(&config, &file, "gpu", &used, "0000000000000003").unwrap_err();
        assert!(format!("{e:#}").contains("was removed"), "{e:#}");
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
}

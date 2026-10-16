//! The relay of an SSH box (`twr runner --stdio`): what the hub's `ssh` runs on the box. It
//! finds the box daemon of this hub key and box ([`crate::daemon`]), starts it when none
//! runs (or replaces one of another build when asked to), and then copies bytes between its
//! stdin/stdout (the hub, through ssh) and the daemon's socket until either side ends. The
//! hub and the daemon talk through it end to end ([`twr_core::sealed`]): the relay sees only
//! sealed records.
//!
//! Everything the relay says goes to stderr, one short `twr-box: ...` line each, which the hub
//! shows; stdout carries nothing but the daemon's bytes.
//!
//! # Paths
//!
//! Resolved here, once ([`DaemonPaths`]), and passed to the daemon as arguments: a daemon that
//! `systemd-run` starts has another environment.
//!
//! As it starts, the relay adds its own build to its hub's reference for its box in the
//! builds directory it runs from ([`record_build_ref`], [`twr_core::BUILD_REFS_DIR`]): a
//! prune by any hub that shares the directory keeps it.
//!
//! # Starting the daemon
//!
//! The daemon is this executable ([`RelayOptions::program`]) run as `runner --daemon` with
//! every path, in a session of its own (`setsid`), its stdin, stdout and stderr `/dev/null`
//! (it writes its own log, [`DaemonPaths::log`]): the ssh session's end does not reach it. On
//! Linux, when the user lingers (`loginctl show-user <uid> -p Linger` is `Linger=yes`) and
//! `systemd-run` is on `PATH`, it is started as a transient user service instead
//! (`systemd-run --user --collect --quiet --unit twr-box-<hash>-<random>`, a new unit name
//! each start: [`unit_name`]), outside the ssh session's scope, so logind cannot end it with
//! the session. Such a service gets the user manager's
//! environment, which lacks what the session has (its `PATH`, which finds the agents'
//! executables, and the API keys the agents read): the relay passes its own environment on,
//! by name only (`--setenv=NAME`, which takes the value from `systemd-run`'s environment), so
//! no value appears in an argument list other users can read, and with the manager's `$VAR`
//! expansion off, so the paths reach the daemon as resolved ([`systemd_run_args`]). When
//! `systemd-run` fails (one too old for that form included) and its unit does not run (one
//! that only did not answer in time counts as started), the daemon is started in a new
//! session instead. When logind ends a user's processes with their last session
//! (`KillUserProcesses=yes`) and the daemon runs in the ssh session (the user does not linger,
//! or `systemd-run` failed), the relay warns: the daemon and its agents end with the ssh
//! session. Each probe is best effort, bounded by
//! [`PROBE_TIMEOUT`]: one that fails means "no". The relay then waits for the daemon to answer
//! on its socket (at most [`RelayOptions::start_timeout`]), or fails with the end of its log.
//!
//! `TWR_BOX_LAUNCHER=setsid` in the relay's environment ([`LAUNCHER_ENV`]) always starts the
//! daemon in a new session, without probing logind or trying `systemd-run`: for tests (a
//! daemon a test can find and stop) and for troubleshooting.
//!
//! # Stopping the daemon
//!
//! `twr runner --stdio --stop` ([`stop`]) asks the daemon of the hub key and box to stop and
//! waits until it has (at most [`RelayOptions::stop_timeout`]): its runtimes end, its data
//! directory and worktrees stay. With no daemon running, it says so and succeeds. No socket is
//! not proof of that: a daemon removes its socket first and then drains, holding its machine
//! lock, so `--stop` (and a relay about to start a daemon) waits for that lock, within the
//! same timeout.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};

use crate::client::{machine_lock_path_in, validate_box_name};
use crate::daemon::{request, Request, Status};

/// How long each probe of the box's session setup (`loginctl`, `busctl`, `systemd-run`) may
/// take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Lines of the daemon's log shown when it does not start.
const LOG_TAIL_LINES: usize = 20;

/// Where the box daemon of one hub key and box keeps everything on the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    /// Its socket: `<state>/twr/box-daemons/<hash>.sock`, `<hash>` the first 16 hex digits
    /// of the SHA-256 of the hub id, a NUL byte and the box name ([`socket_hash`]), short
    /// enough for macOS's 103-byte socket paths. In the state directory, not
    /// `$XDG_RUNTIME_DIR`, which logind removes when the user's last session ends.
    pub socket: PathBuf,
    /// Its data directory (spool, worktrees, trash, log): `<data>/twr/box-daemons/<hub
    /// id>-<box>`, so two hubs (two hub keys) sharing a box get separate daemons.
    pub data_dir: PathBuf,
    /// Its machine lock: [`crate::client::machine_lock_path`] of hub `ssh://<hub id>` and the
    /// box.
    pub lock: PathBuf,
    /// Its log: `<data dir>/daemon.log`.
    pub log: PathBuf,
}

impl DaemonPaths {
    /// The paths for hub key `hub_key` and box `box_name` from this process's environment
    /// ([`DaemonPaths::resolve_in`]).
    pub fn resolve(hub_key: &[u8; 32], box_name: &str) -> anyhow::Result<DaemonPaths> {
        Self::resolve_in(hub_key, box_name, |k| std::env::var_os(k))
    }

    /// The paths for hub key `hub_key` and box `box_name`, the environment read through `env`:
    /// the state directory is `$XDG_STATE_HOME`, else `$HOME/.local/state`; the data directory
    /// `$XDG_DATA_HOME`, else `$HOME/.local/share`. A variable that is empty or not an
    /// absolute path counts as unset (as the XDG spec says), so every path is absolute.
    pub fn resolve_in(
        hub_key: &[u8; 32],
        box_name: &str,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> anyhow::Result<DaemonPaths> {
        validate_box_name(box_name)?;
        let env = |k: &str| env(k).filter(|v| Path::new(v).is_absolute());
        let var = |k: &str| env(k).map(PathBuf::from);
        let home = var("HOME");
        let missing = |what: &str, var: &str, under: &str| {
            anyhow!(
                "neither {var} nor HOME is set to an absolute path: the box daemon's {what} \
                 would be under $HOME/{under}"
            )
        };
        let state = var("XDG_STATE_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local/state")))
            .ok_or_else(|| missing("socket", "XDG_STATE_HOME", ".local/state"))?;
        let data = var("XDG_DATA_HOME")
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")))
            .ok_or_else(|| missing("data", "XDG_DATA_HOME", ".local/share"))?;
        let id = twr_core::sealed::hub_id(hub_key);
        let socket = state
            .join("twr/box-daemons")
            .join(format!("{}.sock", socket_hash(&id, box_name)));
        let data_dir = data
            .join("twr/box-daemons")
            .join(format!("{id}-{box_name}"));
        let lock = machine_lock_path_in(&format!("ssh://{id}"), box_name, env)
            .ok_or_else(|| missing("lock", "XDG_STATE_HOME", ".local/state"))?;
        Ok(DaemonPaths {
            log: data_dir.join("daemon.log"),
            socket,
            data_dir,
            lock,
        })
    }
}

/// The first 16 hex digits of the SHA-256 of `hub_id`, a NUL byte and `box_name`: names the
/// daemon's socket and its systemd units (`twr-box-<hash>-<random>`).
pub fn socket_hash(hub_id: &str, box_name: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(hub_id.as_bytes());
    h.update([0]);
    h.update(box_name.as_bytes());
    h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// How the relay starts a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launcher {
    /// As the module docs say: `systemd-run` when the user lingers (Linux), else in a new
    /// session.
    Auto,
    /// Always in a new session, without probing the box's session setup (tests).
    Setsid,
}

/// The relay's environment variable that picks how it starts a daemon: `setsid` is
/// [`Launcher::Setsid`]; anything else, or nothing, [`Launcher::Auto`].
pub const LAUNCHER_ENV: &str = "TWR_BOX_LAUNCHER";

impl Launcher {
    /// The launcher [`LAUNCHER_ENV`]'s value `value` asks for.
    pub fn from_env(value: Option<&std::ffi::OsStr>) -> Launcher {
        match value {
            Some(v) if v == "setsid" => Launcher::Setsid,
            _ => Launcher::Auto,
        }
    }
}

/// Everything the relay needs.
#[derive(Debug, Clone)]
pub struct RelayOptions {
    /// The box.
    pub box_name: String,
    /// The hub's public key (`--hub-key`).
    pub hub_key: [u8; 32],
    /// Replace a daemon of another build than this one (`--replace`, [`decide`]).
    pub replace: bool,
    /// Where the daemon lives.
    pub paths: DaemonPaths,
    /// The executable started as the daemon (`runner --daemon ...`): this one
    /// ([`std::env::current_exe`]).
    pub program: PathBuf,
    /// How the daemon is started.
    pub launcher: Launcher,
    /// How long a started daemon may take to answer on its socket.
    pub start_timeout: Duration,
    /// How long a daemon asked to stop may take to let go of its socket and its lock.
    pub stop_timeout: Duration,
    /// The build the hub launched (`--build`, the build id of this relay's executable as the
    /// hub knows it): taken as this relay's own when it cannot read its executable, so that
    /// `--replace` still tells another build's daemon apart ([`decide`]).
    pub build: Option<String>,
}

impl RelayOptions {
    /// The options for box `box_name` and hub key `hub_key`: paths from the environment
    /// ([`DaemonPaths::resolve`]), this executable as the daemon, the launcher
    /// [`LAUNCHER_ENV`] asks for, 15 s to start, 60 s to stop.
    pub fn new(box_name: &str, hub_key: [u8; 32], replace: bool) -> anyhow::Result<RelayOptions> {
        Ok(RelayOptions {
            box_name: box_name.to_string(),
            hub_key,
            replace,
            paths: DaemonPaths::resolve(&hub_key, box_name)?,
            program: std::env::current_exe().context("finding this twr's executable")?,
            launcher: Launcher::from_env(std::env::var_os(LAUNCHER_ENV).as_deref()),
            start_timeout: Duration::from_secs(15),
            stop_timeout: Duration::from_secs(60),
            build: None,
        })
    }

    /// This relay's build: its executable's ([`twr_core::build_id`]), else the one the hub
    /// launched ([`RelayOptions::build`]).
    pub fn own_build(&self) -> Option<&str> {
        twr_core::build_id().or(self.build.as_deref())
    }

    /// The daemon's arguments (after the program).
    pub fn daemon_args(&self) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec![
            "runner".into(),
            "--daemon".into(),
            // Joined: a name is never taken for an option.
            format!("--box={}", self.box_name).into(),
            "--hub-key".into(),
            hex_of(&self.hub_key).into(),
        ];
        for (flag, path) in [
            ("--daemon-socket", &self.paths.socket),
            ("--data-dir", &self.paths.data_dir),
            ("--lock", &self.paths.lock),
            ("--log", &self.paths.log),
        ] {
            args.push(flag.into());
            args.push(path.clone().into_os_string());
        }
        args
    }
}

/// Lowercase hex of `bytes`.
fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// What the relay does with the daemon it found ([`decide`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// None answers: start one.
    Start,
    /// Use the one that answers.
    Use,
    /// Stop the one that answers and start this build.
    Replace,
}

/// What to do with the daemon that answered with `running` (`None`: none did), for the hub
/// with id `hub_id`, this relay being build `own_build` and `--replace` given or not:
///
/// - none: start one;
/// - one of another hub key: an error (the socket is named after the hub key: whatever
///   answers there is not that hub's daemon);
/// - with `--replace`, one of another build than this one (or of an unknown one): replace it,
///   whatever its version or protocol (the hub asks for it when it wants its own build);
///   when this relay's own build is unknown, nothing tells the two apart: use it;
/// - otherwise use it as is (another build of the same protocol serves the hub as well; the
///   hub decides when to switch).
pub fn decide(
    running: Option<&Status>,
    hub_id: &str,
    own_build: Option<&str>,
    replace: bool,
) -> anyhow::Result<Decision> {
    let Some(running) = running else {
        return Ok(Decision::Start);
    };
    if running.hub_id != hub_id {
        bail!(
            "the box daemon's socket answers for hub key {}, not this hub's ({hub_id}): \
             something else listens there",
            running.hub_id
        );
    }
    match own_build {
        Some(own) if replace && running.build.as_deref() != Some(own) => Ok(Decision::Replace),
        _ => Ok(Decision::Use),
    }
}

/// Runs the relay on stdin/stdout, its notes on stderr (`twr runner --stdio`).
pub async fn relay(opts: RelayOptions) -> anyhow::Result<()> {
    let io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    relay_with(opts, io, &mut |line: &str| {
        use std::io::Write;
        // A closed stderr is no reason to stop relaying.
        let _ = writeln!(std::io::stderr(), "{line}");
    })
    .await
}

/// [`relay`] over `io` (the hub's side), its notes to `note`: finds, starts or replaces the
/// daemon ([`decide`]), connects to it and copies bytes both ways until the daemon's side ends
/// (or both have). The hub's end of input is passed on (the socket's write side shut), and
/// the daemon then closes its side.
pub async fn relay_with<IO: AsyncRead + AsyncWrite + Unpin>(
    opts: RelayOptions,
    io: IO,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<()> {
    validate_box_name(&opts.box_name)?;
    let hub_id = twr_core::sealed::hub_id(&opts.hub_key);
    // Before anything else: another hub's prune keeps this build from now on.
    if let Err(e) = record_build_ref(&opts.program, &hub_id, &opts.box_name) {
        tracing::warn!(error = %format!("{e:#}"), "recording this build in the builds references failed");
    }
    let socket = &opts.paths.socket;
    let mut running = daemon_status(socket).await?;
    loop {
        match decide(running.as_ref(), &hub_id, opts.own_build(), opts.replace)? {
            Decision::Use => {}
            Decision::Start => {
                // No socket, the lock held: a daemon shutting down (it removed its socket
                // first) or starting (it takes the lock before it binds). The one shutting
                // down ends first; the one starting is decided about once it answers.
                if !lock_free(&opts.paths.lock) {
                    note("twr-box: the box daemon is shutting down or starting: waiting");
                    if let Settled::Answers(st) =
                        wait_settled(&opts, "is still shutting down or starting").await?
                    {
                        running = Some(st);
                        continue;
                    }
                }
                let started = start_daemon(&opts, note).await?;
                note(&format!(
                    "twr-box: started the box daemon {}",
                    started.build_label()
                ));
            }
            Decision::Replace => {
                let old = running.expect("a daemon to replace answered");
                stop_daemon(&opts).await?;
                let started = start_daemon(&opts, note).await?;
                note(&format!(
                    "twr-box: replaced the box daemon {} with {}",
                    old.build_label(),
                    started.build_label()
                ));
            }
        }
        break;
    }
    let (stream, _) = request(socket, Request::Connect)
        .await
        .with_context(|| format!("connecting to the box daemon on {}", socket.display()))?;
    pump(io, stream).await;
    Ok(())
}

/// Adds this relay's build to hub `hub_id`'s reference for box `box_name` in the builds
/// directory it lives in ([`twr_core::BUILD_REFS_DIR`]): `program` is
/// `<builds>/<build id>/twr` when the hub launched it. Written to a temporary file and
/// renamed, only into a builds tree no other user can write to ([`check_builds_tree`]), under
/// the references' lock ([`RefsLock`]: a hub's prune rewrites that reference too).
/// `Ok(false)` when `program` is not in a builds directory (nothing to record).
pub fn record_build_ref(program: &Path, hub_id: &str, box_name: &str) -> anyhow::Result<bool> {
    let Some(dir) = program.parent() else {
        return Ok(false);
    };
    let (Some(id), Some(builds)) = (
        dir.file_name().and_then(|n| n.to_str()),
        dir.parent()
            .filter(|b| b.file_name().is_some_and(|n| n == "builds")),
    ) else {
        return Ok(false);
    };
    if !twr_core::valid_build_id(id) {
        return Ok(false);
    }
    check_builds_tree(builds)?;
    let refs = builds.join(twr_core::BUILD_REFS_DIR);
    {
        use std::os::unix::fs::DirBuilderExt;
        match std::fs::DirBuilder::new().mode(0o700).create(&refs) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", refs.display())),
        }
    }
    owned_by_me(&refs, true)?;
    let _lock = RefsLock::take(builds, REFS_LOCK_WAIT)?;
    let path = refs.join(format!("{hub_id}-{box_name}"));
    let mut ids: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| twr_core::valid_build_id(l))
        .take(64)
        .map(str::to_string)
        .collect();
    if ids.iter().any(|i| i == id) {
        // Still touched: its age is how long its hub has not used the box.
        let f = std::fs::File::options().append(true).open(&path)?;
        f.set_modified(std::time::SystemTime::now())?;
        return Ok(true);
    }
    ids.push(id.to_string());
    let tmp = refs.join(format!("{hub_id}-{box_name}.tmp.{}", std::process::id()));
    std::fs::write(
        &tmp,
        ids.iter().map(|i| format!("{i}\n")).collect::<String>(),
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(true)
}

/// How long the relay waits for the builds references' lock ([`RefsLock`]) before it records
/// nothing: less than a prune would ([`twr_core::BUILD_REFS_LOCK_WAIT_SECS`]), as the hub
/// waits for the relay to open its channel.
const REFS_LOCK_WAIT: Duration = Duration::from_secs(5);

/// The builds references' lock ([`twr_core::BUILD_REFS_LOCK`], a directory holding its
/// writer's random token in `owner`, as the hub's prune takes it), held until dropped.
struct RefsLock {
    path: PathBuf,
    token: String,
}

impl RefsLock {
    /// Takes the lock in `builds`, waiting at most `within`. A lock older than
    /// [`twr_core::BUILD_REFS_LOCK_STALE_SECS`] (its writer died) is renamed aside atomically
    /// and checked again there: still stale, removed; fresh (another writer took over in
    /// between), put back when nothing took its place. An error when it could not be taken.
    fn take(builds: &Path, within: Duration) -> anyhow::Result<RefsLock> {
        let path = builds.join(twr_core::BUILD_REFS_LOCK);
        let token = twr_core::runner::new_nonce()?[..16].to_string();
        let aside = builds.join(format!("{}.{token}", twr_core::BUILD_REFS_LOCK));
        let stale = |p: &Path| {
            std::fs::symlink_metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > Duration::from_secs(twr_core::BUILD_REFS_LOCK_STALE_SECS))
        };
        let deadline = std::time::Instant::now() + within;
        loop {
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    std::fs::write(path.join("owner"), format!("{token}\n"))
                        .with_context(|| format!("writing {}", path.display()))?;
                    return Ok(RefsLock { path, token });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
            if stale(&path) {
                // Renamed aside (by this writer only), then: still stale, removed; fresh (taken
                // over in between), put back when nothing took its place.
                let moved = std::fs::rename(&path, &aside).is_ok();
                if moved
                    && (stale(&aside) || path.exists() || std::fs::rename(&aside, &path).is_err())
                {
                    let _ = std::fs::remove_dir_all(&aside);
                }
                continue;
            }
            if std::time::Instant::now() >= deadline {
                bail!("{} is held by another writer", path.display());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for RefsLock {
    /// Lets go of the lock, only while it is this writer's (its token): one whose stale lock
    /// another writer took over leaves the new one alone.
    fn drop(&mut self) {
        let owner = std::fs::read_to_string(self.path.join("owner")).unwrap_or_default();
        if owner.trim_end() == self.token {
            let _ = std::fs::remove_file(self.path.join("owner"));
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

/// Refuses the builds directory `builds` (`<data>/twr/runner/builds`) when another user could
/// write to it: it, `<data>/twr/runner` and `<data>/twr` must be this user's directories
/// (not symbolic links) that neither group nor others may write to ([`owned_by_me`]), and the
/// directories above them follow [`twr_core::paths::check_ancestors`]'s rules. The hub's probe
/// checks the same before it runs or uploads anything there
/// (`twr_hub::ssh_box::probe_script`).
pub fn check_builds_tree(builds: &Path) -> anyhow::Result<()> {
    let runner = builds.parent().unwrap_or(builds);
    let twr = runner.parent().unwrap_or(runner);
    twr_core::paths::check_ancestors(twr)?;
    for dir in [twr, runner, builds] {
        owned_by_me(dir, true)?;
    }
    Ok(())
}

/// Refuses `path` unless it is this user's directory (`dir`) or file, not a symbolic link,
/// that neither its group nor others may write to.
fn owned_by_me(path: &Path, dir: bool) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    let me = twr_core::paths::effective_uid();
    let why = if meta.file_type().is_symlink() {
        Some("is a symbolic link".to_string())
    } else if meta.is_dir() != dir {
        Some("is not what twr made there".to_string())
    } else if meta.uid() != me {
        Some(format!("belongs to uid {}, not {me}", meta.uid()))
    } else if meta.mode() & 0o022 != 0 {
        Some(format!(
            "other users can write to it (mode {:03o})",
            meta.mode() & 0o777
        ))
    } else {
        None
    };
    match why {
        Some(why) => bail!(
            "{} {why}: twr writes nothing there (another user could change what runs)",
            path.display()
        ),
        None => Ok(()),
    }
}

/// Stops the daemon of `opts`'s hub key and box (`twr runner --stdio --stop`, see the module
/// docs), its notes to `note`: asks it to stop and waits until nothing listens on its socket
/// and its lock is free. No daemon running is no error (a note says so); a socket that answers
/// for another hub key is.
pub async fn stop(opts: &RelayOptions, note: &mut (dyn FnMut(&str) + Send)) -> anyhow::Result<()> {
    validate_box_name(&opts.box_name)?;
    let hub_id = twr_core::sealed::hub_id(&opts.hub_key);
    let mut running = daemon_status(&opts.paths.socket).await?;
    // No socket is not no daemon: one shutting down removed its socket first, and drains its
    // runtimes and repository changes holding its machine lock; one starting holds the lock
    // before it binds its socket (it is asked to stop once it answers).
    if running.is_none() && !lock_free(&opts.paths.lock) {
        note("twr-box: the box daemon is shutting down or starting: waiting");
        match wait_settled(opts, "is still shutting down or starting").await? {
            Settled::Gone => {
                note("twr-box: the box daemon has ended");
                return Ok(());
            }
            Settled::Answers(st) => running = Some(st),
        }
    }
    let Some(running) = running else {
        note(&format!(
            "twr-box: no box daemon runs for box {} of hub key {hub_id}",
            opts.box_name
        ));
        return Ok(());
    };
    // (Only to refuse a socket that answers for another hub key.)
    decide(Some(&running), &hub_id, None, false)?;
    stop_daemon(opts).await?;
    note(&format!(
        "twr-box: stopped the box daemon {}",
        running.build_label()
    ));
    Ok(())
}

/// Copies `io` to `stream` and back. The relay ends when the daemon's side ends; the hub's
/// end of input shuts the socket's write side and the relay goes on until the daemon closes.
/// A failed copy is an end too (the hub's ssh gone, say), not an error: there is no one left
/// to tell.
async fn pump<IO: AsyncRead + AsyncWrite + Unpin>(io: IO, stream: tokio::net::UnixStream) {
    let (mut input, mut output) = tokio::io::split(io);
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let up = async {
        let r = tokio::io::copy(&mut input, &mut to_daemon).await;
        let _ = to_daemon.shutdown().await;
        r
    };
    let down = async {
        let r = tokio::io::copy(&mut from_daemon, &mut output).await;
        let _ = output.shutdown().await;
        r
    };
    tokio::pin!(up, down);
    let r = tokio::select! {
        r = &mut down => r,
        _ = &mut up => down.await,
    };
    if let Err(e) = r {
        tracing::debug!(error = %e, "the relay ends");
    }
}

/// The status of the daemon on `socket`, `None` when none listens there (no socket file, or
/// one nobody accepts on).
async fn daemon_status(socket: &Path) -> anyhow::Result<Option<Status>> {
    match request(socket, Request::Status).await {
        Ok((_, status)) => Ok(Some(status)),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(e).with_context(|| format!("asking the box daemon on {}", socket.display())),
    }
}

/// Whether connecting failed because nothing listens: no socket file, or a stale one.
fn gone(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

/// Asks the daemon to stop and waits (at most [`RelayOptions::stop_timeout`]) until nothing
/// listens on its socket and its machine lock is free (the process is gone).
async fn stop_daemon(opts: &RelayOptions) -> anyhow::Result<()> {
    match request(&opts.paths.socket, Request::Stop).await {
        Ok(_) => {}
        Err(e) if gone(&e) => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "asking the box daemon on {} to stop",
                    opts.paths.socket.display()
                )
            })
        }
    }
    wait_gone(opts, "did not stop within that of being asked to").await
}

/// Waits (at most [`RelayOptions::stop_timeout`]) until nothing listens on the daemon's socket
/// and its machine lock is free: the daemon has ended, its runtimes and repository changes
/// drained. On timeout an error saying the daemon `still` (`is still shutting down`, say).
async fn wait_gone(opts: &RelayOptions, still: &str) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + opts.stop_timeout;
    loop {
        let listening = tokio::net::UnixStream::connect(&opts.paths.socket)
            .await
            .is_ok();
        if !listening && lock_free(&opts.paths.lock) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the box daemon {still} ({:?}; its socket {}, its lock {} still held): see {}",
                opts.stop_timeout,
                opts.paths.socket.display(),
                opts.paths.lock.display(),
                opts.paths.log.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// How a daemon with its lock held and no socket answering settled ([`wait_settled`]).
enum Settled {
    /// It ended: no socket, the lock free.
    Gone,
    /// It answers on its socket now (it was starting): its status.
    Answers(Status),
}

/// Waits (at most [`RelayOptions::stop_timeout`]) until the daemon of `opts` either answers
/// on its socket (one that was starting: [`Settled::Answers`]) or has ended, its lock free
/// ([`Settled::Gone`]). On timeout an error saying the daemon `still` (`is still shutting
/// down or starting`, say).
async fn wait_settled(opts: &RelayOptions, still: &str) -> anyhow::Result<Settled> {
    let deadline = tokio::time::Instant::now() + opts.stop_timeout;
    loop {
        if let Some(st) = daemon_status(&opts.paths.socket).await? {
            return Ok(Settled::Answers(st));
        }
        if lock_free(&opts.paths.lock) {
            return Ok(Settled::Gone);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the box daemon {still} ({:?}; its socket {}, its lock {} still held): see {}",
                opts.stop_timeout,
                opts.paths.socket.display(),
                opts.paths.lock.display(),
                opts.paths.log.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Whether no process holds the machine lock at `lock` (a missing file is free). Taken and
/// let go at once.
fn lock_free(lock: &Path) -> bool {
    match std::fs::OpenOptions::new().write(true).open(lock) {
        Ok(f) => f.try_lock().is_ok(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Starts the daemon (see the module docs) and waits until it answers: its status.
async fn start_daemon(
    opts: &RelayOptions,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<Status> {
    twr_core::paths::ensure_private_dir(&opts.paths.data_dir)?;
    let args = opts.daemon_args();
    let mut child = None;
    let mut launched = false;
    if opts.launcher == Launcher::Auto && cfg!(target_os = "linux") {
        let lingers = lingers().await;
        if lingers && on_path("systemd-run") {
            let unit = unit_name(&opts.hub_key, &opts.box_name);
            match systemd_run(&unit, &opts.program, &args).await {
                Ok(()) => launched = true,
                Err(e) => {
                    // One that did not answer in time may have started all the same: no
                    // second daemon then.
                    let state = probe(
                        "systemctl",
                        &["--user", "show", "-p", "ActiveState", "--value", &unit],
                    )
                    .await;
                    let kills = kills_user_processes().await;
                    let (started, says) =
                        after_systemd_run_failure(&e.to_string(), state.as_deref(), kills);
                    launched = started;
                    for line in says {
                        note(&line);
                    }
                }
            }
        } else if !lingers && kills_user_processes().await {
            note(KILLS_WARNING);
        }
    }
    if !launched {
        child = Some(
            spawn_setsid(&opts.program, &args)
                .with_context(|| format!("starting the box daemon {}", opts.program.display()))?,
        );
    }
    let started = wait_for_daemon(opts, child.as_mut()).await;
    // It runs on (or has exited): reap it whenever it exits, so it leaves no zombie behind
    // this relay.
    if let Some(mut c) = child {
        std::thread::spawn(move || {
            let _ = c.wait();
        });
    }
    started
}

/// The relay's warning when logind ends the user's processes with their last session and
/// the daemon runs in the ssh session (no lingering, or `systemd-run` failed).
const KILLS_WARNING: &str = "twr-box: warning: logind ends your processes with your last \
     session (KillUserProcesses=yes): the box daemon and its agents will end with this ssh \
     session. Run `loginctl enable-linger` on the box (systemd-run then starts the daemon \
     outside the session)";

/// The transient unit a start runs the daemon of hub key `hub_key` and box `box_name` as:
/// `twr-box-<hash>-<random>`, a new name each start, so a daemon of the same hub and box
/// still stopping (its unit not gone yet) never collides with the new one.
fn unit_name(hub_key: &[u8; 32], box_name: &str) -> String {
    let tag = twr_core::runner::new_nonce()
        .map(|n| n[..8].to_string())
        .unwrap_or_else(|_| std::process::id().to_string());
    format!(
        "twr-box-{}-{tag}",
        socket_hash(&twr_core::sealed::hub_id(hub_key), box_name)
    )
}

/// What a start does after `systemd-run` failed (`error`): whether the daemon was started all
/// the same (its unit's `ActiveState`, `state`, is `active` or `activating`: `systemd-run`
/// only did not answer in time), and the notes to print. Else it is started in a new session,
/// with the KillUserProcesses warning when logind ends the user's processes (`kills`).
fn after_systemd_run_failure(error: &str, state: Option<&str>, kills: bool) -> (bool, Vec<String>) {
    if matches!(state, Some("active" | "activating" | "reloading")) {
        return (
            true,
            vec![format!(
                "twr-box: systemd-run did not finish ({error}), but the box daemon's unit started"
            )],
        );
    }
    let mut says = vec![format!(
        "twr-box: warning: systemd-run could not start the box daemon ({error}); starting it \
         in a new session instead"
    )];
    if kills {
        says.push(KILLS_WARNING.to_string());
    }
    (false, says)
}

/// Waits until the daemon answers on its socket (at most [`RelayOptions::start_timeout`]),
/// failing early when `child` exits first; either failure carries the end of its log.
async fn wait_for_daemon(
    opts: &RelayOptions,
    mut child: Option<&mut std::process::Child>,
) -> anyhow::Result<Status> {
    let deadline = tokio::time::Instant::now() + opts.start_timeout;
    loop {
        match request(&opts.paths.socket, Request::Status).await {
            Ok((_, status)) => return Ok(status),
            Err(e) if gone(&e) => {}
            Err(e) => tracing::debug!(error = %e, "the box daemon does not answer yet"),
        }
        if let Some(c) = child.as_deref_mut() {
            if let Ok(Some(st)) = c.try_wait() {
                bail!(
                    "the box daemon exited ({st}) before it answered on {}{}",
                    opts.paths.socket.display(),
                    log_tail(&opts.paths.log)
                );
            }
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "the box daemon did not answer on {} within {:?}{}",
                opts.paths.socket.display(),
                opts.start_timeout,
                log_tail(&opts.paths.log)
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// The last lines of the daemon's log at `log`, for an error: `; the end of its log <log>:`
/// and the lines, or a note that there is none.
fn log_tail(log: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let read = || -> std::io::Result<String> {
        let mut f = std::fs::File::open(log)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(len.saturating_sub(8 << 10)))?;
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    match read() {
        Ok(text) if !text.trim().is_empty() => {
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_TAIL_LINES)..];
            format!(
                "; the end of its log {}:\n{}",
                log.display(),
                tail.join("\n")
            )
        }
        _ => format!("; its log {} is empty or missing", log.display()),
    }
}

/// Starts `program args` in a new session (`setsid`), stdin, stdout and stderr `/dev/null`.
fn spawn_setsid(program: &Path, args: &[OsString]) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn setsid() -> i32;
    }
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // SAFETY: runs in the child between fork and exec, where only async-signal-safe calls
    // are allowed: setsid(2) is one, and the closure allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn()
}

/// Variables of the relay's environment that do not go to a daemon `systemd-run` starts
/// ([`systemd_run_args`]): those of the ssh connection the relay serves, which the daemon
/// outlives, and the token runner's token (never set here; never passed on either).
const NOT_PASSED_ON: &[&str] = &[
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
    crate::client::TOKEN_ENV,
];

/// The arguments of `systemd-run` that start `program args` as the transient user service
/// `unit`, with every variable of `env` (the relay's environment) passed on by name
/// (`--setenv=NAME`: `systemd-run` takes the value from its own environment, which is the
/// relay's, so no value is in the argument list), but those in [`NOT_PASSED_ON`] and names
/// that cannot be variable names (empty, holding `=`, not UTF-8). `--expand-environment=no`:
/// the service manager would otherwise expand `$VAR` and `${VAR}` (and turn `$$` into `$`) in
/// `program` and `args`, which are paths the relay resolved (R6: a literal `$` in
/// `XDG_STATE_HOME` must reach the daemon as it is, or it listens elsewhere than the relay
/// looks). `%` needs nothing: a transient unit's command line is not subject to specifier
/// expansion. A `systemd-run` older than 254 refuses the option, and the relay falls back to
/// `setsid`.
fn systemd_run_args(
    unit: &str,
    program: &Path,
    args: &[OsString],
    env: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<OsString> {
    let mut out: Vec<OsString> = [
        "--user",
        "--collect",
        "--quiet",
        "--expand-environment=no",
        "--unit",
        unit,
    ]
    .iter()
    .map(OsString::from)
    .collect();
    let mut names: Vec<String> = env
        .into_iter()
        .filter_map(|(name, _)| name.into_string().ok())
        .filter(|n| !n.is_empty() && !n.contains('=') && !NOT_PASSED_ON.contains(&n.as_str()))
        .collect();
    names.sort();
    names.dedup();
    out.extend(
        names
            .into_iter()
            .map(|n| OsString::from(format!("--setenv={n}"))),
    );
    out.push("--".into());
    out.push(program.as_os_str().to_owned());
    out.extend(args.iter().cloned());
    out
}

/// Starts `program args` as the transient user service `unit` (`systemd-run --user`) with the
/// relay's environment ([`systemd_run_args`]): an error with what `systemd-run` said when it
/// fails or does not finish within [`PROBE_TIMEOUT`].
async fn systemd_run(unit: &str, program: &Path, args: &[OsString]) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new("systemd-run");
    cmd.args(systemd_run_args(unit, program, args, std::env::vars_os()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("it did not finish within {PROBE_TIMEOUT:?}"))??;
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        bail!("{}: {}", out.status, why.trim());
    }
    Ok(())
}

/// The trimmed stdout of `program args` when it succeeds within [`PROBE_TIMEOUT`]; `None`
/// otherwise (missing, failed, too slow).
async fn probe(program: &str, args: &[&str]) -> Option<String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(PROBE_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether logind keeps this user's processes after their last session (`Linger=yes`).
async fn lingers() -> bool {
    let uid = twr_core::paths::effective_uid().to_string();
    probe("loginctl", &["show-user", &uid, "-p", "Linger"])
        .await
        .is_some_and(|out| out == "Linger=yes")
}

/// Whether logind ends a user's processes with their last session (`KillUserProcesses`).
async fn kills_user_processes() -> bool {
    probe(
        "busctl",
        &[
            "get-property",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "KillUserProcesses",
        ],
    )
    .await
    .is_some_and(|out| out == "b true")
}

/// Whether an executable file `name` is in a directory of `$PATH`.
fn on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|d| {
        std::fs::metadata(d.join(name))
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| OsString::from(v))
        }
    }

    const KEY: [u8; 32] = [3u8; 32];

    /// A daemon that is starting, as the relay sees one: its machine lock held at once, its
    /// socket answering (as a daemon: the status line of `KEY`, then nothing) only after
    /// `after`. A `stop` request lets go of both. The requests it was made, in order.
    fn starting_daemon(
        paths: &DaemonPaths,
        after: Duration,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<Request>>> {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let lock = std::fs::File::create(&paths.lock).unwrap();
        lock.lock().unwrap();
        let (socket, log) = (paths.socket.clone(), std::sync::Arc::clone(&seen));
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let line = crate::daemon::read_line(&mut s, 256).await.unwrap();
                let req = line.as_deref().and_then(Request::parse).unwrap();
                log.lock().unwrap().push(req);
                s.write_all(Status::of(&KEY).line().as_bytes())
                    .await
                    .unwrap();
                if req == Request::Stop {
                    drop(listener);
                    let _ = std::fs::remove_file(&socket);
                    drop(lock);
                    return;
                }
            }
        });
        seen
    }

    fn quick_opts(dir: &Path) -> RelayOptions {
        RelayOptions {
            box_name: "gpu".into(),
            hub_key: KEY,
            replace: false,
            paths: DaemonPaths {
                socket: dir.join("d.sock"),
                data_dir: dir.join("data"),
                lock: dir.join("box.lock"),
                log: dir.join("data/daemon.log"),
            },
            // (Nothing to start: a start would fail.)
            program: dir.join("no-such-twr"),
            launcher: Launcher::Setsid,
            start_timeout: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(10),
            build: None,
        }
    }

    /// A daemon that holds its lock but does not answer yet is starting, not ending:
    /// `--stop` waits until it answers, asks it to stop, and waits for it to end.
    #[tokio::test]
    async fn stop_stops_a_daemon_that_was_starting() {
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let seen = starting_daemon(&opts.paths, Duration::from_millis(300));
        let mut notes = Vec::new();
        stop(&opts, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap();
        assert!(notes[0].contains("shutting down or starting"), "{notes:?}");
        assert!(
            notes.last().unwrap().contains("stopped the box daemon"),
            "{notes:?}"
        );
        assert_eq!(seen.lock().unwrap().last(), Some(&Request::Stop));
        assert!(lock_free(&opts.paths.lock));
    }

    /// A relay that finds a daemon starting uses it once it answers (no second daemon).
    #[tokio::test]
    async fn a_relay_uses_a_daemon_that_was_starting() {
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let seen = starting_daemon(&opts.paths, Duration::from_millis(300));
        let (io, _hub) = tokio::io::duplex(1024);
        let mut notes = Vec::new();
        relay_with(opts, io, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap();
        assert!(!notes.iter().any(|n| n.contains("started")), "{notes:?}");
        assert_eq!(seen.lock().unwrap().last(), Some(&Request::Connect));
    }

    /// A daemon that removed its socket but still holds its machine lock (draining) is not
    /// gone: `--stop` waits for the lock, says so, and fails when it outlasts the stop
    /// timeout; with no lock held, nothing runs.
    #[tokio::test]
    async fn stop_waits_for_a_daemon_that_is_shutting_down() {
        let dir = tempfile::tempdir().unwrap();
        let paths = DaemonPaths {
            socket: dir.path().join("d.sock"),
            data_dir: dir.path().join("data"),
            lock: dir.path().join("box.lock"),
            log: dir.path().join("data/daemon.log"),
        };
        let opts = |stop_timeout: Duration| RelayOptions {
            box_name: "gpu".into(),
            hub_key: KEY,
            replace: false,
            paths: paths.clone(),
            program: dir.path().join("twr"),
            launcher: Launcher::Setsid,
            start_timeout: Duration::from_secs(1),
            stop_timeout,
            build: None,
        };
        let mut notes = Vec::new();
        stop(&opts(Duration::from_secs(1)), &mut |l: &str| {
            notes.push(l.to_string())
        })
        .await
        .unwrap();
        assert!(notes[0].contains("no box daemon runs"), "{notes:?}");
        // The lock held, as by a daemon draining.
        let held = std::fs::File::create(&paths.lock).unwrap();
        held.lock().unwrap();
        let mut notes = Vec::new();
        let e = stop(&opts(Duration::from_millis(300)), &mut |l: &str| {
            notes.push(l.to_string())
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(e.contains("is still shutting down"), "{e}");
        assert!(
            notes[0].contains("shutting down or starting: waiting"),
            "{notes:?}"
        );
        // Released while it waits: done.
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let start = std::time::Instant::now();
        let mut notes = Vec::new();
        stop(&opts(Duration::from_secs(10)), &mut |l: &str| {
            notes.push(l.to_string())
        })
        .await
        .unwrap();
        assert!(start.elapsed() >= Duration::from_millis(250));
        assert_eq!(notes.last().unwrap(), "twr-box: the box daemon has ended");
        release.join().unwrap();
    }

    /// `TWR_BOX_LAUNCHER=setsid` forces the setsid launch; anything else is the usual one.
    #[test]
    fn launcher_from_env() {
        use std::ffi::OsStr;
        assert_eq!(
            Launcher::from_env(Some(OsStr::new("setsid"))),
            Launcher::Setsid
        );
        for v in [Some("systemd-run"), Some(""), Some("SETSID"), None] {
            assert_eq!(
                Launcher::from_env(v.map(OsStr::new)),
                Launcher::Auto,
                "{v:?}"
            );
        }
    }

    /// The paths follow `XDG_STATE_HOME` / `XDG_DATA_HOME`, else `HOME`; the socket is
    /// named by the hub id and box, the data directory by both in clear, the lock is the
    /// runner's machine lock of hub `ssh://<hub id>`.
    #[test]
    fn daemon_paths() {
        let id = twr_core::sealed::hub_id(&KEY);
        let p = DaemonPaths::resolve_in(&KEY, "gpu", env(&[("HOME", "/home/u")])).unwrap();
        let hash = socket_hash(&id, "gpu");
        assert_eq!(hash.len(), 16);
        assert_eq!(
            p.socket,
            PathBuf::from(format!("/home/u/.local/state/twr/box-daemons/{hash}.sock"))
        );
        assert_eq!(
            p.data_dir,
            PathBuf::from(format!("/home/u/.local/share/twr/box-daemons/{id}-gpu"))
        );
        assert_eq!(p.log, p.data_dir.join("daemon.log"));
        assert_eq!(
            Some(p.lock.clone()),
            machine_lock_path_in(&format!("ssh://{id}"), "gpu", env(&[("HOME", "/home/u")]))
        );
        assert!(p.lock.starts_with("/home/u/.local/state/twr/runner-locks"));
        // Short enough for macOS's socket paths under a usual home.
        assert!(p.socket.as_os_str().len() < 80);

        let xdg = DaemonPaths::resolve_in(
            &KEY,
            "gpu",
            env(&[
                ("HOME", "/home/u"),
                ("XDG_STATE_HOME", "/srv/state"),
                ("XDG_DATA_HOME", "/srv/data"),
            ]),
        )
        .unwrap();
        assert!(xdg.socket.starts_with("/srv/state/twr/box-daemons"));
        assert!(xdg.data_dir.starts_with("/srv/data/twr/box-daemons"));
        assert!(xdg.lock.starts_with("/srv/state/twr/runner-locks"));
        assert_eq!(xdg.socket.file_name(), p.socket.file_name());

        // Empty or relative values count as unset.
        let odd = DaemonPaths::resolve_in(
            &KEY,
            "gpu",
            env(&[
                ("HOME", "/home/u"),
                ("XDG_STATE_HOME", ""),
                ("XDG_DATA_HOME", "rel/data"),
            ]),
        )
        .unwrap();
        assert_eq!(odd, p);

        // Another box, another hub key: other paths.
        let other = DaemonPaths::resolve_in(&KEY, "gpu2", env(&[("HOME", "/home/u")])).unwrap();
        assert_ne!(other.socket, p.socket);
        assert_ne!(other.lock, p.lock);
        let other =
            DaemonPaths::resolve_in(&[4u8; 32], "gpu", env(&[("HOME", "/home/u")])).unwrap();
        assert_ne!(other.socket, p.socket);
        assert_ne!(other.data_dir, p.data_dir);
        assert_ne!(other.lock, p.lock);

        assert!(DaemonPaths::resolve_in(&KEY, "gpu", env(&[])).is_err());
        assert!(DaemonPaths::resolve_in(&KEY, "a/b", env(&[("HOME", "/home/u")])).is_err());
    }

    #[test]
    fn the_daemon_gets_every_path_as_an_argument() {
        let mut o = RelayOptions::new("gpu", KEY, false).unwrap();
        o.paths = DaemonPaths::resolve_in(&KEY, "gpu", env(&[("HOME", "/home/u")])).unwrap();
        let args: Vec<String> = o
            .daemon_args()
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let hex = hex_of(&KEY);
        let want = [
            "runner",
            "--daemon",
            "--box=gpu",
            "--hub-key",
            hex.as_str(),
            "--daemon-socket",
            o.paths.socket.to_str().unwrap(),
            "--data-dir",
            o.paths.data_dir.to_str().unwrap(),
            "--lock",
            o.paths.lock.to_str().unwrap(),
            "--log",
            o.paths.log.to_str().unwrap(),
        ];
        assert_eq!(args, want);
        assert_eq!(twr_core::sealed::parse_hub_public(&hex).unwrap(), KEY);
    }

    #[test]
    fn what_the_relay_does_with_the_daemon_it_finds() {
        let id = "fedcba9876543210";
        let status = |build: Option<&str>, protocol: u32| Status {
            build: build.map(str::to_string),
            protocol,
            hub_id: id.into(),
        };
        let me = Some("0123456789abcdef");
        let same = status(me, 6);
        let other = status(Some("aaaaaaaaaaaaaaaa"), 6);
        let other_protocol = status(Some("aaaaaaaaaaaaaaaa"), 5);
        let unknown = status(None, 6);
        for replace in [false, true] {
            assert_eq!(decide(None, id, me, replace).unwrap(), Decision::Start);
            assert_eq!(decide(Some(&same), id, me, replace).unwrap(), Decision::Use);
            // Without our own build nothing tells them apart.
            assert_eq!(
                decide(Some(&other), id, None, replace).unwrap(),
                Decision::Use
            );
        }
        assert_eq!(decide(Some(&other), id, me, false).unwrap(), Decision::Use);
        assert_eq!(
            decide(Some(&other_protocol), id, me, false).unwrap(),
            Decision::Use
        );
        assert_eq!(
            decide(Some(&unknown), id, me, false).unwrap(),
            Decision::Use
        );
        assert_eq!(
            decide(Some(&other), id, me, true).unwrap(),
            Decision::Replace
        );
        assert_eq!(
            decide(Some(&other_protocol), id, me, true).unwrap(),
            Decision::Replace
        );
        assert_eq!(
            decide(Some(&unknown), id, me, true).unwrap(),
            Decision::Replace
        );
        let foreign = Status {
            hub_id: "0000000000000000".into(),
            ..same
        };
        let e = decide(Some(&foreign), id, me, true)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not this hub's (fedcba9876543210)"), "{e}");
    }

    /// `systemd-run` gets the relay's environment by name, never a value, without the ssh
    /// connection's variables, the token's, or names that cannot be variable names.
    #[test]
    fn systemd_run_passes_the_environment_on_by_name() {
        use std::os::unix::ffi::OsStringExt;
        let env = [
            ("PATH", "/home/u/.local/bin:/usr/bin"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("HOME", "/home/u"),
            ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22"),
            ("SSH_CLIENT", "1.2.3.4 5 22"),
            ("SSH_TTY", "/dev/pts/3"),
            ("SSH_ORIGINAL_COMMAND", "twr runner --stdio"),
            ("TWR_RUNNER_TOKEN", "tok"),
            ("SSH_AUTH_SOCK", "/tmp/agent"),
            ("", "empty"),
            ("A=B", "c"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        let bad = (OsString::from_vec(b"BAD\xff".to_vec()), OsString::from("x"));
        let args = systemd_run_args(
            "twr-box-0123456789abcdef",
            Path::new("/home/u/twr"),
            &["runner".into(), "--daemon".into()],
            env.into_iter().chain([bad]),
        );
        let args: Vec<String> = args.into_iter().map(|a| a.into_string().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--collect",
                "--quiet",
                "--expand-environment=no",
                "--unit",
                "twr-box-0123456789abcdef",
                "--setenv=ANTHROPIC_API_KEY",
                "--setenv=HOME",
                "--setenv=PATH",
                "--setenv=SSH_AUTH_SOCK",
                "--",
                "/home/u/twr",
                "runner",
                "--daemon",
            ]
        );
        let all = args.join(" ");
        assert!(
            !all.contains("sk-secret") && !all.contains("/usr/bin"),
            "{all}"
        );
        // Paths with `$` and `%` go as they are (the manager is told not to expand them).
        let odd = "/srv/${UNSET}/$HOME/%h/$$/x.sock";
        let args = systemd_run_args(
            "u",
            Path::new("/b/$X%n/twr"),
            &["--daemon-socket".into(), odd.into()],
            [],
        );
        let tail: Vec<&str> = args[args.len() - 3..]
            .iter()
            .map(|a| a.to_str().unwrap())
            .collect();
        assert_eq!(tail, ["/b/$X%n/twr", "--daemon-socket", odd]);
    }

    /// The real `systemd-run --user` with [`systemd_run_args`] hands the service its
    /// arguments as they are: `$`, `${...}`, `$$` and `%` unexpanded. Skipped where no user
    /// manager answers.
    #[cfg(target_os = "linux")]
    #[test]
    fn systemd_runs_the_daemon_with_its_arguments_unexpanded() {
        let usable = std::process::Command::new("systemd-run")
            .args(["--user", "--quiet", "--collect", "--wait", "true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !usable {
            eprintln!("skipped: `systemd-run --user` does not work here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("arg");
        let odd = "/srv/${UNSET}/$HOME/%h/$$/x.sock";
        let unit = format!("twr-test-{}-{}", std::process::id(), rand_tag());
        let args = systemd_run_args(
            &unit,
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                "printf %s \"$1\" > \"$2\"".into(),
                "sh".into(),
                odd.into(),
                out.as_os_str().to_owned(),
            ],
            [],
        );
        let ran = std::process::Command::new("systemd-run")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            ran.status.success(),
            "{}",
            String::from_utf8_lossy(&ran.stderr)
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let got = loop {
            if let Ok(s) = std::fs::read_to_string(&out) {
                if !s.is_empty() {
                    break s;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the service did not run"
            );
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(got, odd);
    }

    fn rand_tag() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }

    /// The relay records its build in its hub's reference for its box, once, only when it
    /// runs from a builds directory, and only in a builds tree no other user can write to (a
    /// group-writable one, one through a symbolic link, one reached through a link whose
    /// target lies under a directory every user may write to: refused; another user's
    /// directory cannot be made here without root, and is the same owner check).
    #[test]
    fn the_relay_records_its_build_for_its_hub() {
        let dir = tempfile::tempdir().unwrap();
        let builds = dir.path().join("twr/runner/builds");
        let prog = |id: &str| builds.join(id).join("twr");
        let refs = builds.join(twr_core::BUILD_REFS_DIR);
        std::fs::create_dir_all(builds.join("0123456789abcdef")).unwrap();
        std::fs::create_dir_all(builds.join("fedcba9876543210")).unwrap();
        // (As twr makes them, whatever this test's umask.)
        let mode = |p: &Path, m: u32| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        for d in ["twr", "twr/runner", "twr/runner/builds"] {
            mode(&dir.path().join(d), 0o700);
        }
        // A builds tree another user could write to: nothing written.
        mode(&builds, 0o770);
        let e = record_build_ref(&prog("0123456789abcdef"), "aaaaaaaaaaaaaaaa", "gpu")
            .unwrap_err()
            .to_string();
        assert!(e.contains("other users can write to it"), "{e}");
        assert!(!refs.exists());
        mode(&builds, 0o700);
        let real = dir.path().join("real-runner");
        std::fs::rename(dir.path().join("twr/runner"), &real).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("twr/runner")).unwrap();
        let e = record_build_ref(&prog("0123456789abcdef"), "aaaaaaaaaaaaaaaa", "gpu")
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a symbolic link"), "{e}");
        std::fs::remove_file(dir.path().join("twr/runner")).unwrap();
        std::fs::rename(&real, dir.path().join("twr/runner")).unwrap();
        // Reached through a symbolic link of the user's whose target lies under a directory
        // every user may write to: what is above the target is checked too.
        let open = dir.path().join("open");
        std::fs::create_dir_all(&open).unwrap();
        mode(&open, 0o777);
        std::fs::rename(dir.path().join("twr"), open.join("twr")).unwrap();
        let via = dir.path().join("via");
        std::os::unix::fs::symlink("open", &via).unwrap();
        let through = via.join("twr/runner/builds/0123456789abcdef/twr");
        let e = record_build_ref(&through, "aaaaaaaaaaaaaaaa", "gpu")
            .unwrap_err()
            .to_string();
        assert!(e.contains(&open.display().to_string()), "{e}");
        assert!(!open
            .join("twr/runner/builds")
            .join(twr_core::BUILD_REFS_DIR)
            .exists());
        std::fs::rename(open.join("twr"), dir.path().join("twr")).unwrap();
        // The references' lock held by another writer: nothing recorded (after a while); one
        // left long ago is removed.
        let lock = builds.join(twr_core::BUILD_REFS_LOCK);
        std::fs::create_dir(&lock).unwrap();
        let e = record_build_ref(&prog("0123456789abcdef"), "aaaaaaaaaaaaaaaa", "gpu")
            .unwrap_err()
            .to_string();
        assert!(e.contains("is held by another writer"), "{e}");
        std::fs::File::open(&lock)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();
        let read = |n: &str| std::fs::read_to_string(refs.join(n)).unwrap();
        assert!(record_build_ref(&prog("0123456789abcdef"), "aaaaaaaaaaaaaaaa", "gpu").unwrap());
        assert!(!lock.exists(), "let go");
        // Its writer's own: a lock another writer took over in the meantime stays.
        let mine = RefsLock::take(&builds, Duration::from_secs(1)).unwrap();
        std::fs::write(lock.join("owner"), "someone else\n").unwrap();
        drop(mine);
        assert!(lock.exists(), "another writer's lock stays");
        std::fs::remove_dir_all(&lock).unwrap();
        assert!(record_build_ref(&prog("0123456789abcdef"), "aaaaaaaaaaaaaaaa", "gpu").unwrap());
        assert!(record_build_ref(&prog("fedcba9876543210"), "aaaaaaaaaaaaaaaa", "gpu").unwrap());
        assert!(record_build_ref(&prog("fedcba9876543210"), "bbbbbbbbbbbbbbbb", "gpu").unwrap());
        assert_eq!(
            read("aaaaaaaaaaaaaaaa-gpu"),
            "0123456789abcdef\nfedcba9876543210\n"
        );
        assert_eq!(read("bbbbbbbbbbbbbbbb-gpu"), "fedcba9876543210\n");
        let names: Vec<String> = std::fs::read_dir(&refs)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        // Not a build's place: nothing.
        let other = dir.path().join("bin/twr");
        assert!(!record_build_ref(&other, "aaaaaaaaaaaaaaaa", "gpu").unwrap());
        let not_id = builds.join("x").join("twr");
        assert!(!record_build_ref(&not_id, "aaaaaaaaaaaaaaaa", "gpu").unwrap());
    }

    /// Each start gets a unit name of its own (a stopping daemon's unit never collides); a
    /// `systemd-run` that failed but whose unit runs started the daemon (no second one); one
    /// whose unit does not run falls back, with the KillUserProcesses warning when logind
    /// would end it with the session.
    #[test]
    fn a_failed_systemd_run_is_checked_before_the_fallback() {
        let a = unit_name(&KEY, "gpu");
        let b = unit_name(&KEY, "gpu");
        assert_ne!(a, b);
        let hash = socket_hash(&twr_core::sealed::hub_id(&KEY), "gpu");
        assert!(a.starts_with(&format!("twr-box-{hash}-")), "{a}");
        let (started, says) = after_systemd_run_failure("timed out", Some("activating"), true);
        assert!(started);
        assert!(says[0].contains("unit started"), "{says:?}");
        let (started, says) = after_systemd_run_failure("exit 1", Some("inactive"), true);
        assert!(!started);
        assert!(says[0].contains("in a new session instead"), "{says:?}");
        assert!(says[1].contains("KillUserProcesses=yes"), "{says:?}");
        let (started, says) = after_systemd_run_failure("exit 1", None, false);
        assert!(!started && says.len() == 1, "{says:?}");
    }

    #[test]
    fn the_log_tail_shows_the_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("daemon.log");
        assert!(log_tail(&log).contains("is empty or missing"));
        let text: String = (0..50).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&log, text).unwrap();
        let t = log_tail(&log);
        assert!(t.contains("line 49") && t.contains("line 30"), "{t}");
        assert!(!t.contains("line 29\n"), "{t}");
    }
}

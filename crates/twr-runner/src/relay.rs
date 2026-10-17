//! The relay of an SSH box (`twr runner --stdio`): what the hub's `ssh` runs on the box, or a
//! command box's command (`docker exec -i <container> sh -c ...`). It
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
//! session; it warns too when `KillUserProcesses` can't be read on a box with logind
//! (`loginctl` on `PATH`), since it may be on ([`kills_warning`]). Each probe is bounded by
//! [`PROBE_TIMEOUT`] and leaves nothing running (`twr_adapters::process::run_bounded`); for lingering, one that
//! fails means "no". The relay then waits for the daemon to answer on its socket (at most
//! [`RelayOptions::start_timeout`]), or fails with the end of its log.
//!
//! On macOS the daemon is started as a launchd job in the user's GUI session instead
//! ([`launchd`]: `launchctl bootstrap gui/<uid>`, a new label each start), where the agents it
//! runs can read the user's login keychain (Claude Code's login is there); from ssh they
//! can't. With no one logged in at the Mac's screen (no GUI session), or when launchd does not
//! take the job, it is started in a new session, with a note that names `twr box restart
//! <box>`.
//!
//! `TWR_BOX_LAUNCHER=setsid` in the relay's environment ([`LAUNCHER_ENV`]) always starts the
//! daemon in a new session, without probing logind or trying `systemd-run` or launchd: for
//! tests (a daemon a test can find and stop) and for troubleshooting.
//! `TWR_BOX_LAUNCHER=launchd` always starts it with launchd, never in a new session instead (an
//! error when that fails, and off macOS).
//!
//! A daemon started in a new session gets the relay's environment but `XPC_SERVICE_NAME` and
//! `INVOCATION_ID`, which tell a daemon that launchd or systemd started it
//! ([`crate::daemon::own_launch`]).
//!
//! # One start at a time
//!
//! A relay that is to start, replace or restart the daemon first takes the daemon's start
//! lock ([`DaemonPaths::start_lock`], `<hash>.start.lock` beside the socket, `flock`), and
//! holds it while it checks the daemon again, cleans up (launchd: jobs of this box that ended,
//! plists a relay left), starts it and waits for it to answer (or fail): two relays never start
//! two daemons, nor clean up each other's job. One that finds the daemon as it wants it uses it
//! without the lock.
//!
//! `--restart` ([`RelayOptions::restart`]) stops the daemon that answers, whatever its build,
//! and starts a new one, under that lock; then relays as usual (`twr box restart`, after a
//! login at a Mac's screen: the new daemon runs in the GUI session).
//!
//! Stopping a daemon may take its whole shutdown ([`STOP_TIMEOUT`], 330 s: a repository change
//! it runs is finished first), and a relay may wait for another's start before its own: the
//! longest a relay that replaces or restarts the daemon takes before it relays is
//! [`START_WORST`], that of any other relay (which may still wait for another's start, for a
//! daemon that is shutting down, and start one) [`PLAIN_START_WORST`] ([`relay_worst`]: every
//! `launchctl` or probe call of a start is bounded too, [`LAUNCH_BUDGET`], and so is each
//! request to the daemon's socket); the hub's deadline to open the channel of each attempt
//! covers the one it may meet. While it waits, the relay says so every [`PROGRESS_EVERY`],
//! with how long it has: the hub shows these lines as the box's notes, and `twr box restart`
//! prints them.
//!
//! # A hub that goes away
//!
//! The hub sends nothing before the daemon's hello, so the relay reads its input from its start
//! until it relays ([`HubWatch`]) to see it end: the hub gave up on the attempt (its ssh or
//! command killed after a timeout, say). Once it has, the relay starts or stops nothing more.
//! The status requests, the wait for another relay's start, for a daemon to settle, the
//! request that the daemon stop and the wait for it to end, and the request for the daemon's
//! connection are left where they are, and the relay ends with an error. A start already under way is not cut short (a launchd job or a
//! systemd unit half made, a daemon without its reaper): it runs to its end, and the relay then
//! ends instead of relaying; the daemon it started serves the next connection. What the hub
//! sent meanwhile goes to the daemon first ([`pump`]), at most [`EARLY_MAX`] bytes of it.
//!
//! # Stopping the daemon, and its status
//!
//! `twr runner --stdio --stop` ([`stop`]) asks the daemon of the hub key and box to stop and
//! waits until it has (at most [`RelayOptions::stop_timeout`]): its runtimes end, its data
//! directory and worktrees stay. With no daemon running, it says so and succeeds. No socket is
//! not proof of that: a daemon removes its socket first and then drains, holding its machine
//! lock, so `--stop` (and a relay about to start a daemon) waits for that lock, within the
//! same timeout. `twr runner --stdio --status` ([`status`]) only prints the daemon's status
//! line, or `no daemon`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{machine_lock_path_in, validate_box_name};
use crate::daemon::{request, Request, Status};

pub mod launchd;

/// How long each probe of the box's session setup (`loginctl`, `busctl`, `systemd-run`) may
/// take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a started daemon may take to answer on its socket ([`RelayOptions::start_timeout`]
/// by default).
pub const START_TIMEOUT: Duration = Duration::from_secs(15);

/// How long a daemon asked to stop may take to end ([`RelayOptions::stop_timeout`] by
/// default): its whole shutdown, a repository change it finishes first included
/// ([`crate::client::SHUTDOWN_BUDGET`], 330 s).
pub const STOP_TIMEOUT: Duration = crate::client::SHUTDOWN_BUDGET;

/// How often a relay that waits for the daemon (to stop, to settle) or for another relay's
/// start says again that it does, and for how long it has ([`RelayOptions::progress_every`] by
/// default): the hub shows these notes, and `twr box restart` prints them.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(30);

/// The longest the launcher's own calls take before the daemon is waited for: launchd's
/// ([`launchd::CALLS_BUDGET`]), or the probes and `systemd-run` of a start on Linux (at most
/// four, each [`PROBE_TIMEOUT`] and `twr_adapters::process::REAP_LIMIT`).
pub const LAUNCH_BUDGET: Duration = {
    let systemd = 4 * (PROBE_TIMEOUT.as_secs() + twr_adapters::process::REAP_LIMIT.as_secs());
    let launchd = launchd::CALLS_BUDGET.as_secs();
    Duration::from_secs(if launchd > systemd { launchd } else { systemd })
};

/// The longest one request to the daemon's socket takes ([`crate::daemon::REQUEST_TIMEOUT`]),
/// in seconds: each wait below may end with one that began just before its deadline.
const REQUEST_SECS: u64 = crate::daemon::REQUEST_TIMEOUT.as_secs();

/// The longest one relay's start of the daemon under its start lock takes, with stop timeout
/// `stop` and start timeout `start` (what another relay waits for, [`RelayOptions::start_lock_wait`]):
/// the daemon's status asked again under the lock (a request), waiting for a daemon that is
/// shutting down or starting to settle (`stop`, and the request it may end with), stopping it
/// (a restart or a replace: the `stop` request, then `stop`), the launcher's calls
/// ([`LAUNCH_BUDGET`]), and the new daemon's answer (`start`, and the request it may end with).
pub const fn start_work(stop: Duration, start: Duration) -> Duration {
    Duration::from_secs(
        2 * stop.as_secs() + LAUNCH_BUDGET.as_secs() + start.as_secs() + 4 * REQUEST_SECS,
    )
}

/// [`start_work`] of a relay that never stops the daemon (neither `--replace` nor `--restart`:
/// a daemon that answers once it settled is used as it is): no `stop` request and no wait for
/// one to stop.
const fn start_work_without_stop(stop: Duration, start: Duration) -> Duration {
    Duration::from_secs(start_work(stop, start).as_secs() - stop.as_secs() - REQUEST_SECS)
}

/// The longest a relay takes before it relays, with stop timeout `stop` and start timeout
/// `start`, one that may stop the daemon (`stops`: `--replace`, `--restart`) or not: the
/// builds references' lock ([`REFS_LOCK_WAIT`]) and the daemon's status (a request) before the
/// start lock, another relay's start it waits for ([`start_work`]), its own start (one that
/// may stop the daemon: [`start_work`]; else less), and the request that connects it to the
/// daemon. ([`RelayOptions::start_worst`].)
pub const fn relay_worst(stop: Duration, start: Duration, stops: bool) -> Duration {
    let own = if stops {
        start_work(stop, start)
    } else {
        start_work_without_stop(stop, start)
    };
    Duration::from_secs(
        REFS_LOCK_WAIT.as_secs()
            + 2 * REQUEST_SECS
            + start_work(stop, start).as_secs()
            + own.as_secs(),
    )
}

/// The longest a relay that is to replace or restart the daemon takes before it relays, with
/// the default timeouts ([`relay_worst`]): another relay's start it waits for (one the hub gave
/// up on and that still runs, say), then its own. The hub's deadline to open the channel of an
/// attempt that replaces or restarts the daemon covers it.
pub const START_WORST: Duration = relay_worst(STOP_TIMEOUT, START_TIMEOUT, true);

/// [`START_WORST`] of a relay that neither replaces nor restarts the daemon (it may still
/// start one, after waiting for another relay's start, or for a daemon that is shutting down
/// to end): the hub's deadline to open the channel of an ordinary attempt covers it.
pub const PLAIN_START_WORST: Duration = relay_worst(STOP_TIMEOUT, START_TIMEOUT, false);

/// Lines of the daemon's log shown when it does not start.
const LOG_TAIL_LINES: usize = 20;

/// The most a relay keeps of what the hub sends before the daemon's hello ([`HubWatch`]): the
/// hub sends nothing then, so more ends the relay.
pub const EARLY_MAX: usize = 64 << 10;

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

    /// The daemon's start lock ([`StartLock`]): `<hash>.start.lock` beside its socket
    /// (`<hash>.sock`).
    pub fn start_lock(&self) -> PathBuf {
        let stem = self
            .socket
            .file_stem()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        let mut name = stem;
        name.push(".start.lock");
        self.socket.with_file_name(name)
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
    /// As the module docs say: `systemd-run` when the user lingers (Linux), a launchd job in
    /// the GUI session (macOS), else in a new session.
    Auto,
    /// Always in a new session, without probing the box's session setup (tests).
    Setsid,
    /// Always a launchd job in the GUI session, never in a new session instead: an error when
    /// that fails, and off macOS.
    Launchd,
}

/// The relay's environment variable that picks how it starts a daemon: `setsid` is
/// [`Launcher::Setsid`], `launchd` [`Launcher::Launchd`]; anything else, or nothing,
/// [`Launcher::Auto`].
pub const LAUNCHER_ENV: &str = "TWR_BOX_LAUNCHER";

impl Launcher {
    /// The launcher [`LAUNCHER_ENV`]'s value `value` asks for.
    pub fn from_env(value: Option<&std::ffi::OsStr>) -> Launcher {
        match value {
            Some(v) if v == "setsid" => Launcher::Setsid,
            Some(v) if v == "launchd" => Launcher::Launchd,
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
    /// Stop the daemon that answers, whatever its build, and start a new one (`--restart`,
    /// [`decide`]).
    pub restart: bool,
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
    /// How often a wait says again that it waits ([`PROGRESS_EVERY`]).
    pub progress_every: Duration,
    /// The build the hub launched (`--build`, the build id of this relay's executable as the
    /// hub knows it): taken as this relay's own when it cannot read its executable, so that
    /// `--replace` still tells another build's daemon apart ([`decide`]).
    pub build: Option<String>,
}

impl RelayOptions {
    /// The options for box `box_name` and hub key `hub_key`: paths from the environment
    /// ([`DaemonPaths::resolve`]), this executable as the daemon, the launcher
    /// [`LAUNCHER_ENV`] asks for, [`START_TIMEOUT`] to start, [`STOP_TIMEOUT`] to stop, no
    /// restart.
    pub fn new(box_name: &str, hub_key: [u8; 32], replace: bool) -> anyhow::Result<RelayOptions> {
        Ok(RelayOptions {
            box_name: box_name.to_string(),
            hub_key,
            replace,
            restart: false,
            paths: DaemonPaths::resolve(&hub_key, box_name)?,
            program: std::env::current_exe().context("finding this twr's executable")?,
            launcher: Launcher::from_env(std::env::var_os(LAUNCHER_ENV).as_deref()),
            start_timeout: START_TIMEOUT,
            stop_timeout: STOP_TIMEOUT,
            progress_every: PROGRESS_EVERY,
            build: None,
        })
    }

    /// How long a relay waits for another's start of the daemon ([`StartLock`]): long enough
    /// for one that waits for a daemon to settle, stops one, cleans up and starts one
    /// ([`start_work`]).
    pub fn start_lock_wait(&self) -> Duration {
        start_work(self.stop_timeout, self.start_timeout)
    }

    /// The longest this relay takes before it relays ([`relay_worst`]: [`START_WORST`] with
    /// the defaults and `--replace` or `--restart`, else [`PLAIN_START_WORST`]).
    pub fn start_worst(&self) -> Duration {
        relay_worst(
            self.stop_timeout,
            self.start_timeout,
            self.replace || self.restart,
        )
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
    /// Stop the one that answers, whatever its build, and start a new one (`--restart`).
    Restart,
}

/// What to do with the daemon that answered with `running` (`None`: none did), for the hub
/// with id `hub_id`, this relay being build `own_build`, `--replace` and `--restart` given or
/// not:
///
/// - none: start one;
/// - one of another hub key: an error (the socket is named after the hub key: whatever
///   answers there is not that hub's daemon);
/// - with `--restart`, restart it, whatever its build;
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
    restart: bool,
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
    if restart {
        return Ok(Decision::Restart);
    }
    match own_build {
        Some(own) if replace && running.build.as_deref() != Some(own) => Ok(Decision::Replace),
        _ => Ok(Decision::Use),
    }
}

/// Runs the relay on stdin/stdout, its notes on stderr (`twr runner --stdio`).
pub async fn relay(opts: RelayOptions) -> anyhow::Result<()> {
    // No handler for SIGTERM or SIGINT, unlike the runner (`crate::client::Shutdown`): their
    // default action, at any moment, is the right stop. The relay has nothing to finish: its
    // start lock is an flock the kernel releases, the daemon it starts runs on its own (a
    // session, systemd or launchd) and must outlive it, a plist it leaves is removed by the
    // next relay under that lock, and the hub's side dials again. A handler would only keep
    // the signal from ending the startup waits and the copying.
    let io = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    relay_with(opts, io, &mut |line: &str| {
        use std::io::Write;
        // A closed stderr is no reason to stop relaying.
        let _ = writeln!(std::io::stderr(), "{line}");
    })
    .await
}

/// [`relay`] over `io` (the hub's side), its notes to `note`: finds, starts, replaces or
/// restarts the daemon ([`decide`]; anything but using it under its start lock, [`StartLock`],
/// and decided again there), connects to it and copies bytes both ways until the daemon's
/// side ends (or both have). The hub's end of input is passed on (the socket's write side
/// shut), and the daemon then closes its side. Until it relays, the hub's side ending ends it
/// with an error, starting or stopping nothing more ([`HubWatch`], see the module docs).
pub async fn relay_with<IO: AsyncRead + AsyncWrite + Unpin>(
    opts: RelayOptions,
    mut io: IO,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<()> {
    validate_box_name(&opts.box_name)?;
    let hub_id = twr_core::sealed::hub_id(&opts.hub_key);
    // Before anything else: another hub's prune keeps this build from now on.
    if let Err(e) = record_build_ref(&opts.program, &hub_id, &opts.box_name) {
        tracing::warn!(error = %format!("{e:#}"), "recording this build in the builds references failed");
    }
    let socket = &opts.paths.socket;
    let decision = |running: Option<&Status>| {
        decide(
            running,
            &hub_id,
            opts.own_build(),
            opts.replace,
            opts.restart,
        )
    };
    // The hub's side, read until the relay relays: its end ends the startup (see the module
    // docs).
    let mut watch = HubWatch::new(&mut io);
    const STATUS: &str = "asked the box daemon for its status";
    let mut running = watch.during(daemon_status(socket), STATUS).await?;
    if decision(running.as_ref())? != Decision::Use {
        // Held until the daemon answers (or the start failed); decided again under it: another
        // relay may have started or replaced the daemon meanwhile.
        let _start = watch
            .during(
                StartLock::take(&opts, note),
                "waited for another relay's start of the box daemon",
            )
            .await?;
        running = watch.during(daemon_status(socket), STATUS).await?;
        loop {
            match decision(running.as_ref())? {
                Decision::Use => {}
                Decision::Start => {
                    // No socket, the lock held: a daemon shutting down (it removed its socket
                    // first) or starting (it takes the lock before it binds). The one shutting
                    // down ends first; the one starting is decided about once it answers.
                    if !lock_free(&opts.paths.lock) {
                        note(&settling_note(&opts));
                        let settled = watch
                            .during(
                                wait_settled(&opts, "is still shutting down or starting", note),
                                "waited for the box daemon to finish shutting down or starting",
                            )
                            .await?;
                        if let Settled::Answers(st) = settled {
                            running = Some(st);
                            continue;
                        }
                    }
                    let started = watch.start(&opts, note).await?;
                    note(&format!(
                        "twr-box: started the box daemon {}",
                        started.build_label()
                    ));
                }
                d @ (Decision::Replace | Decision::Restart) => {
                    let old = running.take().expect("a daemon to replace answered");
                    watch.check("was about to stop the box daemon", "stopping nothing")?;
                    note(&stopping_note(&opts, &old));
                    // (Left where it is, before or after the daemon took the request, the
                    // daemon runs on or finishes its own shutdown, which the next relay waits
                    // for.)
                    watch
                        .during(ask_stop(&opts), "asked the box daemon to stop")
                        .await?;
                    watch
                        .during(
                            wait_gone(&opts, "did not stop within that of being asked to", note),
                            "waited for the box daemon to stop",
                        )
                        .await?;
                    let started = watch.start(&opts, note).await?;
                    let (did, how) = match d {
                        Decision::Replace => ("replaced", "with"),
                        _ => ("restarted", "as"),
                    };
                    note(&format!(
                        "twr-box: {did} the box daemon {} {how} {}",
                        old.build_label(),
                        started.build_label()
                    ));
                }
            }
            break;
        }
    }
    // (A connection left before the daemon's answer is one the daemon sees end.)
    let (stream, _) = watch
        .during(
            async {
                request(socket, Request::Connect).await.with_context(|| {
                    format!("connecting to the box daemon on {}", socket.display())
                })
            },
            "asked the box daemon for a connection",
        )
        .await?;
    // What the hub sent before the daemon's hello (nothing, as it speaks second), passed on.
    let early = watch.into_early();
    pump(io, stream, early).await;
    Ok(())
}

/// The relay's reads of the hub's side from its start until it relays (see the module docs):
/// each read's bytes are kept for the daemon (at most [`EARLY_MAX`]: more is an error), and its
/// end (or a read error) marks the hub gone, which ends the startup.
struct HubWatch<'a, IO> {
    io: &'a mut IO,
    buf: Box<[u8]>,
    /// What the hub sent (passed on to the daemon first).
    early: Vec<u8>,
    /// The hub's side ended.
    gone: bool,
}

impl<'a, IO: AsyncRead + Unpin> HubWatch<'a, IO> {
    fn new(io: &'a mut IO) -> HubWatch<'a, IO> {
        HubWatch {
            io,
            buf: vec![0; 4096].into_boxed_slice(),
            early: Vec::new(),
            gone: false,
        }
    }

    /// The error of a relay whose hub's side ended while it `what`, which then does `then`.
    fn ended(what: &str, then: &str) -> anyhow::Error {
        anyhow!(
            "the hub's side of this relay ended (its connection is gone) while it {what}: {then}"
        )
    }

    /// One read's outcome: its bytes kept (an error past [`EARLY_MAX`]), or the end noted.
    fn took(&mut self, read: std::io::Result<usize>) -> anyhow::Result<()> {
        match read {
            Ok(0) | Err(_) => self.gone = true,
            Ok(n) => {
                self.early.extend_from_slice(&self.buf[..n]);
                if self.early.len() > EARLY_MAX {
                    bail!(
                        "the hub sent {} bytes before the box daemon's hello (it sends nothing \
                         then): starting nothing",
                        self.early.len()
                    );
                }
            }
        }
        Ok(())
    }

    /// `fut`, unless the hub's side ends first: then `fut` is dropped where it is (only for
    /// what may be left so: a request to the daemon, a wait) and the error says the relay ended while
    /// it `what`. Reads go on after bytes came (the hub's next ones, or its end).
    async fn during<T>(
        &mut self,
        fut: impl std::future::Future<Output = anyhow::Result<T>>,
        what: &str,
    ) -> anyhow::Result<T> {
        if self.gone {
            return Err(Self::ended(what, "starting nothing"));
        }
        tokio::pin!(fut);
        loop {
            tokio::select! {
                biased;
                r = &mut fut => return r,
                // (A read cut short here keeps what it read for the next one: `Stdin`'s, a
                // duplex's.)
                read = self.io.read(&mut self.buf) => {
                    self.took(read)?;
                    if self.gone {
                        return Err(Self::ended(what, "starting nothing"));
                    }
                }
            }
        }
    }

    /// An error when the hub's side has ended, by now or in what a read gives at once (without
    /// waiting): the relay `what`, and does `then`.
    fn check(&mut self, what: &str, then: &str) -> anyhow::Result<()> {
        while !self.gone {
            // (Polled once: a read not ready yet is dropped, keeping what it read for the next.)
            let Some(read) = futures::FutureExt::now_or_never(self.io.read(&mut self.buf)) else {
                return Ok(());
            };
            self.took(read)?;
        }
        Err(Self::ended(what, then))
    }

    /// Starts the daemon ([`start_daemon`]), unless the hub's side has ended: a start is never
    /// cut short (see the module docs), so it runs to its end while the reads go on, and when
    /// the hub's side ended meanwhile the relay ends instead of relaying (an error).
    async fn start(
        &mut self,
        opts: &RelayOptions,
        note: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<Status> {
        self.check("was about to start the box daemon", "starting nothing")?;
        let start = start_daemon(opts, note);
        tokio::pin!(start);
        let mut overflow = None;
        let started = loop {
            tokio::select! {
                biased;
                r = &mut start => break r,
                read = self.io.read(&mut self.buf), if !self.gone && overflow.is_none() => {
                    if let Err(e) = self.took(read) {
                        overflow = Some(e);
                    }
                }
            }
        };
        let started = started?;
        if let Some(e) = overflow {
            return Err(e);
        }
        if self.gone {
            return Err(Self::ended(
                "started the box daemon",
                "not relaying (that daemon serves the next connection)",
            ));
        }
        Ok(started)
    }

    /// What the hub sent meanwhile, for the daemon.
    fn into_early(self) -> Vec<u8> {
        self.early
    }
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
        note(&settling_note(opts));
        match wait_settled(opts, "is still shutting down or starting", note).await? {
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
    decide(Some(&running), &hub_id, None, false, false)?;
    note(&stopping_note(opts, &running));
    stop_daemon(opts, note).await?;
    note(&format!(
        "twr-box: stopped the box daemon {}",
        running.build_label()
    ));
    Ok(())
}

/// The status of the daemon of `opts`'s hub key and box (`twr runner --stdio --status`, which
/// `twr box doctor` runs), `None` when none answers on its socket (none runs, or one is
/// starting or shutting down). Nothing is started or stopped.
pub async fn status(opts: &RelayOptions) -> anyhow::Result<Option<Status>> {
    validate_box_name(&opts.box_name)?;
    daemon_status(&opts.paths.socket).await
}

/// Copies `io` to `stream` (after `early`, what was read of `io` already) and back. The relay
/// ends when the daemon's side ends; the hub's end of input shuts the socket's write side and
/// the relay goes on until the daemon closes. A failed copy is an end too (the hub's ssh gone,
/// say), not an error: there is no one left to tell.
async fn pump<IO: AsyncRead + AsyncWrite + Unpin>(
    io: IO,
    stream: tokio::net::UnixStream,
    early: Vec<u8>,
) {
    let (mut input, mut output) = tokio::io::split(io);
    let (mut from_daemon, mut to_daemon) = stream.into_split();
    let up = async {
        let r = match to_daemon.write_all(&early).await {
            Ok(()) => tokio::io::copy(&mut input, &mut to_daemon).await,
            Err(e) => Err(e),
        };
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

/// The note of a relay about to wait for a daemon with its lock held and no socket answering.
fn settling_note(opts: &RelayOptions) -> String {
    format!(
        "twr-box: the box daemon is shutting down or starting: waiting (at most {} s)",
        opts.stop_timeout.as_secs()
    )
}

/// The note of a relay about to stop daemon `running`.
fn stopping_note(opts: &RelayOptions, running: &Status) -> String {
    format!(
        "twr-box: stopping the box daemon {} (its runtimes end, and a repository change it runs \
         is finished first: at most {} s)",
        running.build_label(),
        opts.stop_timeout.as_secs()
    )
}

/// Says through `note`, every [`RelayOptions::progress_every`] of a wait that began at
/// `since` and may last `within`, that it still waits `for_what`.
struct Progress {
    since: tokio::time::Instant,
    next: tokio::time::Instant,
    every: Duration,
    within: Duration,
}

impl Progress {
    fn new(opts: &RelayOptions, within: Duration) -> Progress {
        let since = tokio::time::Instant::now();
        Progress {
            since,
            next: since + opts.progress_every,
            every: opts.progress_every,
            within,
        }
    }

    fn tick(&mut self, note: &mut (dyn FnMut(&str) + Send), for_what: &str) {
        let now = tokio::time::Instant::now();
        if now < self.next {
            return;
        }
        self.next = now + self.every;
        note(&format!(
            "twr-box: still waiting {for_what} ({} s of at most {} s)",
            (now - self.since).as_secs(),
            self.within.as_secs()
        ));
    }
}

/// Asks the daemon to stop and waits (at most [`RelayOptions::stop_timeout`]) until nothing
/// listens on its socket and its machine lock is free (the process is gone), saying through
/// `note` as it waits ([`Progress`]).
async fn stop_daemon(
    opts: &RelayOptions,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<()> {
    ask_stop(opts).await?;
    wait_gone(opts, "did not stop within that of being asked to", note).await
}

/// Asks the daemon to stop (none listening is no error).
async fn ask_stop(opts: &RelayOptions) -> anyhow::Result<()> {
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
    Ok(())
}

/// Waits (at most [`RelayOptions::stop_timeout`]) until nothing listens on the daemon's socket
/// and its machine lock is free: the daemon has ended, its runtimes and repository changes
/// drained. On timeout an error saying the daemon `still` (`is still shutting down`, say).
async fn wait_gone(
    opts: &RelayOptions,
    still: &str,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + opts.stop_timeout;
    let mut progress = Progress::new(opts, opts.stop_timeout);
    loop {
        let listening = tokio::net::UnixStream::connect(&opts.paths.socket)
            .await
            .is_ok();
        if !listening && lock_free(&opts.paths.lock) {
            return Ok(());
        }
        progress.tick(
            note,
            "for the box daemon to stop (it drains its runtimes and repository changes)",
        );
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
/// ([`Settled::Gone`]), saying through `note` as it waits ([`Progress`]). On timeout an error
/// saying the daemon `still` (`is still shutting down or starting`, say).
async fn wait_settled(
    opts: &RelayOptions,
    still: &str,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<Settled> {
    let deadline = tokio::time::Instant::now() + opts.stop_timeout;
    let mut progress = Progress::new(opts, opts.stop_timeout);
    loop {
        if let Some(st) = daemon_status(&opts.paths.socket).await? {
            return Ok(Settled::Answers(st));
        }
        if lock_free(&opts.paths.lock) {
            return Ok(Settled::Gone);
        }
        progress.tick(
            note,
            "for the box daemon to finish shutting down or starting",
        );
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

/// A relay's hold on the start of its daemon (see the module docs): an exclusive `flock` on
/// [`DaemonPaths::start_lock`] (made 0600 in the socket's directory, which is made private
/// first), let go when dropped (or when the relay ends: the lock is the open file's).
struct StartLock {
    _file: std::fs::File,
}

impl StartLock {
    /// Takes the start lock of `opts`'s daemon, waiting at most
    /// [`RelayOptions::start_lock_wait`] for another relay to let go of it (with a note that
    /// it waits).
    async fn take(
        opts: &RelayOptions,
        note: &mut (dyn FnMut(&str) + Send),
    ) -> anyhow::Result<StartLock> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = opts.paths.start_lock();
        if let Some(dir) = path.parent() {
            crate::daemon::ensure_socket_dir(dir)?;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .with_context(|| format!("opening the box daemon's start lock {}", path.display()))?;
        let within = opts.start_lock_wait();
        let deadline = tokio::time::Instant::now() + within;
        let mut said = false;
        let mut progress = Progress::new(opts, within);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(StartLock { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()))
                }
            }
            if !said {
                note(&format!(
                    "twr-box: another relay is starting the box daemon: waiting (at most {} s)",
                    within.as_secs()
                ));
                said = true;
            }
            progress.tick(note, "for another relay's start of the box daemon");
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "another relay has been starting the box daemon for {within:?} (it holds {}): \
                     see {}",
                    path.display(),
                    opts.paths.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
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
    let ctl = launchd::Launchctl::system();
    let vars = || std::env::vars_os().collect();
    let launched = match opts.launcher {
        Launcher::Setsid => false,
        Launcher::Launchd if !cfg!(target_os = "macos") => bail!(
            "{LAUNCHER_ENV}=launchd: launchd starts the box daemon on macOS only, and this box \
             runs {}",
            std::env::consts::OS
        ),
        Launcher::Launchd => launchd::start(opts, &args, true, &ctl, vars(), note).await?,
        Launcher::Auto if cfg!(target_os = "macos") => {
            launchd::start(opts, &args, false, &ctl, vars(), note).await?
        }
        Launcher::Auto if cfg!(target_os = "linux") => start_systemd(opts, &args, note).await,
        Launcher::Auto => false,
    };
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

/// On Linux ([`Launcher::Auto`]), starts the daemon (`program args`) as a transient user
/// service when the user lingers and `systemd-run` is on `PATH` (see the module docs): whether
/// it did (its unit runs). Else (or when `systemd-run` failed and its unit does not run) the
/// caller starts it in a new session, with the warnings given to `note`.
async fn start_systemd(
    opts: &RelayOptions,
    args: &[OsString],
    note: &mut (dyn FnMut(&str) + Send),
) -> bool {
    let lingers = lingers().await;
    if lingers && on_path("systemd-run") {
        let unit = unit_name(&opts.hub_key, &opts.box_name);
        match systemd_run(&unit, &opts.program, args).await {
            Ok(()) => return true,
            Err(e) => {
                // One that did not answer in time may have started all the same: no second
                // daemon then.
                let state = probe(
                    "systemctl",
                    &["--user", "show", "-p", "ActiveState", "--value", &unit],
                )
                .await;
                let warning = kills_warning(kills_user_processes().await, true);
                let (started, says) =
                    after_systemd_run_failure(&e.to_string(), state.as_deref(), warning);
                for line in says {
                    note(&line);
                }
                return started;
            }
        }
    } else if !lingers {
        if let Some(w) = kills_warning(kills_user_processes().await, on_path("loginctl")) {
            note(w);
        }
    }
    false
}

/// The relay's warning when logind ends the user's processes with their last session and
/// the daemon runs in the ssh session (no lingering, or `systemd-run` failed).
const KILLS_WARNING: &str = "twr-box: warning: logind ends your processes with your last \
     session (KillUserProcesses=yes): the box daemon and its agents will end with this ssh \
     session. Run `loginctl enable-linger` on the box (systemd-run then starts the daemon \
     outside the session)";

/// The relay's warning when logind's `KillUserProcesses` could not be read (no `busctl`, or
/// its query failed) and the daemon runs in the ssh session: it may end with it.
const KILLS_UNKNOWN_WARNING: &str = "twr-box: warning: couldn't read logind's \
     KillUserProcesses; if it is on, the box daemon and its agents end with this ssh session \
     unless you run `loginctl enable-linger` on the box";

/// What the relay warns, once, when the daemon is to run in the ssh session: `kills` is
/// logind's `KillUserProcesses` (`None`: it could not be read), `logind` whether the box has
/// logind at all (`loginctl` on `PATH`). On: [`KILLS_WARNING`]; not known on a box with
/// logind: [`KILLS_UNKNOWN_WARNING`]; off, or no logind: nothing.
fn kills_warning(kills: Option<bool>, logind: bool) -> Option<&'static str> {
    match kills {
        Some(true) => Some(KILLS_WARNING),
        None if logind => Some(KILLS_UNKNOWN_WARNING),
        _ => None,
    }
}

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
/// with `warning` ([`kills_warning`]) when there is one.
fn after_systemd_run_failure(
    error: &str,
    state: Option<&str>,
    warning: Option<&str>,
) -> (bool, Vec<String>) {
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
    if let Some(w) = warning {
        says.push(w.to_string());
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

/// The variable launchd sets to a job's label ([`crate::daemon::own_launch`]).
const LAUNCHD_JOB_ENV: &str = "XPC_SERVICE_NAME";

/// The variable systemd sets in a unit's processes ([`crate::daemon::own_launch`]).
const SYSTEMD_INVOCATION_ENV: &str = "INVOCATION_ID";

/// Starts `program args` in a new session (`setsid`), stdin, stdout and stderr `/dev/null`,
/// without [`LAUNCHD_JOB_ENV`] and [`SYSTEMD_INVOCATION_ENV`].
fn spawn_setsid(program: &Path, args: &[OsString]) -> std::io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;
    extern "C" {
        fn setsid() -> i32;
    }
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        // (What tells a daemon that launchd or systemd started it: not so.)
        .env_remove(LAUNCHD_JOB_ENV)
        .env_remove(SYSTEMD_INVOCATION_ENV)
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
/// outlives, the token runner's token (never set here; never passed on either), and those
/// that tell a daemon how it was started (systemd sets the unit's own).
const NOT_PASSED_ON: &[&str] = &[
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
    "SSH_ORIGINAL_COMMAND",
    crate::client::TOKEN_ENV,
    LAUNCHD_JOB_ENV,
    SYSTEMD_INVOCATION_ENV,
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
    // No terminal (a relay run by hand from one has it): `systemd-run` asks for none
    // (`--user` talks to the user's manager, no polkit prompt), and a wrapper on `PATH` may
    // touch it. Not the daemon's session: the service manager starts that.
    twr_adapters::process::new_session(&mut cmd);
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
/// otherwise (missing, failed, too slow, too much output). A probe
/// ([`twr_adapters::process::run_bounded`]): no terminal, and nothing it starts outlives it.
async fn probe(program: &str, args: &[&str]) -> Option<String> {
    use twr_adapters::process::{run_bounded, Bounded};
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args);
    match run_bounded(cmd, PROBE_TIMEOUT, 64 << 10, |_| false).await {
        Bounded::Exited(Some(0), out) => Some(String::from_utf8_lossy(&out).trim().to_string()),
        _ => None,
    }
}

/// Whether logind keeps this user's processes after their last session (`Linger=yes`).
async fn lingers() -> bool {
    let uid = twr_core::paths::effective_uid().to_string();
    probe("loginctl", &["show-user", &uid, "-p", "Linger"])
        .await
        .is_some_and(|out| out == "Linger=yes")
}

/// Whether logind ends a user's processes with their last session (`KillUserProcesses`):
/// `None` when it could not be read (no `busctl`, its query failed, or it said neither `b
/// true` nor `b false`).
async fn kills_user_processes() -> Option<bool> {
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
    .and_then(|out| match out.as_str() {
        "b true" => Some(true),
        "b false" => Some(false),
        _ => None,
    })
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
        draining_daemon(paths, after, Duration::ZERO)
    }

    /// [`starting_daemon`], which after a `stop` request (its socket gone at once) holds its
    /// lock for `drain` more, as a daemon finishing a repository change does.
    fn draining_daemon(
        paths: &DaemonPaths,
        after: Duration,
        drain: Duration,
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
                    tokio::time::sleep(drain).await;
                    drop(lock);
                    return;
                }
            }
        });
        seen
    }

    /// Options with short timeouts, the socket (and the start lock) in a private directory in
    /// `dir`, as the relay makes it.
    fn quick_opts(dir: &Path) -> RelayOptions {
        use std::os::unix::fs::DirBuilderExt;
        let sockets = dir.join("sockets");
        if !sockets.is_dir() {
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&sockets)
                .unwrap();
        }
        RelayOptions {
            box_name: "gpu".into(),
            hub_key: KEY,
            replace: false,
            restart: false,
            paths: DaemonPaths {
                socket: sockets.join("d.sock"),
                data_dir: dir.join("data"),
                lock: dir.join("box.lock"),
                log: dir.join("data/daemon.log"),
            },
            // (Nothing to start: a start would fail.)
            program: dir.join("no-such-twr"),
            launcher: Launcher::Setsid,
            start_timeout: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(10),
            progress_every: PROGRESS_EVERY,
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

    /// A relay that is to start the daemon while another holds its start lock waits for it
    /// (and says so), then decides again: the daemon the other started answers by then, and is
    /// used (no second one).
    #[tokio::test]
    async fn a_relay_waits_for_anothers_start_and_decides_again() {
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let held = std::fs::File::create(opts.paths.start_lock()).unwrap();
        held.lock().unwrap();
        let seen = starting_daemon(&opts.paths, Duration::from_millis(300));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(held);
        });
        let (io, _hub) = tokio::io::duplex(1024);
        let mut notes = Vec::new();
        relay_with(opts, io, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap();
        release.join().unwrap();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("twr-box: another relay is starting the box daemon: waiting"),
            "{notes:?}"
        );
        assert_eq!(seen.lock().unwrap().last(), Some(&Request::Connect));
    }

    /// `--restart` stops the daemon that answers, whatever its build (the same as this relay's
    /// here), and starts another, under the start lock, which is free again once the start
    /// failed (there is no program to start here).
    #[tokio::test]
    async fn a_restart_stops_the_daemon_then_starts_one() {
        let dir = tempfile::tempdir().unwrap();
        let opts = RelayOptions {
            restart: true,
            ..quick_opts(dir.path())
        };
        let seen = starting_daemon(&opts.paths, Duration::ZERO);
        while daemon_status(&opts.paths.socket).await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (io, _hub) = tokio::io::duplex(1024);
        let lock = opts.paths.start_lock();
        let e = relay_with(opts, io, &mut |_: &str| {}).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("starting the box daemon"),
            "{e:#}"
        );
        assert!(seen.lock().unwrap().contains(&Request::Stop));
        let f = std::fs::File::open(&lock).unwrap();
        assert!(f.try_lock().is_ok(), "the start lock was kept");
    }

    /// A restart whose daemon takes long to stop (it finishes a repository change, holding its
    /// lock) waits for it within the stop timeout, saying as it waits, before it starts one.
    #[tokio::test]
    async fn a_restart_waits_for_a_slow_shutdown_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let opts = RelayOptions {
            restart: true,
            progress_every: Duration::from_millis(100),
            ..quick_opts(dir.path())
        };
        let seen = draining_daemon(&opts.paths, Duration::ZERO, Duration::from_millis(800));
        while daemon_status(&opts.paths.socket).await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (io, _hub) = tokio::io::duplex(1024);
        let mut notes = Vec::new();
        let e = relay_with(opts, io, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap_err();
        // Stopped (there is no program to start a new one here).
        assert!(
            format!("{e:#}").contains("starting the box daemon"),
            "{e:#}"
        );
        assert!(seen.lock().unwrap().contains(&Request::Stop));
        assert!(
            notes[0].starts_with("twr-box: stopping the box daemon")
                && notes[0].contains("at most 10 s"),
            "{notes:?}"
        );
        let waiting = notes
            .iter()
            .filter(|n| n.starts_with("twr-box: still waiting for the box daemon to stop ("))
            .count();
        assert!(waiting >= 3, "{notes:?}");
    }

    /// The default budgets: a daemon's whole shutdown to stop (a repository change it finishes
    /// included), and the longest a relay may take before it relays, which the hub's opening
    /// deadline of each attempt covers: one that replaces or restarts the daemon, and one that
    /// does not (it may still wait for another relay's start, then for a daemon shutting down,
    /// and start one), every request to the daemon's socket and the builds references' lock
    /// counted.
    #[test]
    fn the_budgets_cover_a_daemons_whole_shutdown() {
        assert_eq!(STOP_TIMEOUT, Duration::from_secs(330));
        assert!(STOP_TIMEOUT > twr_core::git::GIT_CHANGE_TIMEOUT);
        assert_eq!(STOP_TIMEOUT, launchd::EXIT_TIMEOUT);
        let opts = RelayOptions::new("gpu", KEY, false).unwrap();
        assert_eq!(opts.stop_timeout, STOP_TIMEOUT);
        assert_eq!(opts.start_worst(), PLAIN_START_WORST);
        let replace = RelayOptions::new("gpu", KEY, true).unwrap();
        assert_eq!(replace.start_worst(), START_WORST);
        let restart = RelayOptions {
            restart: true,
            ..RelayOptions::new("gpu", KEY, false).unwrap()
        };
        assert_eq!(restart.start_worst(), START_WORST);
        assert!(LAUNCH_BUDGET >= launchd::CALLS_BUDGET);
        let request = crate::daemon::REQUEST_TIMEOUT;
        // Under the lock: the status again; a settle (and its last request); a stop request and
        // the stop; the launcher; the start (and its last request).
        let work = 2 * STOP_TIMEOUT + LAUNCH_BUDGET + START_TIMEOUT + 4 * request;
        assert_eq!(opts.start_lock_wait(), work);
        // Around it: the references' lock, the status before the lock, the connect after.
        let around = REFS_LOCK_WAIT + 2 * request;
        assert_eq!(START_WORST, around + 2 * work);
        assert_eq!(
            PLAIN_START_WORST,
            around + work + (work - STOP_TIMEOUT - request)
        );
        assert!(PLAIN_START_WORST > work + STOP_TIMEOUT + START_TIMEOUT + LAUNCH_BUDGET);
    }

    /// A relay whose hub went away (the end of its input: its ssh is gone) while another relay
    /// held the start lock ends there, without waiting for the lock: it starts nothing. What the
    /// hub sent before then (it sends nothing before the daemon's hello) is not lost: passed on
    /// to the daemon first.
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_does_not_queue_for_the_start_lock() {
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let held = std::fs::File::create(opts.paths.start_lock()).unwrap();
        held.lock().unwrap();
        let (io, hub) = tokio::io::duplex(1024);
        drop(hub);
        let mut notes = Vec::new();
        let start = std::time::Instant::now();
        let e = relay_with(opts, io, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("the hub's side of this relay ended"),
            "{e:#}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
        drop(held);

        // Bytes before the lock (unexpected, but kept): the daemon gets them first.
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let held = std::fs::File::create(opts.paths.start_lock()).unwrap();
        held.lock().unwrap();
        let seen = starting_daemon(&opts.paths, Duration::from_millis(300));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(held);
        });
        let (io, mut hub) = tokio::io::duplex(1024);
        hub.write_all(b"early").await.unwrap();
        relay_with(opts, io, &mut |_: &str| {}).await.unwrap();
        release.join().unwrap();
        assert_eq!(seen.lock().unwrap().last(), Some(&Request::Connect));

        // More than a relay keeps before the daemon's hello: it ends, starting nothing.
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let held = std::fs::File::create(opts.paths.start_lock()).unwrap();
        held.lock().unwrap();
        let (io, mut hub) = tokio::io::duplex(1 << 20);
        hub.write_all(&vec![b'x'; EARLY_MAX + 1]).await.unwrap();
        let e = tokio::time::timeout(
            Duration::from_secs(5),
            relay_with(opts, io, &mut |_: &str| {}),
        )
        .await
        .expect("the relay ends")
        .unwrap_err();
        assert!(
            format!("{e:#}").contains("before the box daemon's hello"),
            "{e:#}"
        );
        drop(held);
    }

    /// A relay whose hub went away while it waited for a daemon to settle (its machine lock
    /// held, no socket: shutting down or starting) ends there at once, and starts nothing
    /// once the lock is free.
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_while_a_daemon_settles_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let paths = opts.paths.clone();
        let held = std::fs::File::create(&paths.lock).unwrap();
        held.lock().unwrap();
        let (io, hub) = tokio::io::duplex(1024);
        // The hub goes away as the relay starts waiting.
        let mut hub = Some(hub);
        let mut notes = Vec::new();
        let e = tokio::time::timeout(
            Duration::from_secs(1),
            relay_with(opts, io, &mut |l: &str| {
                if l.contains("shutting down or starting") {
                    hub.take();
                }
                notes.push(l.to_string());
            }),
        )
        .await
        .expect("the relay ends within 1 s")
        .unwrap_err();
        assert!(
            format!("{e:#}").contains(
                "the hub's side of this relay ended (its connection is gone) while it waited \
                 for the box daemon to finish shutting down or starting: starting nothing"
            ),
            "{e:#}"
        );
        assert!(hub.is_none(), "{notes:?}");
        // Its start lock is free, and once the daemon's lock is, nothing starts.
        assert!(std::fs::File::open(paths.start_lock())
            .unwrap()
            .try_lock()
            .is_ok());
        drop(held);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!paths.socket.exists());
        assert!(!notes.iter().any(|n| n.contains("started")), "{notes:?}");
    }

    /// A restart whose hub went away while the old daemon was stopping (the stop asked for, its
    /// shutdown held back as a repository change holds it) ends at once: the old daemon
    /// finishes stopping by itself, no new one is started, and the start lock is free.
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_while_a_daemon_stops_starts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let opts = RelayOptions {
            restart: true,
            ..quick_opts(dir.path())
        };
        let paths = opts.paths.clone();
        let drain = Duration::from_millis(1500);
        let seen = draining_daemon(&paths, Duration::ZERO, drain);
        while daemon_status(&paths.socket).await.unwrap().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (io, hub) = tokio::io::duplex(1024);
        // Gone once the daemon took the stop request and answered it (a moment for the relay
        // to read the answer): the relay then waits for the daemon to end.
        {
            let seen = std::sync::Arc::clone(&seen);
            tokio::spawn(async move {
                while !seen.lock().unwrap().contains(&Request::Stop) {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                drop(hub);
            });
        }
        let mut notes = Vec::new();
        let start = std::time::Instant::now();
        let e = tokio::time::timeout(
            Duration::from_secs(5),
            relay_with(opts, io, &mut |l: &str| notes.push(l.to_string())),
        )
        .await
        .expect("the relay ends")
        .unwrap_err();
        assert!(start.elapsed() < drain, "it waited for the stop");
        assert!(
            format!("{e:#}").contains(
                "the hub's side of this relay ended (its connection is gone) while it waited \
                 for the box daemon to stop: starting nothing"
            ),
            "{e:#}"
        );
        assert!(seen.lock().unwrap().contains(&Request::Stop));
        assert!(std::fs::File::open(paths.start_lock())
            .unwrap()
            .try_lock()
            .is_ok());
        // The old daemon finishes stopping by itself.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !lock_free(&paths.lock) {
            assert!(
                std::time::Instant::now() < deadline,
                "the old daemon still runs"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !notes
                .iter()
                .any(|n| n.contains("started") || n.contains("restarted")),
            "{notes:?}"
        );
        assert!(!paths.socket.exists());
    }

    /// A relay whose hub went away while it started the daemon lets the start finish (never
    /// cut short), then ends instead of relaying: it never connects. The daemon it started is
    /// the one the next relay uses (one daemon, not two).
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_while_it_starts_the_daemon_does_not_relay() {
        let dir = tempfile::tempdir().unwrap();
        let marks = dir.path().join("starts");
        let program = dir.path().join("fake-twr");
        // Stands for the daemon's process: the test serves its socket once it ran.
        crate::test_support::install(
            &program,
            format!(
                "#!/bin/sh\necho started >> '{}'\nexec sleep 5\n",
                marks.display()
            ),
        );
        let opts = RelayOptions {
            program,
            start_timeout: Duration::from_secs(10),
            ..quick_opts(dir.path())
        };
        let paths = opts.paths.clone();
        let (io, hub) = tokio::io::duplex(1024);
        // Once the start is under way: the hub goes away, and the daemon answers a little
        // later.
        let daemon = {
            let (marks, paths) = (marks.clone(), paths.clone());
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
                while !marks.exists() {
                    assert!(tokio::time::Instant::now() < deadline, "no start");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                drop(hub);
                starting_daemon(&paths, Duration::from_millis(300))
            })
        };
        let mut notes = Vec::new();
        let e = tokio::time::timeout(
            Duration::from_secs(10),
            relay_with(opts.clone(), io, &mut |l: &str| notes.push(l.to_string())),
        )
        .await
        .expect("the relay ends")
        .unwrap_err();
        assert!(
            format!("{e:#}").contains(
                "the hub's side of this relay ended (its connection is gone) while it started \
                 the box daemon: not relaying"
            ),
            "{e:#}"
        );
        let seen = daemon.await.unwrap();
        assert!(
            !seen.lock().unwrap().contains(&Request::Connect),
            "{:?}",
            seen.lock().unwrap()
        );
        // The next relay uses that daemon.
        let (io, _hub) = tokio::io::duplex(1024);
        let mut notes = Vec::new();
        relay_with(opts, io, &mut |l: &str| notes.push(l.to_string()))
            .await
            .unwrap();
        assert!(!notes.iter().any(|n| n.contains("started")), "{notes:?}");
        assert_eq!(seen.lock().unwrap().last(), Some(&Request::Connect));
        assert_eq!(std::fs::read_to_string(&marks).unwrap(), "started\n");
    }

    /// A daemon that answers status requests at once and a `connect` only once `answer` is
    /// notified, then reads what comes on that connection into the returned buffer. `asked`
    /// is notified when the `connect` came.
    fn slow_to_connect_daemon(
        paths: &DaemonPaths,
        asked: std::sync::Arc<tokio::sync::Notify>,
        answer: std::sync::Arc<tokio::sync::Notify>,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        slow_to_answer_daemon(paths, Request::Connect, asked, answer)
    }

    /// [`slow_to_connect_daemon`], slow to answer `slow` (a `stop` is then not acted on).
    fn slow_to_answer_daemon(
        paths: &DaemonPaths,
        slow: Request,
        asked: std::sync::Arc<tokio::sync::Notify>,
        answer: std::sync::Arc<tokio::sync::Notify>,
    ) -> std::sync::Arc<std::sync::Mutex<Vec<u8>>> {
        let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let into = std::sync::Arc::clone(&got);
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let line = crate::daemon::read_line(&mut s, 256).await.unwrap();
                let req = line.as_deref().and_then(Request::parse).unwrap();
                if req == slow {
                    asked.notify_one();
                    answer.notified().await;
                }
                // (The relay may have left: a write to a closed connection is no failure.)
                if s.write_all(Status::of(&KEY).line().as_bytes())
                    .await
                    .is_err()
                {
                    continue;
                }
                if req == Request::Connect {
                    let mut buf = [0u8; 256];
                    while let Ok(n) = s.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        into.lock().unwrap().extend_from_slice(&buf[..n]);
                    }
                }
            }
        });
        got
    }

    /// A relay whose hub went away while the daemon was slow to answer its `connect` ends
    /// there, at once (not after the request's own timeout), and relays nothing. What the hub
    /// sent while it waited (it sends nothing then) is still the daemon's first input.
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_while_it_connects_ends_there() {
        use std::sync::Arc;
        use tokio::sync::Notify;
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let (asked, answer) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let got = slow_to_connect_daemon(&opts.paths, Arc::clone(&asked), Arc::clone(&answer));
        let (io, hub) = tokio::io::duplex(1024);
        let relay = tokio::spawn(async move { relay_with(opts, io, &mut |_: &str| {}).await });
        tokio::time::timeout(Duration::from_secs(5), asked.notified())
            .await
            .expect("the relay asks for a connection");
        drop(hub);
        let e = tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .expect("the relay ends within 1 s")
            .unwrap()
            .unwrap_err();
        assert!(
            format!("{e:#}").contains(
                "the hub's side of this relay ended (its connection is gone) while it asked the \
                 box daemon for a connection"
            ),
            "{e:#}"
        );
        answer.notify_one();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(got.lock().unwrap().is_empty());

        // Bytes from the hub while the daemon is slow to answer: the daemon gets them first.
        let dir = tempfile::tempdir().unwrap();
        let opts = quick_opts(dir.path());
        let (asked, answer) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        let got = slow_to_connect_daemon(&opts.paths, Arc::clone(&asked), Arc::clone(&answer));
        let (io, mut hub) = tokio::io::duplex(1024);
        let relay = tokio::spawn(async move { relay_with(opts, io, &mut |_: &str| {}).await });
        tokio::time::timeout(Duration::from_secs(5), asked.notified())
            .await
            .expect("the relay asks for a connection");
        hub.write_all(b"early").await.unwrap();
        // (The relay reads them while it waits.)
        tokio::time::sleep(Duration::from_millis(200)).await;
        answer.notify_one();
        // (Its end only once it relays: an end before then ends the relay, as above.)
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while got.lock().unwrap().is_empty() {
            assert!(std::time::Instant::now() < deadline, "nothing relayed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        hub.write_all(b" then").await.unwrap();
        hub.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), relay)
            .await
            .expect("the relay ends")
            .unwrap()
            .unwrap();
        assert_eq!(got.lock().unwrap().as_slice(), b"early then");
    }

    /// A relay whose hub went away while the daemon was slow to answer its `stop` (a restart)
    /// ends there, at once (not after the request's own timeout): its start lock free, and
    /// nothing started.
    #[tokio::test]
    async fn a_relay_whose_hub_went_away_while_it_asks_for_a_stop_ends_there() {
        use std::sync::Arc;
        use tokio::sync::Notify;
        let dir = tempfile::tempdir().unwrap();
        let opts = RelayOptions {
            restart: true,
            ..quick_opts(dir.path())
        };
        let paths = opts.paths.clone();
        let (asked, answer) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        slow_to_answer_daemon(
            &paths,
            Request::Stop,
            Arc::clone(&asked),
            Arc::clone(&answer),
        );
        let (io, hub) = tokio::io::duplex(1024);
        let notes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let relay = {
            let notes = Arc::clone(&notes);
            tokio::spawn(async move {
                relay_with(opts, io, &mut |l: &str| {
                    notes.lock().unwrap().push(l.to_string())
                })
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), asked.notified())
            .await
            .expect("the relay asks the daemon to stop");
        drop(hub);
        let e = tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .expect("the relay ends within 1 s")
            .unwrap()
            .unwrap_err();
        assert!(
            format!("{e:#}").contains(
                "the hub's side of this relay ended (its connection is gone) while it asked the \
                 box daemon to stop: starting nothing"
            ),
            "{e:#}"
        );
        assert!(std::fs::File::open(paths.start_lock())
            .unwrap()
            .try_lock()
            .is_ok());
        answer.notify_one();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let notes = notes.lock().unwrap();
        assert!(!notes.iter().any(|n| n.contains("started")), "{notes:?}");
    }

    /// `TWR_BOX_LAUNCHER=launchd` off macOS: an error, never a start in a new session.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn launchd_forced_off_macos_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let opts = RelayOptions {
            launcher: Launcher::Launchd,
            ..quick_opts(dir.path())
        };
        let (io, _hub) = tokio::io::duplex(1024);
        let e = relay_with(opts, io, &mut |_: &str| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("TWR_BOX_LAUNCHER=launchd") && e.contains("macOS only"),
            "{e}"
        );
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
            restart: false,
            paths: paths.clone(),
            program: dir.path().join("twr"),
            launcher: Launcher::Setsid,
            start_timeout: Duration::from_secs(1),
            stop_timeout,
            progress_every: PROGRESS_EVERY,
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

    /// `TWR_BOX_LAUNCHER=setsid` forces the setsid launch, `launchd` launchd; anything else is
    /// the usual one.
    #[test]
    fn launcher_from_env() {
        use std::ffi::OsStr;
        assert_eq!(
            Launcher::from_env(Some(OsStr::new("setsid"))),
            Launcher::Setsid
        );
        assert_eq!(
            Launcher::from_env(Some(OsStr::new("launchd"))),
            Launcher::Launchd
        );
        for v in [
            Some("systemd-run"),
            Some(""),
            Some("SETSID"),
            Some("LAUNCHD"),
            None,
        ] {
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
        // The start lock is beside the socket.
        assert_eq!(
            p.start_lock(),
            PathBuf::from(format!(
                "/home/u/.local/state/twr/box-daemons/{hash}.start.lock"
            ))
        );

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
            assert_eq!(
                decide(None, id, me, replace, false).unwrap(),
                Decision::Start
            );
            assert_eq!(
                decide(Some(&same), id, me, replace, false).unwrap(),
                Decision::Use
            );
            // Without our own build nothing tells them apart.
            assert_eq!(
                decide(Some(&other), id, None, replace, false).unwrap(),
                Decision::Use
            );
        }
        assert_eq!(
            decide(Some(&other), id, me, false, false).unwrap(),
            Decision::Use
        );
        assert_eq!(
            decide(Some(&other_protocol), id, me, false, false).unwrap(),
            Decision::Use
        );
        assert_eq!(
            decide(Some(&unknown), id, me, false, false).unwrap(),
            Decision::Use
        );
        assert_eq!(
            decide(Some(&other), id, me, true, false).unwrap(),
            Decision::Replace
        );
        assert_eq!(
            decide(Some(&other_protocol), id, me, true, false).unwrap(),
            Decision::Replace
        );
        assert_eq!(
            decide(Some(&unknown), id, me, true, false).unwrap(),
            Decision::Replace
        );
        let foreign = Status {
            hub_id: "0000000000000000".into(),
            ..same.clone()
        };
        let e = decide(Some(&foreign), id, me, true, false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not this hub's (fedcba9876543210)"), "{e}");
        // `--restart`: whatever answers for this hub key is restarted, whatever its build; none
        // is started; another hub key's is still refused.
        for running in [&same, &other, &other_protocol, &unknown] {
            for (own, replace) in [(me, false), (me, true), (None, false)] {
                assert_eq!(
                    decide(Some(running), id, own, replace, true).unwrap(),
                    Decision::Restart
                );
            }
        }
        assert_eq!(decide(None, id, me, false, true).unwrap(), Decision::Start);
        assert!(decide(Some(&foreign), id, me, false, true).is_err());
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

    #[cfg(target_os = "linux")]
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
        let on = kills_warning(Some(true), true);
        let (started, says) = after_systemd_run_failure("timed out", Some("activating"), on);
        assert!(started);
        assert!(says[0].contains("unit started"), "{says:?}");
        let (started, says) = after_systemd_run_failure("exit 1", Some("inactive"), on);
        assert!(!started);
        assert!(says[0].contains("in a new session instead"), "{says:?}");
        assert!(says[1].contains("KillUserProcesses=yes"), "{says:?}");
        let off = kills_warning(Some(false), true);
        let (started, says) = after_systemd_run_failure("exit 1", None, off);
        assert!(!started && says.len() == 1, "{says:?}");
        // Not read: warned too (it may be on).
        let unknown = kills_warning(None, true);
        let (_, says) = after_systemd_run_failure("exit 1", None, unknown);
        assert!(
            says[1].contains("couldn't read logind's KillUserProcesses"),
            "{says:?}"
        );
    }

    /// The relay warns of logind ending the daemon with the ssh session when it says so, and
    /// when it can't be read on a box with logind; only `b false`, or no logind at all, is
    /// silent.
    #[test]
    fn logind_warnings_take_an_unknown_answer_as_a_risk() {
        assert_eq!(kills_warning(Some(true), true), Some(KILLS_WARNING));
        assert_eq!(kills_warning(Some(true), false), Some(KILLS_WARNING));
        assert_eq!(kills_warning(None, true), Some(KILLS_UNKNOWN_WARNING));
        assert_eq!(kills_warning(None, false), None);
        assert_eq!(kills_warning(Some(false), true), None);
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

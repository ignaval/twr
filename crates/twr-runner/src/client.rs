//! The runner process: connects outbound to the hub over WebSocket (`ws://` or `wss://` via
//! rustls with the webpki root certificates) and serves the hub from a [`LocalBox`]. Or, as an
//! SSH box's daemon ([`RunnerOptions::daemon`], `twr runner --daemon`), takes its connections
//! from the hub on a Unix socket instead of dialing ([`crate::daemon`]): the same session runs
//! over either transport (the code below is generic over the WebSocket's byte stream), and
//! only how a connection is made and how the hub is authenticated differ.
//!
//! Connection loop: connect; authenticate ([`twr_core::runner`]: `hello` with a fresh nonce,
//! check the hub's proof in its `challenge` before sending anything else, send this runner's
//! proof, wait for `authenticated`); send the `inventory`; wait for `welcome`; drop spooled
//! frames at or below the hub's watermarks; replay the rest of the spool in order; send `synced`; then
//! stream live frames as the box spools them, while a reader task applies acks, answers pings
//! and runs commands. A lost connection is retried with exponential backoff (1 s doubling,
//! ±20% jitter): at most 5 s apart during the first minute of an outage (a hub restart is back
//! within seconds), then up to 30 s by default; a welcomed connection starts the schedule
//! over. Runtimes keep running while disconnected; their events wait in the spool.
//!
//! Keepalive: the runner sends `ping` every `ping_interval` and reconnects when nothing has
//! arrived from the hub for `idle_timeout` (the hub must answer `ping` with `pong`).
//!
//! Shutdown (SIGINT / SIGTERM, or the future given to [`run_with`]): the runner stops its
//! runtimes gracefully (the same stop the hub would send) so each one's exit is spooled, keeps
//! the connection up to deliver those frames until they are acked (at most
//! `shutdown_grace`), then closes. Runtimes still running after the grace period are
//! abandoned with a synthetic `exited{code: null}`; frames not yet acked stay in the spool and
//! are replayed by the next runner of the box. Runtimes are not left running across a runner
//! restart: a new runner could not re-attach to them.
//!
//! A box that fails ([`LocalBox::fatal_error`]: its spool cannot record an event, an exit or a
//! command result) ends the runner with that error instead of running on; the runtimes are
//! stopped as in a shutdown.
//!
//! The token never crosses the connection (only proofs made with it do), and it is never
//! logged ([`twr_core::runner::Secret`] redacts itself). A hub that cannot prove it knows the
//! token (a wrong URL, a different token, or another process listening on the port, such as
//! another user's on a loopback port an SSH tunnel did not get) gets nothing but the hello,
//! and the runner tries again later.
//!
//! A daemon has no token: its connections are sealed channels the hub's key opened
//! ([`twr_core::sealed`]), so the hub is proven before the WebSocket starts, and the
//! handshake inside skips the proofs (sealed connections, [`twr_core::runner`]: the runner
//! does not check the challenge's proof and answers
//! [`twr_core::runner::SEALED_AUTH_PROOF`]). A daemon never backs off: it accepts.

use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use twr_core::runner::{
    auth_proof, check_auth_proof, hub_errors, new_nonce, valid_nonce, Hello, HubMsg, Inventory,
    Prover, RunnerMsg, Secret, MAX_FRAME_BYTES, PROTOCOL_VERSION, SEALED_AUTH_PROOF,
};

use crate::lbox::{LocalBox, LocalBoxOptions};
use crate::spool::DEFAULT_MAX_BYTES;

/// Runner version reported in the inventory: twr's whole version, with the build of a
/// development install (`0.2.0+dev.<commit>`, [`twr_core::version`]).
pub fn version() -> &'static str {
    twr_core::version()
}

/// A connection the hub refused (`error{unauthorized}` or `error{protocol}`), or whose hub
/// could not prove that it knows the box's token.
#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// Environment variable the CLI reads the token from (removed from the process environment
/// once read, see [`take_token_env`]).
pub const TOKEN_ENV: &str = "TWR_RUNNER_TOKEN";

/// Largest token file read.
const MAX_TOKEN_FILE: u64 = 64 << 10;

/// Takes the token out of the process environment ([`TOKEN_ENV`]): returns it (`None` when
/// unset or empty) and removes the variable either way, so no runtime, git or other child the
/// runner starts inherits it. Call it at startup, before anything is spawned.
pub fn take_token_env() -> Option<Secret> {
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    // Edition 2021: safe. Nothing reads the environment concurrently this early (the runner
    // has not started any task yet).
    std::env::remove_var(TOKEN_ENV);
    token.map(Secret::from)
}

/// `O_NONBLOCK` for opening the token file, and other files that a FIFO or a device at their
/// path must not hold up (`twr update`'s `file://` mirror). No `libc` on Linux, so the value
/// is spelled out per OS and architecture, as `crate::local::trash` does: `0o4000` on Linux's
/// generic ABI, x86 and ARM included, `0x0004` on macOS; elsewhere 0 (the regular-file check
/// on the opened descriptor stands alone).
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64",
        target_arch = "arm",
        target_arch = "aarch64"
    )
))]
pub const O_NONBLOCK: i32 = 0o4000;
#[cfg(target_os = "macos")]
pub const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "arm",
            target_arch = "aarch64"
        )
    )
)))]
pub const O_NONBLOCK: i32 = 0;

/// Reads the token from `path` (`twr runner --token-file`), trimmed of surrounding whitespace.
/// Refused when the file is not a regular file, is readable or writable by group or others
/// (it must be `chmod 600` or stricter), is empty, or is larger than 64 KiB. It is opened
/// without blocking (`O_NONBLOCK`: a FIFO or a device at the path cannot hold the runner up)
/// and checked on the opened descriptor, so what is read is what was checked.
pub fn read_token_file(path: &std::path::Path) -> anyhow::Result<Secret> {
    use std::io::Read;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening the token file {}", path.display()))?;
    let meta = f
        .metadata()
        .with_context(|| format!("reading the token file {}", path.display()))?;
    if !meta.is_file() {
        bail!("the token file {} is not a regular file", path.display());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "the token file {} is accessible by other users (mode {mode:03o}); run `chmod 600 {}`",
            path.display(),
            twr_core::paths::sh_word(&path.to_string_lossy())
        );
    }
    let mut text = String::new();
    f.take(MAX_TOKEN_FILE + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("reading the token file {}", path.display()))?;
    if text.len() as u64 > MAX_TOKEN_FILE {
        bail!("the token file {} is too large", path.display());
    }
    let token = text.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", path.display());
    }
    Ok(Secret::new(token))
}

/// Spooled frames sent per batch between checks for other work.
const BATCH: usize = 128;

/// What makes a runner an SSH box's daemon (`twr runner --daemon`): where it takes the hub's
/// connections and whose key opens them. Every path comes from the relay that started it
/// ([`crate::relay::DaemonPaths`]), as an argument: a daemon started by `systemd-run` has
/// another environment, so it derives no path from its own.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// The Unix socket it listens on (its directory private, [`crate::daemon`]).
    pub socket: PathBuf,
    /// The hub's public key ([`twr_core::sealed::HubKey::public`]): only a connection this
    /// key opened ([`twr_core::sealed::daemon_side`]) reaches the runner protocol.
    pub hub_key: [u8; 32],
    /// The box's machine lock ([`LocalBoxOptions::machine_lock`]): one daemon per hub key and
    /// box on the machine.
    pub lock: PathBuf,
    /// The build the daemon says it runs (its status line, the channel's hello, the
    /// inventory): its own, [`twr_core::build_id`]. Tests make a daemon "of another build".
    pub build: Option<String>,
}

/// Everything `twr runner` needs.
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// `ws://host:port[/path]` or `wss://...`. A daemon's ([`RunnerOptions::for_daemon`]) is
    /// `ssh://<hub id>`, which only names the hub in its logs.
    pub hub_url: String,
    /// The box's token (`[boxes.<name>] token` on the hub). Empty for a daemon.
    pub token: Secret,
    /// Box name.
    pub box_name: String,
    /// Data directory (spool `runner-<box>.db`, worktrees).
    pub data_dir: PathBuf,
    /// Worktrees directory (default `<data_dir>/worktrees`).
    pub worktrees_dir: Option<PathBuf>,
    /// Bound of the spool's outbox (default 256 MiB).
    pub spool_max_bytes: u64,
    /// First reconnect delay.
    pub backoff_min: Duration,
    /// Largest reconnect delay.
    pub backoff_max: Duration,
    /// The shortest wait before connecting again after the hub refused the runner, or could
    /// not prove that it knows the token (default a minute): retrying sooner can't help, since
    /// one side's twr or token has to change first.
    pub refused_retry: Duration,
    /// How often to ping the hub.
    pub ping_interval: Duration,
    /// Reconnect when the hub has been silent this long.
    pub idle_timeout: Duration,
    /// How long the handshake may take, from the hello to the welcome.
    pub welcome_timeout: Duration,
    /// How long shutdown waits for runtimes to exit and their frames to be acked.
    pub shutdown_grace: Duration,
    /// For `wss://`: trust only the CA certificates in this PEM file (a hub with a private or
    /// self-signed CA) instead of the public web roots.
    pub ca_file: Option<PathBuf>,
    /// Allow plain `ws://` to a host that is not loopback (`--allow-insecure`): the token and
    /// every event cross the network in clear text. Off: such a URL is refused
    /// ([`RunnerOptions::validate`]).
    pub allow_insecure: bool,
    /// Run as an SSH box's daemon: accept the hub's sealed connections on a Unix socket
    /// instead of dialing `hub_url` with the token.
    pub daemon: Option<DaemonOptions>,
}

impl RunnerOptions {
    /// Options with the defaults.
    pub fn new(
        hub_url: impl Into<String>,
        token: Secret,
        box_name: impl Into<String>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        RunnerOptions {
            hub_url: hub_url.into(),
            token,
            box_name: box_name.into(),
            data_dir: data_dir.into(),
            worktrees_dir: None,
            spool_max_bytes: DEFAULT_MAX_BYTES,
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            refused_retry: Duration::from_secs(60),
            ping_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
            welcome_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(10),
            ca_file: None,
            allow_insecure: false,
            daemon: None,
        }
    }

    /// The options of an SSH box's daemon (`twr runner --daemon`) with the defaults: no token,
    /// `hub_url` `ssh://<hub id>` ([`twr_core::sealed::hub_id`]), for its logs only.
    pub fn for_daemon(
        box_name: impl Into<String>,
        data_dir: impl Into<PathBuf>,
        daemon: DaemonOptions,
    ) -> Self {
        let url = format!("ssh://{}", twr_core::sealed::hub_id(&daemon.hub_key));
        let mut o = RunnerOptions::new(url, Secret::default(), box_name, data_dir);
        o.daemon = Some(daemon);
        o
    }

    /// The box options these runner options imply (built-in adapters, the box's machine
    /// lock: [`machine_lock_path`], or a daemon's [`DaemonOptions::lock`], which its relay
    /// resolved; the user's login shell asked for its `PATH` and the agents probed:
    /// [`crate::BoxEnv::user`]).
    pub fn box_options(&self) -> LocalBoxOptions {
        let mut o = LocalBoxOptions::new(&self.data_dir, &self.box_name);
        o.machine_lock = match &self.daemon {
            Some(d) => Some(d.lock.clone()),
            None => machine_lock_path(&self.hub_url, &self.box_name),
        };
        o.spool_max_bytes = self.spool_max_bytes;
        if let Some(w) = &self.worktrees_dir {
            o.worktrees = w.clone();
        }
        o.env = crate::BoxEnv::user();
        o
    }

    /// Checks the options: the box name, a token of at least
    /// [`twr_core::runner::TOKEN_MIN_LEN`] characters, and a `wss://` hub URL or a `ws://` one
    /// to a loopback host ([`is_loopback_host`]; any host with
    /// [`RunnerOptions::allow_insecure`]). A daemon has neither token nor URL: only its box
    /// name is checked.
    pub fn validate(&self) -> anyhow::Result<()> {
        validate_box_name(&self.box_name)?;
        if self.daemon.is_some() {
            return Ok(());
        }
        if self.token.is_empty() {
            bail!("the runner token is empty");
        }
        twr_core::runner::check_token_len(self.token.expose(), "the runner token")
            .map_err(anyhow::Error::msg)?;
        let lower = self.hub_url.to_ascii_lowercase();
        if lower.starts_with("ws://") {
            let host = url_host(&self.hub_url)?;
            if !self.allow_insecure && !is_loopback_host(&host) {
                bail!(
                    "refusing plain ws:// to {host}: the box token and every event would cross the \
                     network in clear text. Use wss:// (with --ca <pem> for a private CA), an SSH \
                     tunnel to a loopback port (--hub ws://127.0.0.1:<port>), or --allow-insecure \
                     on a network you trust"
                );
            }
        } else if !lower.starts_with("wss://") {
            bail!("the hub URL must start with ws:// or wss://");
        }
        Ok(())
    }
}

/// The lock file of box `box_name` of the hub at `hub_url` on this machine
/// ([`LocalBoxOptions::machine_lock`]): `<dir>/runner-locks/<box>-<hash>.lock`, where `<dir>`
/// is `$XDG_STATE_HOME/twr`, else `~/.local/state/twr` (neither of which a runner option
/// moves), and `<hash>` the first 16 hex digits of the SHA-256 of the hub URL (lowercase, no
/// trailing `/`) and the box name. `None` without `HOME` or `XDG_STATE_HOME`. Two spellings of
/// one hub's address (`localhost` and `127.0.0.1`) are two keys.
///
/// Not `$XDG_RUNTIME_DIR`: logind removes it when the user's last session ends, while a
/// runner started from it keeps running (holding a lock on a file no longer there), and a
/// runner started without a session (cron, `@reboot`) has none. Either way a second runner of
/// the box would lock another file.
pub fn machine_lock_path(hub_url: &str, box_name: &str) -> Option<PathBuf> {
    machine_lock_path_in(hub_url, box_name, |k| std::env::var_os(k))
}

/// [`machine_lock_path`] with the environment read through `env`.
pub(crate) fn machine_lock_path_in(
    hub_url: &str,
    box_name: &str,
    env: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    use sha2::Digest;
    let var = |k: &str| env(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let dir = var("XDG_STATE_HOME")
        .or_else(|| var("HOME").map(|h| h.join(".local/state")))?
        .join("twr");
    let url = hub_url.trim().trim_end_matches('/').to_ascii_lowercase();
    let mut h = sha2::Sha256::new();
    h.update(url.as_bytes());
    h.update([0]);
    h.update(box_name.as_bytes());
    let hash: String = h.finalize()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(
        dir.join("runner-locks")
            .join(format!("{box_name}-{hash}.lock")),
    )
}

/// The host of the hub URL `url`, as the WebSocket client connects to it (parsed the same way,
/// as an [`http::Uri`](tokio_tungstenite::tungstenite::http::Uri): user information and port
/// left out; an IPv6 address in its brackets).
fn url_host(url: &str) -> anyhow::Result<String> {
    let uri: tokio_tungstenite::tungstenite::http::Uri = url
        .parse()
        .map_err(|e| anyhow!("the hub URL {url:?} is not a URL: {e}"))?;
    match uri.host() {
        Some(h) if !h.is_empty() => Ok(h.to_string()),
        _ => bail!("the hub URL {url:?} has no host"),
    }
}

/// Whether `host` ([`url_host`]) is literally a loopback host: `localhost` (any case), an
/// IPv4 address in `127.0.0.0/8`, or the IPv6 `[::1]` (also IPv4-mapped loopback,
/// `[::ffff:127.0.0.1]`). Names are not resolved: one that resolves to loopback does not count.
pub fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6.parse::<std::net::Ipv6Addr>().is_ok_and(|ip| {
            ip.is_loopback() || ip.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        });
    }
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback())
}

/// Box names become file names (`runner-<box>.db`): [`twr_core::runner::validate_box_name`],
/// which the hub's config checks share.
pub use twr_core::runner::validate_box_name;

/// The TLS configuration for a hub whose certificate chains to the CA(s) in `ca_file` (and
/// nothing else).
pub fn tls_connector(ca_file: &std::path::Path) -> anyhow::Result<tokio_tungstenite::Connector> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::CertificateDer;
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca_file)
        .with_context(|| format!("reading the CA file {}", ca_file.display()))?
    {
        let cert = cert.with_context(|| format!("parsing the CA file {}", ca_file.display()))?;
        roots
            .add(cert)
            .with_context(|| format!("a CA certificate in {} is invalid", ca_file.display()))?;
    }
    if roots.is_empty() {
        bail!("no CA certificate in {}", ca_file.display());
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(tokio_tungstenite::Connector::Rustls(Arc::new(config)))
}

/// Opens the box the options describe.
pub fn open_box(opts: &RunnerOptions) -> anyhow::Result<LocalBox> {
    opts.validate()?;
    twr_core::paths::ensure_private_dir(&opts.data_dir)?;
    LocalBox::open(opts.box_options())
}

/// Runs the runner until SIGINT or SIGTERM.
pub async fn run(opts: RunnerOptions) -> anyhow::Result<()> {
    let lbox = open_box(&opts)?;
    run_with(opts, lbox, shutdown_signal()).await
}

/// Resolves on SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "cannot listen for SIGTERM");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

/// Runs the runner on `lbox` until `shutdown` resolves, then shuts down gracefully. A daemon
/// binds its socket first (an error if it cannot: [`crate::daemon`]), and also shuts down so
/// when a `stop` request comes on it.
pub async fn run_with(
    opts: RunnerOptions,
    lbox: LocalBox,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    opts.validate()?;
    // wss: rustls with ring (no system OpenSSL). Fails only when already installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let socket = match &opts.daemon {
        Some(d) => Some(crate::daemon::DaemonSocket::bind(&d.socket).await?),
        None => None,
    };
    // The login shell's PATH and the agents, in the background: never before the connection.
    let environment = tokio::spawn({
        let lbox = lbox.clone();
        async move { lbox.environment_loop().await }
    });
    let _environment = AbortOnDrop(environment);
    let (stop_tx, stop_rx) = watch::channel(false);
    // A daemon's `stop` request (it stores its wakeup: none is lost).
    let asked = Notify::new();
    let conn = connect_loop(&opts, &lbox, socket, stop_rx, &asked);
    tokio::pin!(conn);
    let requested = async {
        tokio::select! {
            _ = shutdown => {}
            () = asked.notified() => tracing::info!("asked to stop: shutting down"),
        }
    };
    let mut shutdown_at = None;
    tokio::select! {
        () = requested => {
            shutdown_at = Some(Instant::now());
            let _ = stop_tx.send(true);
            conn.as_mut().await;
        }
        () = conn.as_mut() => {}
    }
    // Whatever the connection did not see through (not connected, or the grace ran out).
    let left = shutdown_at.map_or(opts.shutdown_grace, |t| {
        opts.shutdown_grace.saturating_sub(t.elapsed())
    });
    lbox.shutdown(left.max(Duration::from_millis(100))).await;
    // A repository change still running (its connection is gone; the connection's reader has
    // ended, so none begins any more) runs on to its end first, received but not started yet
    // included, at most for as long as a git change may run before it is killed.
    if lbox.changes_running() > 0 || !lbox.git_changes_done(Duration::ZERO).await {
        tracing::info!("waiting for the repository changes in progress before stopping");
        let limit = twr_core::git::GIT_CHANGE_TIMEOUT;
        let done = tokio::time::timeout(limit, async {
            lbox.changes_done().await;
            lbox.git_changes_done(limit).await
        })
        .await
        .unwrap_or(false);
        if !done {
            tracing::warn!("stopping with a repository change still running; what it was changing is left part way");
        }
    }
    if let Some(e) = lbox.fatal_error() {
        bail!("the runner failed: {e}");
    }
    tracing::info!("runner stopped");
    Ok(())
}

/// Aborts its task when dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// While an outage is younger than this, reconnects are at most [`SHORT_OUTAGE_CAP`] apart.
const SHORT_OUTAGE: Duration = Duration::from_secs(60);

/// Longest wait between reconnects during a short outage (a hub restart, a network blip).
const SHORT_OUTAGE_CAP: Duration = Duration::from_secs(5);

/// The wait before reconnect attempt `attempt` (0-based) of an outage that has lasted
/// `outage`: `min` doubling per attempt, at most [`SHORT_OUTAGE_CAP`] while the outage is
/// shorter than [`SHORT_OUTAGE`] and `max` after that, with ±20% jitter from `unit` (in
/// `[0, 1)`), and never above that cap.
fn backoff_delay(
    attempt: u32,
    outage: Duration,
    min: Duration,
    max: Duration,
    unit: f64,
) -> Duration {
    let cap = if outage < SHORT_OUTAGE {
        max.min(SHORT_OUTAGE_CAP).max(min)
    } else {
        max
    };
    let base = min
        .checked_mul(1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX))
        .unwrap_or(cap)
        .min(cap);
    base.mul_f64(0.8 + 0.4 * unit.clamp(0.0, 1.0)).min(cap)
}

/// Reconnect backoff ([`backoff_delay`]) with jitter.
#[derive(Debug)]
struct Backoff {
    min: Duration,
    max: Duration,
    /// Attempts since the outage began.
    attempt: u32,
    /// When the outage began: the end of the last welcomed connection (or the start).
    since: Instant,
    seed: u64,
}

impl Backoff {
    fn new(min: Duration, max: Duration) -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            ^ u64::from(std::process::id()).rotate_left(32)
            | 1;
        Backoff {
            min,
            max,
            attempt: 0,
            since: Instant::now(),
            seed,
        }
    }

    /// A welcomed connection just ended: a new outage starts now.
    fn reset(&mut self) {
        self.attempt = 0;
        self.since = Instant::now();
    }

    /// xorshift64*
    fn rand_unit(&mut self) -> f64 {
        let mut x = self.seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed = x;
        (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next(&mut self) -> Duration {
        let unit = self.rand_unit();
        let d = backoff_delay(self.attempt, self.since.elapsed(), self.min, self.max, unit);
        self.attempt = self.attempt.saturating_add(1);
        d
    }
}

/// Resolves once `stop` is set (never, if its sender is gone without setting it).
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The WebSocket limits of a connection to the hub, either transport. What this side buffers
/// to send is bounded too: a peer that pings without reading (one that is not the hub, before
/// the handshake failed) cannot grow it.
pub(crate) fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
        .max_write_buffer_size(MAX_FRAME_BYTES + (1 << 20))
}

/// Serves the hub until `stop` is set or the box fails: a daemon's connections come from its
/// `socket` ([`crate::daemon::accept_loop`], which tells `asked` of a `stop` request), a
/// runner's from dialing the hub ([`dial_loop`]).
async fn connect_loop(
    opts: &RunnerOptions,
    lbox: &LocalBox,
    socket: Option<crate::daemon::DaemonSocket>,
    stop: watch::Receiver<bool>,
    asked: &Notify,
) {
    match (socket, &opts.daemon) {
        (Some(socket), Some(daemon)) => {
            crate::daemon::accept_loop(opts, daemon, socket, lbox, stop, asked).await
        }
        _ => dial_loop(opts, lbox, stop).await,
    }
}

/// The runner's connection loop: dials the hub, serves it, and dials again with backoff (see
/// the module docs).
async fn dial_loop(opts: &RunnerOptions, lbox: &LocalBox, mut stop: watch::Receiver<bool>) {
    let mut backoff = Backoff::new(opts.backoff_min, opts.backoff_max);
    let connector = match opts.ca_file.as_deref().map(tls_connector).transpose() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %format!("{e:#}"), "cannot set up TLS");
            return;
        }
    };
    let epoch_hint = std::sync::Mutex::new(None);
    let mut fatal = lbox.fatal_watch();
    loop {
        if *stop.borrow() || lbox.fatal_error().is_some() {
            return;
        }
        // The hub takes a new connection as proof that no repository change it sent before
        // still runs here (it may have left one unanswered): those finish first.
        if lbox.changes_running() > 0 {
            tracing::info!(
                changes = lbox.changes_running(),
                "waiting for the repository changes in progress before connecting"
            );
            tokio::select! {
                () = lbox.changes_done() => {}
                () = stopped(&mut stop) => return,
            }
        }
        let attempt = tokio::select! {
            r = tokio::time::timeout(
                opts.welcome_timeout,
                tokio_tungstenite::connect_async_tls_with_config(
                    opts.hub_url.as_str(),
                    Some(ws_config()),
                    true,
                    connector.clone(),
                ),
            ) => r,
            () = stopped(&mut stop) => return,
        };
        let mut refused = false;
        match attempt {
            Ok(Ok((ws, _resp))) => {
                let out = serve(
                    ws,
                    lbox,
                    opts,
                    &epoch_hint,
                    stop.clone(),
                    std::future::pending(),
                )
                .await;
                if out.welcomed {
                    backoff.reset();
                }
                if lbox.fatal_error().is_some() {
                    return;
                }
                match out.result {
                    Ok(true) => return,
                    Ok(false) => tracing::warn!("hub connection closed"),
                    Err(e) => {
                        refused = e.downcast_ref::<Refused>().is_some();
                        tracing::warn!(error = %format!("{e:#}"), "hub connection lost")
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(hub = %opts.hub_url, error = %e, "cannot connect to the hub")
            }
            Err(_) => tracing::warn!(hub = %opts.hub_url, "connecting to the hub timed out"),
        }
        let delay = if refused {
            backoff.next().max(opts.refused_retry)
        } else {
            backoff.next()
        };
        tracing::info!(retry_in_ms = delay.as_millis() as u64, "reconnecting");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = stopped(&mut stop) => return,
            _ = fatal.wait_for(Option::is_some) => return,
        }
    }
}

/// What a connection's WebSocket runs over: TCP (and TLS) when the runner dials, a sealed
/// stream over the daemon's socket ([`twr_core::sealed::SealedStream`]).
pub(crate) trait HubStream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}

impl<S: AsyncRead + AsyncWrite + Unpin + Send + 'static> HubStream for S {}

/// The outcome of one connection ([`serve`]).
pub(crate) struct Served {
    /// The hub welcomed it.
    pub(crate) welcomed: bool,
    /// `Ok(true)`: shut down; `Ok(false)`: the hub closed the connection.
    pub(crate) result: anyhow::Result<bool>,
}

/// A connection's reader task ([`read_loop`]). [`serve`] joins it before it returns
/// ([`Reader::join`]): aborting a task only asks it to stop at its next await, and a reader in
/// the middle of taking a command (on another worker thread) would otherwise count or start
/// it after the connection is over, where the next connection's wait for repository changes
/// in progress cannot see it.
struct Reader {
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    /// Its outcome was taken ([`Reader::outcome`]): the task is over.
    done: bool,
}

impl Reader {
    /// Waits for the reader to end by itself: the connection's outcome. Cancel safe.
    async fn outcome(&mut self) -> anyhow::Result<bool> {
        let r = (&mut self.task).await;
        self.done = true;
        reader_result(r)
    }

    /// Stops the reader and waits until it no longer runs.
    async fn join(mut self) {
        if !self.done {
            self.task.abort();
            let _ = (&mut self.task).await;
            self.done = true;
        }
    }
}

impl Drop for Reader {
    /// A connection dropped part way (the runner going away) at least asks its reader to stop.
    fn drop(&mut self) {
        if !self.done {
            self.task.abort();
        }
    }
}

fn text(msg: &RunnerMsg) -> anyhow::Result<Message> {
    Ok(Message::Text(serde_json::to_string(msg)?.into()))
}

/// Serves one connection to the hub: the handshake, the replay of the spool, then live
/// frames and commands, until it ends, `stop` has drained it, the box failed, or `replaced`
/// completes (a daemon's newer connection replaces this one: an error). Whichever way it
/// ends, its reader task has ended when this returns ([`Reader::join`]), so no command of
/// this connection begins afterwards. `epoch_hint` is the epoch of the last welcome, which
/// the inventory reports (shared, not borrowed mutably: a daemon ends a session it replaces
/// part way).
pub(crate) async fn serve<S: HubStream>(
    ws: WebSocketStream<S>,
    lbox: &LocalBox,
    opts: &RunnerOptions,
    epoch_hint: &std::sync::Mutex<Option<u64>>,
    stop: watch::Receiver<bool>,
    replaced: impl std::future::Future<Output = ()> + Send,
) -> Served {
    let mut welcomed = false;
    let mut reader = None;
    let result = tokio::select! {
        r = serve_inner(ws, lbox, opts, epoch_hint, stop, &mut welcomed, &mut reader) => r,
        () = replaced => Err(anyhow!("a newer connection from the hub replaced this one")),
    };
    if let Some(r) = reader {
        r.join().await;
    }
    Served { welcomed, result }
}

async fn serve_inner<S: HubStream>(
    ws: WebSocketStream<S>,
    lbox: &LocalBox,
    opts: &RunnerOptions,
    epoch_hint: &std::sync::Mutex<Option<u64>>,
    mut stop: watch::Receiver<bool>,
    welcomed: &mut bool,
    reader_slot: &mut Option<Reader>,
) -> anyhow::Result<bool> {
    let (mut sink, mut stream) = ws.split();
    let spool = lbox.spool().clone();
    let io_timeout = opts.idle_timeout;

    let hint = *epoch_hint.lock().unwrap_or_else(|e| e.into_inner());
    let (epoch, watermarks) = tokio::time::timeout(
        opts.welcome_timeout,
        handshake(&mut sink, &mut stream, lbox, opts, hint),
    )
    .await
    .map_err(|_| anyhow!("no welcome from the hub within {:?}", opts.welcome_timeout))??;
    spool.apply_watermarks(&watermarks)?;
    *welcomed = true;
    *epoch_hint.lock().unwrap_or_else(|e| e.into_inner()) = Some(epoch);
    tracing::info!(
        epoch,
        box_name = %opts.box_name,
        backlog = spool.len().unwrap_or(0),
        "connected to the hub"
    );

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<RunnerMsg>();
    let last_rx = Arc::new(Mutex::new(Instant::now()));
    let reader = reader_slot.insert(Reader {
        task: tokio::spawn(read_loop(
            stream,
            lbox.clone(),
            out_tx.clone(),
            last_rx.clone(),
        )),
        done: false,
    });

    let mut cursor = 0i64;
    let mut synced = false;
    let mut draining: Option<tokio::time::Instant> = None;
    let mut tick = tokio::time::interval(opts.ping_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.reset();

    macro_rules! send {
        ($m:expr) => {
            tokio::time::timeout(io_timeout, sink.send($m))
                .await
                .map_err(|_| anyhow!("sending to the hub timed out"))?
                .context("sending to the hub")?
        };
    }

    loop {
        if let Some(e) = lbox.fatal_error() {
            bail!("{e}");
        }
        if draining.is_none() && *stop.borrow() {
            lbox.stop_all();
            draining = Some(tokio::time::Instant::now() + opts.shutdown_grace);
        }
        let notified = lbox.outbox_notify().notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let rows = spool.rows_after(cursor, BATCH)?;
        if !rows.is_empty() {
            for r in rows {
                cursor = r.id;
                send!(Message::Text(r.frame.into()));
            }
            while let Ok(m) = out_rx.try_recv() {
                send!(text(&m)?);
            }
            if reader.task.is_finished() {
                return reader.outcome().await;
            }
            continue;
        }
        if !synced {
            send!(text(&RunnerMsg::Synced)?);
            synced = true;
        }
        if let Some(deadline) = draining {
            let done = lbox.live_count() == 0 && spool.is_empty()?;
            if done || tokio::time::Instant::now() >= deadline {
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), sink.send(Message::Close(None)))
                        .await;
                return Ok(true);
            }
        }
        let deadline = draining.unwrap_or_else(|| tokio::time::Instant::now() + opts.ping_interval);
        tokio::select! {
            biased;
            m = out_rx.recv() => {
                if let Some(m) = m {
                    send!(text(&m)?);
                }
            }
            r = stop.changed(), if draining.is_none() => {
                if r.is_err() {
                    // The runner is going away without a shutdown request: treat as one.
                    lbox.stop_all();
                    draining = Some(tokio::time::Instant::now() + opts.shutdown_grace);
                }
            }
            r = reader.outcome() => return r,
            _ = tick.tick() => {
                let silent = last_rx.lock().unwrap_or_else(|e| e.into_inner()).elapsed();
                if silent > opts.idle_timeout {
                    bail!("the hub has been silent for {silent:?}");
                }
                send!(text(&RunnerMsg::Ping)?);
            }
            () = tokio::time::sleep_until(deadline), if draining.is_some() => {}
            () = &mut notified => {}
        }
    }
}

/// The next text message from the hub before the welcome (other frames skipped).
async fn next_hub_msg<S: HubStream>(
    stream: &mut futures::stream::SplitStream<WebSocketStream<S>>,
) -> anyhow::Result<HubMsg> {
    loop {
        let msg = match stream.next().await {
            Some(m) => m.context("reading from the hub")?,
            None => bail!("the hub closed the connection before the welcome"),
        };
        let t = match msg {
            Message::Text(t) => t,
            Message::Close(f) => bail!("the hub closed the connection before the welcome ({f:?})"),
            _ => continue,
        };
        return serde_json::from_str::<HubMsg>(t.as_str())
            .map_err(|e| anyhow!("a malformed message from the hub before the welcome: {e}"));
    }
}

/// The error for a refusal `code` / `message` from the hub during the handshake (logged too
/// when it is about the box or token, the one thing the user has to fix here).
fn refused(opts: &RunnerOptions, code: &str, message: &str) -> anyhow::Error {
    if code == hub_errors::UNAUTHORIZED && opts.daemon.is_some() {
        tracing::error!(
            box_name = %opts.box_name,
            "the hub rejected this box daemon's box name: is the box an SSH box of that hub \
             (`transport = \"ssh\"`)?"
        );
    } else if code == hub_errors::UNAUTHORIZED {
        tracing::error!(
            box_name = %opts.box_name,
            "the hub rejected this runner's box name or token (a hub older than this runner \
             answers so too: both must run the same twr version)"
        );
    } else if code == hub_errors::PROTOCOL {
        tracing::error!(
            box_name = %opts.box_name,
            version = version(),
            "the hub refused this runner's protocol ({message}): run the same twr version on \
             the hub and here, then restart this runner"
        );
    }
    anyhow::Error::new(Refused(format!(
        "the hub refused the runner: {code}: {message}"
    )))
}

/// The handshake (see the module docs) up to the welcome: `(epoch, watermarks)`. Nothing but
/// the hello goes out before the hub has proved that it knows the box's token. On a daemon's
/// sealed connection the channel already proved the hub: the challenge's proof is not
/// checked, and the runner's is [`SEALED_AUTH_PROOF`].
async fn handshake<S: HubStream>(
    sink: &mut futures::stream::SplitSink<WebSocketStream<S>, Message>,
    stream: &mut futures::stream::SplitStream<WebSocketStream<S>>,
    lbox: &LocalBox,
    opts: &RunnerOptions,
    epoch_hint: Option<u64>,
) -> anyhow::Result<(u64, std::collections::BTreeMap<String, u64>)> {
    let nonce = new_nonce()?;
    let hello = RunnerMsg::Hello(Hello {
        box_name: opts.box_name.clone(),
        protocol: PROTOCOL_VERSION,
        nonce: nonce.clone(),
    });
    sink.send(text(&hello)?).await.context("sending hello")?;
    let (hub_nonce, proof) = match next_hub_msg(stream).await? {
        HubMsg::Challenge { nonce, proof } => (nonce, proof),
        HubMsg::Error { code, message } => return Err(refused(opts, &code, &message)),
        other => bail!(
            "the hub sent {} instead of a challenge: is it a twr hub of this version?",
            hub_msg_type(&other)
        ),
    };
    let sealed = opts.daemon.is_some();
    let proven = sealed
        || (valid_nonce(&hub_nonce)
            && hub_nonce != nonce
            && check_auth_proof(
                &opts.token,
                Prover::Hub,
                &opts.box_name,
                &nonce,
                &hub_nonce,
                &proof,
            ));
    if !proven {
        tracing::error!(
            box_name = %opts.box_name,
            hub = %opts.hub_url,
            "the hub could not prove that it knows this box's token: the hub URL is wrong, the \
             token differs from the hub's [boxes.{}] token, the box is not configured there, or \
             something else listens on that address (an SSH tunnel that could not bind its port \
             leaves it to anyone: use -o ExitOnForwardFailure=yes). Nothing was sent to it",
            opts.box_name
        );
        return Err(anyhow::Error::new(Refused(
            "the hub did not prove that it knows the box's token".into(),
        )));
    }
    let mine = if sealed {
        SEALED_AUTH_PROOF.to_string()
    } else {
        auth_proof(
            &opts.token,
            Prover::Runner,
            &opts.box_name,
            &nonce,
            &hub_nonce,
        )
    };
    sink.send(text(&RunnerMsg::Auth { proof: mine })?)
        .await
        .context("sending the proof")?;
    match next_hub_msg(stream).await? {
        HubMsg::Authenticated => {}
        HubMsg::Error { code, message } => return Err(refused(opts, &code, &message)),
        other => bail!(
            "the hub sent {} instead of authenticated",
            hub_msg_type(&other)
        ),
    }
    let inventory = RunnerMsg::Inventory(Inventory {
        version: version().to_string(),
        epoch_hint,
        running: lbox.running(),
        in_flight: lbox.in_flight(),
        acks_needed: lbox.spool().pending()?,
        runtimes: lbox.runtimes().to_vec(),
        worktrees: lbox.worktrees().to_path_buf(),
        build: match &opts.daemon {
            Some(d) => d.build.clone(),
            None => twr_core::build_id().map(str::to_string),
        },
        // What is known: none before the first probe ended (`Op::Agents` asks later).
        agents: lbox.agents(),
    });
    sink.send(text(&inventory)?)
        .await
        .context("sending the inventory")?;
    loop {
        match next_hub_msg(stream).await? {
            HubMsg::Welcome { epoch, watermarks } => return Ok((epoch, watermarks)),
            HubMsg::Error { code, message } => return Err(refused(opts, &code, &message)),
            HubMsg::Ping => sink.send(text(&RunnerMsg::Pong)?).await?,
            other => tracing::debug!(?other, "ignoring a message before the welcome"),
        }
    }
}

/// The `type` of a hub message, for errors (never its content).
fn hub_msg_type(m: &HubMsg) -> String {
    serde_json::to_value(m)
        .ok()
        .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_string))
        .unwrap_or_else(|| "a message".to_string())
}

fn reader_result(r: Result<anyhow::Result<()>, tokio::task::JoinError>) -> anyhow::Result<bool> {
    match r {
        Ok(Ok(())) => Ok(false),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow!("reader task failed: {e}")),
    }
}

/// Reads hub messages: applies acks, answers pings and runs commands (session commands in
/// arrival order, the others concurrently).
async fn read_loop<S: HubStream>(
    mut stream: futures::stream::SplitStream<WebSocketStream<S>>,
    lbox: LocalBox,
    out: mpsc::UnboundedSender<RunnerMsg>,
    last_rx: Arc<Mutex<Instant>>,
) -> anyhow::Result<()> {
    let spool = lbox.spool().clone();
    while let Some(msg) = stream.next().await {
        let msg = msg.context("reading from the hub")?;
        *last_rx.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        let t = match msg {
            Message::Text(t) => t,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let msg = match serde_json::from_str::<HubMsg>(t.as_str()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "ignoring a malformed hub message");
                continue;
            }
        };
        match msg {
            HubMsg::Ack { thread, rseq } => {
                spool.ack(&thread, rseq)?;
                // Lets a draining connection notice that everything is acked.
                lbox.outbox_notify().notify_waiters();
            }
            HubMsg::Cmd { cmd_id, op } => {
                if op.is_session_op() {
                    let r = lbox.handle_cmd(&cmd_id, op).await;
                    let _ = out.send(RunnerMsg::CmdResult(r));
                } else {
                    // Counted from its arrival: the next hello waits for it (the next
                    // connection's, once this reader has ended: `Reader::join`).
                    if op.changes_repository() {
                        lbox.change_received();
                    }
                    let change = op.changes_repository().then(|| lbox.begin_change());
                    let lbox = lbox.clone();
                    let out = out.clone();
                    tokio::spawn(async move {
                        let r = lbox.handle_cmd(&cmd_id, op).await;
                        drop(change);
                        let _ = out.send(RunnerMsg::CmdResult(r));
                    });
                }
            }
            HubMsg::Ping => {
                let _ = out.send(RunnerMsg::Pong);
            }
            HubMsg::Pong => {}
            HubMsg::Welcome { .. } | HubMsg::Challenge { .. } | HubMsg::Authenticated => {
                tracing::warn!("ignoring a handshake message after the welcome")
            }
            HubMsg::Error { code, message } => {
                tracing::warn!(code = %code, message = %message, "the hub reported an error");
                if code == hub_errors::FENCED {
                    return Err(anyhow!("a newer connection of this box replaced this one"));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The machine lock is in the state directory, whatever the session: a runner started from
    /// a login shell (`XDG_RUNTIME_DIR` set) and one started from cron (none) of the same box
    /// and hub lock the same file.
    #[test]
    fn the_machine_lock_does_not_depend_on_the_session() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        let hub = "ws://127.0.0.1:7878/";
        let login = machine_lock_path_in(
            hub,
            "gpu",
            env(&[("HOME", "/home/u"), ("XDG_RUNTIME_DIR", "/run/user/1000")]),
        )
        .unwrap();
        let cron = machine_lock_path_in(hub, "gpu", env(&[("HOME", "/home/u")])).unwrap();
        assert_eq!(login, cron);
        assert!(
            login.starts_with("/home/u/.local/state/twr/runner-locks"),
            "{}",
            login.display()
        );
        let state = machine_lock_path_in(
            hub,
            "gpu",
            env(&[("HOME", "/home/u"), ("XDG_STATE_HOME", "/srv/state")]),
        )
        .unwrap();
        assert!(
            state.starts_with("/srv/state/twr/runner-locks"),
            "{}",
            state.display()
        );
        assert_eq!(state.file_name(), login.file_name());
        assert_eq!(
            machine_lock_path_in(hub, "gpu", env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])),
            None
        );
    }

    /// The reconnect schedule: doubling from 1 s with ±20% jitter, capped at 5 s during the
    /// first minute of an outage (a 7 s hub restart is retried within ~5 s, not 8.8 s), then
    /// growing to the 30 s maximum.
    #[test]
    fn backoff_stays_short_for_short_outages() {
        let (min, max) = (Duration::from_secs(1), Duration::from_secs(30));
        let secs = Duration::from_secs;
        let short = |attempt, unit| backoff_delay(attempt, secs(7), min, max, unit);
        let long = |attempt, unit| backoff_delay(attempt, secs(61), min, max, unit);
        // Middle of the jitter range: the base schedule.
        let base: Vec<Duration> = (0..7).map(|a| short(a, 0.5)).collect();
        assert_eq!(base, [1, 2, 4, 5, 5, 5, 5].map(secs));
        let base: Vec<Duration> = (0..7).map(|a| long(a, 0.5)).collect();
        assert_eq!(base, [1, 2, 4, 8, 16, 30, 30].map(secs));
        // Jitter spreads each wait by ±20%, never past the cap.
        for a in 0..40 {
            for unit in [0.0, 0.3, 0.999] {
                let d = short(a, unit);
                assert!(d <= secs(5), "{a} {unit}: {d:?}");
                let d = long(a, unit);
                assert!(d <= max, "{a} {unit}: {d:?}");
            }
        }
        assert_eq!(short(0, 0.0), Duration::from_millis(800));
        assert_eq!(short(1, 0.999).as_millis(), 2399);
        assert_eq!(short(3, 0.0), secs(4));
        // A cap below the minimum never undercuts it; a small maximum still caps.
        assert_eq!(backoff_delay(9, secs(1), secs(10), secs(60), 0.5), secs(10));
        assert_eq!(backoff_delay(9, secs(1), min, secs(3), 0.5), secs(3));

        // The stateful wrapper: attempts count up and a welcomed connection starts over.
        let mut b = Backoff::new(min, max);
        let d: Vec<Duration> = (0..8).map(|_| b.next()).collect();
        assert!(d.iter().all(|d| *d <= secs(5)), "{d:?}");
        assert!(d[4] >= secs(4), "{d:?}");
        b.reset();
        assert!(b.next() <= Duration::from_millis(1200));
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn options_are_checked() {
        let ok = RunnerOptions::new("wss://hub:7878", Secret::new(TOKEN), "gpu-1", "/tmp/x");
        ok.validate().unwrap();
        for bad in ["", ".x", "a/b", "a b", &"x".repeat(65)] {
            assert!(validate_box_name(bad).is_err(), "{bad:?}");
        }
        let mut o = ok.clone();
        o.hub_url = "http://hub".into();
        assert!(o.validate().is_err());
        let mut o = ok.clone();
        o.token = Secret::default();
        assert!(o.validate().is_err());
        assert!(!format!("{ok:?}").contains(TOKEN));
        // A token shorter than 16 characters is refused; 16 to 31 pass (the CLI warns).
        let mut o = ok.clone();
        o.token = Secret::new("s3cret-123456789");
        o.validate().unwrap();
        o.token = Secret::new("s3cret-12345678");
        let e = o.validate().unwrap_err().to_string();
        assert!(
            e.contains("the runner token is too short (15 characters"),
            "{e}"
        );
        assert!(!e.contains("s3cret"), "{e}");
    }

    #[test]
    fn plain_ws_is_for_loopback_only() {
        let opts = |url: &str| RunnerOptions::new(url, Secret::new(TOKEN), "gpu", "/tmp/x");
        for url in [
            "ws://127.0.0.1:7878",
            "ws://127.9.8.7:7878/path?q=1",
            "ws://localhost:7878",
            "WS://LocalHost:7878/",
            "ws://[::1]:7878",
            "ws://[::ffff:127.0.0.1]:7878",
            "ws://me@127.0.0.1:7878",
            "ws://me:pw@localhost/x",
            "ws://127.0.0.1",
            "wss://hub.example.com:7878",
            "wss://10.0.0.5",
        ] {
            opts(url)
                .validate()
                .unwrap_or_else(|e| panic!("{url}: {e:#}"));
        }
        for (url, host) in [
            ("ws://hub.example.com:7878", "hub.example.com"),
            ("ws://10.0.0.5:7878", "10.0.0.5"),
            ("ws://0.0.0.0:7878", "0.0.0.0"),
            ("ws://[::]:7878", "[::]"),
            ("ws://[2001:db8::1]:7878/p", "[2001:db8::1]"),
            // Names that resolve to loopback are not resolved.
            ("ws://localhost.:7878", "localhost."),
            ("ws://localhost.example.com:7878", "localhost.example.com"),
            ("ws://127.0.0.1.nip.io:7878", "127.0.0.1.nip.io"),
            ("ws://2130706433:7878", "2130706433"),
            // User information is not the host.
            ("ws://127.0.0.1@evil.example:7878", "evil.example"),
            ("ws://localhost:pw@evil.example/", "evil.example"),
        ] {
            let e = opts(url).validate().unwrap_err().to_string();
            assert!(
                e.starts_with(&format!("refusing plain ws:// to {host}: ")),
                "{url}: {e}"
            );
            assert!(
                e.contains("wss://") && e.contains("--ca") && e.contains("SSH tunnel"),
                "{e}"
            );
            assert!(e.contains("--allow-insecure"), "{e}");
            let mut o = opts(url);
            o.allow_insecure = true;
            o.validate().unwrap_or_else(|e| panic!("{url}: {e:#}"));
        }
        for url in ["ws://", "ws://:7878", "ws://[::1", "ws://a b"] {
            let mut o = opts(url);
            o.allow_insecure = true;
            assert!(o.validate().is_err(), "{url}");
        }
    }

    #[test]
    fn loopback_hosts_are_literal() {
        for h in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "127.255.255.254",
            "[::1]",
        ] {
            assert!(is_loopback_host(h), "{h}");
        }
        for h in [
            "",
            "::1",
            "[::1",
            "128.0.0.1",
            "126.255.255.255",
            "localhost.",
            "local",
            "[fe80::1]",
            "[::1%25lo]",
            "127.1",
        ] {
            assert!(!is_loopback_host(h), "{h}");
        }
    }

    #[test]
    fn token_files_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("token");
        std::fs::write(&p, "  s3cret\n").unwrap();
        for mode in [0o644, 0o640, 0o604, 0o620] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = read_token_file(&p).unwrap_err();
            assert!(e.to_string().contains("chmod 600"), "{mode:o}: {e:#}");
            // The token never shows up in the error.
            assert!(!format!("{e:#}").contains("s3cret"));
        }
        for mode in [0o600, 0o400] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(read_token_file(&p).unwrap().expose(), "s3cret");
        }
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(&p, "\n \n").unwrap();
        assert!(read_token_file(&p)
            .unwrap_err()
            .to_string()
            .contains("empty"));
        std::fs::write(&p, "x".repeat(70 << 10)).unwrap();
        assert!(read_token_file(&p)
            .unwrap_err()
            .to_string()
            .contains("too large"));
        assert!(read_token_file(dir.path()).is_err());
        assert!(read_token_file(&dir.path().join("missing")).is_err());
        // A FIFO (no writer: a blocking open would wait for one forever) is refused at once.
        let fifo = dir.path().join("fifo");
        let ran = std::process::Command::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(ran.success());
        let (tx, rx) = std::sync::mpsc::channel();
        let f = fifo.clone();
        std::thread::spawn(move || {
            let _ = tx.send(read_token_file(&f).map(|_| ()).map_err(|e| e.to_string()));
        });
        let r = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("reading a FIFO as the token file hung");
        assert!(
            r.as_ref().unwrap_err().contains("is not a regular file"),
            "{r:?}"
        );
        // The suggested command works pasted as shown, whatever the path holds.
        let odd = dir.path().join("my box's $HOME");
        std::fs::create_dir(&odd).unwrap();
        let p = odd.join("token");
        std::fs::write(&p, "s3cret").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let e = read_token_file(&p).unwrap_err().to_string();
        let cmd = e.split('`').nth(1).unwrap();
        let ran = std::process::Command::new("sh")
            .args(["-c", cmd])
            .status()
            .unwrap();
        assert!(ran.success(), "{cmd}");
        assert_eq!(read_token_file(&p).unwrap().expose(), "s3cret");
    }
}

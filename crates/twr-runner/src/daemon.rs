//! An SSH box's daemon (`twr runner --daemon`, [`crate::RunnerOptions::for_daemon`]): its Unix
//! socket, the one-line requests made on it, and the loop that takes the hub's connections
//! from it.
//!
//! The hub reaches the daemon through `ssh` and a relay on the box (`twr runner --stdio`,
//! [`crate::relay`]), which connects to this socket and copies bytes. The daemon owns the box
//! ([`crate::LocalBox`]: runtimes, spool, worktrees) and outlives every connection, so agents
//! keep running while the hub is away; it ends on a `stop` request, SIGTERM or SIGINT, or when
//! the box fails, never because a connection ended.
//!
//! # The socket
//!
//! `DaemonSocket::bind`: in a directory made 0700 (an existing one other users can access,
//! or that is not this user's, is refused), the socket itself 0600. A socket file left by a
//! daemon that died is removed only when connecting to it fails: the machine lock, taken when
//! the box opened, already keeps a second daemon of this hub key and box away. The socket
//! goes when the daemon stops serving (before it drains), so a relay waiting for it to stop
//! sees it at once.
//!
//! # Requests
//!
//! A connection's peer must run as this daemon's user (its credentials, `SO_PEERCRED`), and
//! starts with one line of at most [`MAX_REQUEST_LINE`] bytes within [`REQUEST_TIMEOUT`]:
//! `twr-box 1 connect`, `twr-box 1 status` or `twr-box 1 stop` ([`Request`]). Anything else is
//! dropped without an answer. Each request is answered with one line ([`Status`]):
//! `twr-box 1 <build id or -> <runner protocol> <hub id>`. Then:
//!
//! - `status`: the daemon closes the connection.
//! - `stop`: it closes, then shuts down as on SIGTERM ([`crate::run_with`]: runtimes stopped,
//!   their exits spooled and delivered to a connected hub within the grace period).
//! - `connect`: the connection becomes the hub's. The daemon opens the sealed channel on it
//!   ([`twr_core::sealed::daemon_side`] with the hub key it was started with, within
//!   [`SEALED_TIMEOUT`]): a peer that cannot sign with that key gets the channel's hello line
//!   and nothing else. A WebSocket client handshake runs inside the channel, then the runner
//!   session ([`crate::client`]), whose handshake skips the token proofs (the channel proved
//!   the hub).
//!
//! # Sessions
//!
//! One at a time. The daemon keeps accepting while it serves one: a newer `connect` (the hub
//! reconnected after a network drop the daemon has not noticed yet) replaces it, the old one
//! ending as a lost connection does (its unacked frames replayed on the new one). The new
//! session first ends the old one and waits until its reader task no longer runs
//! ([`Current::retire`]): only then does it wait for the repository changes in progress and
//! answer the hub, so no command of the old connection can begin after the new connection's
//! inventory (which the hub takes as proof that none still runs). Opening runs beside the
//! session being served, so a slow or hostile peer holds up nothing; at most
//! [`MAX_ADMITTING`] connections open at once. A daemon never backs off: it waits for the hub
//! to come.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch, Notify};
use tokio_tungstenite::WebSocketStream;
use twr_core::runner::PROTOCOL_VERSION;
use twr_core::sealed::SealedStream;

use crate::client::{serve, stopped, ws_config, DaemonOptions, RunnerOptions, Served};
use crate::LocalBox;

/// What every request and status line starts with: the name and version of these lines.
pub const LINE_PREFIX: &str = "twr-box 1 ";

/// Longest request line, its `\n` included.
pub const MAX_REQUEST_LINE: usize = 256;

/// Longest status line read ([`request`]), its `\n` included.
pub const MAX_STATUS_LINE: usize = 256;

/// How long the daemon waits for a connection's request line, and a client for the daemon's
/// status line.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the sealed channel, and then the WebSocket handshake inside it, may each take to
/// open on a `connect`.
pub const SEALED_TIMEOUT: Duration = Duration::from_secs(15);

/// Most connections opening (between accept and their session) at once; more are dropped.
pub const MAX_ADMITTING: usize = 16;

/// A request on the daemon's socket (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// Become the hub's connection.
    Connect,
    /// Only the status line.
    Status,
    /// Shut the daemon down.
    Stop,
}

impl Request {
    /// The request's line, `\n` included.
    pub fn line(self) -> String {
        let word = match self {
            Request::Connect => "connect",
            Request::Status => "status",
            Request::Stop => "stop",
        };
        format!("{LINE_PREFIX}{word}\n")
    }

    /// The request a line (without its `\n`) makes, if it is one exactly.
    pub fn parse(line: &str) -> Option<Request> {
        match line.strip_prefix(LINE_PREFIX)? {
            "connect" => Some(Request::Connect),
            "status" => Some(Request::Status),
            "stop" => Some(Request::Stop),
            _ => None,
        }
    }
}

/// The daemon's answer to every request: who answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The daemon's build id ([`twr_core::build_id`]), when it could read its executable
    /// (`-` on the line otherwise).
    pub build: Option<String>,
    /// The runner protocol it speaks ([`PROTOCOL_VERSION`]).
    pub protocol: u32,
    /// The id of the hub key it was started with ([`twr_core::sealed::hub_id`]).
    pub hub_id: String,
}

impl Status {
    /// This daemon's status for hub key `hub_key`.
    pub fn of(hub_key: &[u8; 32]) -> Status {
        Status {
            build: twr_core::build_id().map(str::to_string),
            protocol: PROTOCOL_VERSION,
            hub_id: twr_core::sealed::hub_id(hub_key),
        }
    }

    /// The build as shown to people: the id, or `-` when unknown.
    pub fn build_label(&self) -> &str {
        self.build.as_deref().unwrap_or("-")
    }

    /// The status line, `\n` included.
    pub fn line(&self) -> String {
        format!(
            "{LINE_PREFIX}{} {} {}\n",
            self.build_label(),
            self.protocol,
            self.hub_id
        )
    }

    /// The status a line (without its `\n`) gives, if it is one: a build id or `-`, a
    /// protocol number and a hub id (16 lowercase hex digits).
    pub fn parse(line: &str) -> Option<Status> {
        let mut words = line.strip_prefix(LINE_PREFIX)?.split(' ');
        let (build, protocol, hub_id) = (words.next()?, words.next()?, words.next()?);
        if words.next().is_some() {
            return None;
        }
        let build = match build {
            "-" => None,
            b if twr_core::valid_build_id(b) => Some(b.to_string()),
            _ => return None,
        };
        let protocol = protocol.parse().ok()?;
        let hex = |s: &str| s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hub_id.len() != 16 || !hex(hub_id) {
            return None;
        }
        Some(Status {
            build,
            protocol,
            hub_id: hub_id.to_string(),
        })
    }
}

/// Reads one line from `s`, a byte at a time so that nothing after its `\n` is consumed (the
/// sealed channel's lines follow a `connect`'s status line in the same stream): the line
/// without its `\n`, `None` when the stream ends before its first byte. An error when it is
/// longer than `max` bytes (its `\n` included), the stream ends inside it, or it is not UTF-8.
pub async fn read_line<S: AsyncRead + Unpin>(
    s: &mut S,
    max: usize,
) -> std::io::Result<Option<String>> {
    use std::io::{Error, ErrorKind};
    let mut line = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if s.read(&mut b).await? == 0 {
            if line.is_empty() {
                return Ok(None);
            }
            return Err(Error::new(
                ErrorKind::UnexpectedEof,
                "the stream ended inside a line",
            ));
        }
        if b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() >= max {
            return Err(Error::new(ErrorKind::InvalidData, "the line is too long"));
        }
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| Error::new(ErrorKind::InvalidData, "the line is not UTF-8"))
}

/// Makes `req` on the daemon socket at `socket`: connects, sends the request line and reads
/// the status line (within [`REQUEST_TIMEOUT`]). Returns the connection, positioned right
/// after the status line (a `connect`'s sealed channel comes next), and the status. Connecting
/// fails as the OS says (`NotFound`, `ConnectionRefused`: no daemon); a peer that does not
/// answer with a status line is an `InvalidData` error.
pub async fn request(socket: &Path, req: Request) -> std::io::Result<(UnixStream, Status)> {
    use std::io::{Error, ErrorKind};
    let mut stream = UnixStream::connect(socket).await?;
    let answer = tokio::time::timeout(REQUEST_TIMEOUT, async {
        stream.write_all(req.line().as_bytes()).await?;
        stream.flush().await?;
        read_line(&mut stream, MAX_STATUS_LINE).await
    })
    .await
    .map_err(|_| {
        Error::new(
            ErrorKind::TimedOut,
            format!(
                "no answer on {} within {REQUEST_TIMEOUT:?}",
                socket.display()
            ),
        )
    })??;
    match answer.as_deref().and_then(Status::parse) {
        Some(status) => Ok((stream, status)),
        None => Err(Error::new(
            ErrorKind::InvalidData,
            format!("{} did not answer as a box daemon", socket.display()),
        )),
    }
}

/// The daemon's listening socket. Dropping it removes the socket file, unless another one was
/// bound at the path since (compared by device and inode).
pub(crate) struct DaemonSocket {
    listener: UnixListener,
    path: PathBuf,
    id: (u64, u64),
}

impl DaemonSocket {
    /// Binds the daemon socket at `path` (see the module docs): its directory made private
    /// ([`ensure_socket_dir`]), a stale socket file removed when connecting to it fails, the
    /// socket set to 0600. Refused: a path too long for a Unix socket, something at the path
    /// that answers (another daemon of this socket) or that is not a socket.
    pub(crate) async fn bind(path: &Path) -> anyhow::Result<DaemonSocket> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
        let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) else {
            bail!(
                "the box daemon's socket {} has no directory",
                path.display()
            );
        };
        if path.as_os_str().len() > twr_core::paths::MAX_SOCKET_PATH {
            bail!(
                "the box daemon's socket path {} is too long ({} bytes; Unix socket paths can be \
                 at most {} bytes): set XDG_STATE_HOME to a shorter directory on the box",
                path.display(),
                path.as_os_str().len(),
                twr_core::paths::MAX_SOCKET_PATH
            );
        }
        ensure_socket_dir(dir)?;
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if UnixStream::connect(path).await.is_ok() {
                bail!(
                    "another box daemon answers on {}: two daemons of one hub key and box \
                     cannot run on one machine",
                    path.display()
                );
            }
            if !meta.file_type().is_socket() {
                bail!(
                    "{} exists and is not a socket: move it away so the box daemon can listen \
                     there",
                    path.display()
                );
            }
            tracing::info!(socket = %path.display(), "removing a stale box daemon socket");
            std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
        }
        let listener =
            UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", path.display()))?;
        let meta = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading {}", path.display()))?;
        Ok(DaemonSocket {
            listener,
            path: path.to_path_buf(),
            id: (meta.dev(), meta.ino()),
        })
    }
}

impl Drop for DaemonSocket {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt;
        if let Ok(meta) = std::fs::symlink_metadata(&self.path) {
            if (meta.dev(), meta.ino()) == self.id {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

/// Makes `dir` the daemon socket's directory: creates it with mode 0700 (and its missing
/// parents), then refuses it unless it is a directory of this user's that group and others
/// cannot access, on a path only this user or root can change
/// ([`twr_core::paths::check_ancestors`]). An existing directory is refused rather than
/// restricted: what was in it while it was open to others may not be the daemon's.
fn ensure_socket_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(parent) = dir.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("creating {}", dir.display())),
    }
    let meta = std::fs::metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let me = twr_core::paths::effective_uid();
    if meta.uid() != me {
        bail!(
            "{} belongs to another user (uid {}, the box daemon runs as uid {me}): the daemon's \
             socket must be in a directory of yours alone; remove it",
            dir.display(),
            meta.uid()
        );
    }
    let mode = meta.mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is accessible by other users (mode {mode:03o}): the box daemon's socket must be \
             in a directory only you can use; run `chmod 700 {}`",
            dir.display(),
            twr_core::paths::sh_word(&dir.to_string_lossy())
        );
    }
    twr_core::paths::check_ancestors(dir)
}

/// The hub's WebSocket over the daemon socket.
type DaemonWs = WebSocketStream<SealedStream<UnixStream>>;

/// What an opening connection ([`admit`]) hands the accept loop.
enum Admitted {
    /// A `stop` request was made.
    Stop,
    /// The `n`th accepted connection opened as the hub's (boxed: the WebSocket is large).
    Session(u64, Box<DaemonWs>),
}

/// What every opening connection needs.
struct Admission {
    box_name: String,
    hub_key: [u8; 32],
    /// The build the daemon says it runs ([`DaemonOptions::build`]).
    build: Option<String>,
    /// The status line ([`Status::line`]).
    status: String,
}

/// A session being served ([`serve`], after ending the one it replaces and waiting for
/// repository changes in progress).
type Session<'a> = Pin<Box<dyn std::future::Future<Output = Served> + Send + 'a>>;

/// The session of the `n`th accepted connection, and how to end it for a newer one.
struct Current<'a> {
    n: u64,
    session: Session<'a>,
    /// Set when a newer connection replaces this one.
    replace: watch::Sender<bool>,
}

impl Current<'_> {
    /// Ends the session for a newer connection and waits until it has: its reader task has
    /// ended ([`serve`]), so no command of its connection begins after this returns. (Quick:
    /// the reader is aborted, which takes effect at its next await.)
    async fn retire(self) {
        let _ = self.replace.send(true);
        let out = self.session.await;
        tracing::info!(
            ended = ?out.result.err().map(|e| format!("{e:#}")),
            "the connection a newer one replaces has ended"
        );
    }
}

/// The daemon's connection loop (see the module docs) until `stop` is set or the box fails;
/// a `stop` request wakes `asked` (the caller sets `stop` then). On the way out the socket
/// goes first, then a session still served drains as on any shutdown.
pub(crate) async fn accept_loop(
    opts: &RunnerOptions,
    daemon: &DaemonOptions,
    socket: DaemonSocket,
    lbox: &LocalBox,
    mut stop: watch::Receiver<bool>,
    asked: &Notify,
) {
    let admission = Arc::new(Admission {
        box_name: opts.box_name.clone(),
        hub_key: daemon.hub_key,
        build: daemon.build.clone(),
        status: Status {
            build: daemon.build.clone(),
            ..Status::of(&daemon.hub_key)
        }
        .line(),
    });
    let (tx, mut rx) = mpsc::channel::<Admitted>(MAX_ADMITTING);
    let mut admitting = tokio::task::JoinSet::new();
    let epoch_hint = std::sync::Mutex::new(None);
    let mut accepted = 0u64;
    let mut current: Option<Current<'_>> = None;
    let mut fatal = lbox.fatal_watch();
    tracing::info!(
        socket = %socket.path.display(),
        box_name = %opts.box_name,
        hub = %opts.hub_url,
        "the box daemon is listening"
    );
    loop {
        if *stop.borrow() || lbox.fatal_error().is_some() {
            break;
        }
        while admitting.try_join_next().is_some() {}
        tokio::select! {
            () = stopped(&mut stop) => break,
            // The guard `wait_for` returns is not `Send`: dropped at once, never held across
            // a branch's await.
            () = async { drop(fatal.wait_for(Option::is_some).await) } => break,
            r = socket.listener.accept() => match r {
                Ok((stream, _)) => {
                    accepted += 1;
                    if admitting.len() >= MAX_ADMITTING {
                        tracing::warn!("too many connections opening at once: dropping one");
                        continue;
                    }
                    admitting.spawn(admit(stream, accepted, admission.clone(), tx.clone()));
                }
                Err(e) => {
                    // Out of descriptors, say: try again shortly rather than spin.
                    tracing::warn!(error = %e, "accepting a connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(a) = rx.recv() => match a {
                Admitted::Stop => asked.notify_one(),
                Admitted::Session(n, ws) => {
                    if current.as_ref().is_some_and(|c| c.n > n) {
                        tracing::info!("an older connection finished opening after a newer one: dropped");
                        continue;
                    }
                    let previous = current.take();
                    if previous.is_some() {
                        tracing::info!("a newer connection from the hub replaces the one served");
                    }
                    let (replace, replaced) = watch::channel(false);
                    let stop = stop.clone();
                    let session = session(*ws, opts, lbox, &epoch_hint, stop, replaced, previous);
                    current = Some(Current { n, session, replace });
                }
            },
            out = async { current.as_mut().expect("guarded by the branch").session.as_mut().await },
                if current.is_some() =>
            {
                current = None;
                match out.result {
                    Ok(true) => break,
                    Ok(false) => tracing::info!("the hub's connection closed"),
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "the hub's connection was lost")
                    }
                }
            }
        }
    }
    // No new connection reaches a daemon on its way out.
    drop(socket);
    admitting.shutdown().await;
    if let Some(c) = current {
        if *stop.borrow() && lbox.fatal_error().is_none() {
            // Delivers the runtimes' exits to the hub (bounded by the shutdown grace).
            let _ = c.session.await;
        } else {
            // Ended, its reader joined: no command of the connection begins after this, while
            // the runner waits for the repository changes in progress (`crate::run_with`).
            c.retire().await;
        }
    }
}

/// Completes once `replaced` is set (a newer connection replaces the session); never when
/// its sender is gone.
async fn replaced_by_newer(mut replaced: watch::Receiver<bool>) {
    if replaced.wait_for(|r| *r).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// The session of a hub connection: first ends the session it replaces (`previous`) and waits
/// until that one's reader has ended ([`Current::retire`]), then waits (unless stopped or
/// replaced meanwhile) for the repository changes a previous connection sent to finish (the
/// hub takes a new connection as proof that none still runs), then [`serve`]s it until it
/// ends or `replaced` is set.
fn session<'a>(
    ws: DaemonWs,
    opts: &'a RunnerOptions,
    lbox: &'a LocalBox,
    epoch_hint: &'a std::sync::Mutex<Option<u64>>,
    mut stop: watch::Receiver<bool>,
    replaced: watch::Receiver<bool>,
    previous: Option<Current<'a>>,
) -> Session<'a> {
    Box::pin(async move {
        if let Some(p) = previous {
            p.retire().await;
        }
        let gone = Served {
            welcomed: false,
            result: Err(anyhow::anyhow!(
                "a newer connection from the hub replaced this one before it was served"
            )),
        };
        if *replaced.borrow() {
            return gone;
        }
        if lbox.changes_running() > 0 {
            tracing::info!(
                changes = lbox.changes_running(),
                "waiting for the repository changes in progress before serving the hub"
            );
            tokio::select! {
                () = lbox.changes_done() => {}
                () = stopped(&mut stop) => {
                    return Served {
                        welcomed: false,
                        result: Ok(true),
                    }
                }
                () = replaced_by_newer(replaced.clone()) => return gone,
            }
        }
        serve(
            ws,
            lbox,
            opts,
            epoch_hint,
            stop,
            replaced_by_newer(replaced),
        )
        .await
    })
}

/// Opens the `n`th accepted connection (see the module docs): checks its peer's user, reads
/// its request, answers the status line, and for a `connect` opens the sealed channel and the
/// WebSocket inside it. What becomes of it goes to `ready`; a connection that fails a step is
/// dropped (and logged when it got that far).
async fn admit(mut stream: UnixStream, n: u64, a: Arc<Admission>, ready: mpsc::Sender<Admitted>) {
    let me = twr_core::paths::effective_uid();
    match stream.peer_cred() {
        Ok(c) if c.uid() == me => {}
        Ok(c) => {
            tracing::warn!(
                uid = c.uid(),
                "dropping a connection from another user on the box daemon's socket"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "dropping a connection whose peer cannot be read");
            return;
        }
    }
    let line =
        tokio::time::timeout(REQUEST_TIMEOUT, read_line(&mut stream, MAX_REQUEST_LINE)).await;
    let Some(req) = line
        .ok()
        .and_then(Result::ok)
        .flatten()
        .as_deref()
        .and_then(Request::parse)
    else {
        tracing::debug!("dropping a connection that made no valid request");
        return;
    };
    let answered = tokio::time::timeout(REQUEST_TIMEOUT, async {
        stream.write_all(a.status.as_bytes()).await?;
        stream.flush().await
    })
    .await;
    if !matches!(answered, Ok(Ok(()))) {
        return;
    }
    match req {
        Request::Status => {
            let _ = stream.shutdown().await;
        }
        Request::Stop => {
            let _ = stream.shutdown().await;
            tracing::info!("a stop request came on the socket");
            let _ = ready.send(Admitted::Stop).await;
        }
        Request::Connect => {
            let opened = tokio::time::timeout(
                SEALED_TIMEOUT,
                twr_core::sealed::daemon_side(stream, &a.hub_key, &a.box_name, a.build.as_deref()),
            )
            .await;
            let sealed = match opened {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "a connection did not open the channel with this daemon's hub key: \
                         dropped (a hub with another key, or not a hub)"
                    );
                    return;
                }
                Err(_) => {
                    tracing::warn!("a connection did not open the channel in time: dropped");
                    return;
                }
            };
            let ws = tokio::time::timeout(
                SEALED_TIMEOUT,
                tokio_tungstenite::client_async_with_config(
                    "ws://twr-box/",
                    sealed,
                    Some(ws_config()),
                ),
            )
            .await;
            match ws {
                Ok(Ok((ws, _))) => {
                    let _ = ready.send(Admitted::Session(n, Box::new(ws))).await;
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "the WebSocket handshake with the hub failed")
                }
                Err(_) => tracing::warn!("the WebSocket handshake with the hub timed out"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_status_lines() {
        for r in [Request::Connect, Request::Status, Request::Stop] {
            let line = r.line();
            assert!(line.ends_with('\n') && line.len() <= MAX_REQUEST_LINE);
            assert_eq!(Request::parse(line.trim_end_matches('\n')), Some(r));
        }
        for bad in [
            "",
            "twr-box 1",
            "twr-box 1 ",
            "twr-box 2 connect",
            "twr-box 1 connect ",
            "twr-box 1 CONNECT",
            " twr-box 1 status",
        ] {
            assert_eq!(Request::parse(bad), None, "{bad:?}");
        }
        let s = Status {
            build: Some("0123456789abcdef".into()),
            protocol: 6,
            hub_id: "fedcba9876543210".into(),
        };
        assert_eq!(s.line(), "twr-box 1 0123456789abcdef 6 fedcba9876543210\n");
        assert_eq!(Status::parse(s.line().trim_end()), Some(s.clone()));
        let none = Status { build: None, ..s };
        assert_eq!(none.line(), "twr-box 1 - 6 fedcba9876543210\n");
        assert_eq!(Status::parse(none.line().trim_end()), Some(none));
        for bad in [
            "twr-box 1 - 6",
            "twr-box 1 - x fedcba9876543210",
            "twr-box 1 0123 6 fedcba9876543210",
            "twr-box 1 - 6 FEDCBA9876543210",
            "twr-box 1 - 6 fedcba987654321",
            "twr-box 1 - 6 fedcba9876543210 x",
            "twr-box 1 -  6 fedcba9876543210",
        ] {
            assert_eq!(Status::parse(bad), None, "{bad:?}");
        }
        let key = [7u8; 32];
        let mine = Status::of(&key);
        assert_eq!(mine.hub_id, twr_core::sealed::hub_id(&key));
        assert_eq!(mine.protocol, PROTOCOL_VERSION);
        assert_eq!(mine.build.as_deref(), twr_core::build_id());
    }

    /// A line is read up to its `\n` and no further; too long, cut short or not UTF-8 is an
    /// error, and an empty stream is no line.
    #[tokio::test]
    async fn lines_are_read_exactly() {
        let mut s: &[u8] = b"twr-box 1 status\nTWR-BOX-HELLO {}\n";
        assert_eq!(
            read_line(&mut s, 64).await.unwrap().as_deref(),
            Some("twr-box 1 status")
        );
        assert_eq!(s, b"TWR-BOX-HELLO {}\n");
        let mut s: &[u8] = b"";
        assert_eq!(read_line(&mut s, 64).await.unwrap(), None);
        let mut s: &[u8] = b"abc";
        assert!(read_line(&mut s, 64).await.is_err());
        let mut s: &[u8] = b"\xff\n";
        assert!(read_line(&mut s, 64).await.is_err());
        // `max` counts the `\n`.
        let mut s: &[u8] = b"abc\n";
        assert_eq!(read_line(&mut s, 4).await.unwrap().as_deref(), Some("abc"));
        let mut s: &[u8] = b"abcd\n";
        assert!(read_line(&mut s, 4).await.is_err());
    }

    /// Waits until nothing answers on the socket file at `path` any more: a process another
    /// test forks may hold a dropped listener's descriptor until it execs.
    async fn wait_unanswered(path: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(path).await.is_ok() {
            assert!(std::time::Instant::now() < deadline, "the listener stays");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The socket's directory is made private, or refused when others can use it; a stale
    /// socket file is replaced, a live one or another file refused; the socket is 0600 and
    /// goes when dropped.
    #[tokio::test]
    async fn the_socket_is_private_and_stale_ones_are_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("state/twr/box-daemons");
        let path = dir.join("0123456789abcdef.sock");
        let s = DaemonSocket::bind(&path).await.unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&path), 0o600);
        // A second one while the first listens.
        let e = DaemonSocket::bind(&path).await.err().unwrap().to_string();
        assert!(e.contains("another box daemon answers"), "{e}");
        drop(s);
        assert!(!path.exists());
        // A stale socket file (nobody listens): replaced.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        wait_unanswered(&path).await;
        let s = DaemonSocket::bind(&path).await.unwrap();
        UnixStream::connect(&path).await.unwrap();
        drop(s);
        // Something else at the path stays.
        std::fs::write(&path, "x").unwrap();
        let e = DaemonSocket::bind(&path).await.err().unwrap().to_string();
        assert!(e.contains("is not a socket"), "{e}");
        std::fs::remove_file(&path).unwrap();
        // A directory others can use is refused, not repaired.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = DaemonSocket::bind(&path).await.err().unwrap().to_string();
        assert!(e.contains("accessible by other users (mode 755)"), "{e}");
        assert!(e.contains("chmod 700"), "{e}");
        assert_eq!(mode(&dir), 0o755);
        // A socket bound by someone else at the path since is not removed by the old one.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let old = DaemonSocket::bind(&path).await.unwrap();
        std::fs::remove_file(&path).unwrap();
        let new = DaemonSocket::bind(&path).await.unwrap();
        drop(old);
        assert!(path.exists());
        drop(new);
        // Too long for a Unix socket.
        let long = tmp.path().join("x".repeat(120)).join("s.sock");
        let e = DaemonSocket::bind(&long).await.err().unwrap().to_string();
        assert!(e.contains("too long"), "{e}");
    }
}

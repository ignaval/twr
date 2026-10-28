//! Links: how a speaker reaches hubs. Each kind gives the speaker the same [`LinkEvent`]s
//! (attached, an item, detached) per workspace, and takes its state reports
//! ([`Link::report`]), so the speaker doesn't care which it has.
//!
//! - **Local** ([`LinkSpec::Local`]): this machine's hubs, one `speech.attach` connection per
//!   workspace (`hello` as `speak`). A hub that is not running is tried again every
//!   [`RETRY`], never started; so is one that answers `conflict` (another speaker of this
//!   device is attached: this one stands by).
//! - **SSH** ([`LinkSpec::Ssh`]): `ssh HOST -- twr speech feed --proto`, whose stdout carries
//!   the remote hubs' lines (`twr_core::speech::ProxyOut`) and whose stdin takes this speaker's
//!   `speech.attach` and `speech.state` (`ProxyIn`). No forwarding of any kind; the remote
//!   command is fixed words and a [`sh_word`]-quoted workspace name. The first connection
//!   may ask in the terminal (a host key, a passphrase); reconnections are `BatchMode=yes`,
//!   after 1, 2, 4 ... [`BACKOFF_MAX`] seconds (back to 1 after a minute connected). Every
//!   [`PING_EVERY`] the link writes a [`PROXY_PING`] line, which the proxy keeps to itself: a
//!   proxy that has had one ends once it hears nothing for [`PROXY_IDLE`], so one whose
//!   speaker vanished (a laptop that slept, a network that changed, the old sshd session kept
//!   alive for hours) lets go of its hubs, and the speaker that reconnects isn't stood by.
//! - **Manual** ([`LinkSpec::Manual`]): driven by hand through a [`ManualHub`] (tests, or a
//!   host program with its own transport).
//!
//! The attach params are made by the speaker at each attach ([`ParamsFn`]): they carry the
//! watermarks as they are then.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use twr_core::client::{Client, SpeechLink};
use twr_core::paths::{sh_word, workspace_name_error, Paths, DEFAULT_WORKSPACE};
use twr_core::proto::{codes, RpcError};
use twr_core::speech::{
    ProxyIn, ProxyOut, SpeechAttachAck, SpeechAttachParams, SpeechItem, SpeechStateParams,
    PROXY_VERSION,
};

/// How often a hub that is down, or a standby speaker, tries again.
pub const RETRY: Duration = Duration::from_secs(5);

/// The longest wait between SSH reconnections.
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How long an SSH connection must last for the backoff to start over.
pub const BACKOFF_RESET: Duration = Duration::from_secs(60);

/// How long connecting to a local hub and greeting it may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How often an SSH link pings its proxy ([`PROXY_PING`]).
pub const PING_EVERY: Duration = Duration::from_secs(20);

/// How long `twr speech feed --proto`, once pinged, waits for a line from its speaker before
/// it takes the speaker for gone and ends.
pub const PROXY_IDLE: Duration = Duration::from_secs(90);

/// The method of the ping line ([`ProxyIn`] with workspace `""`, id 0 and no params): for the
/// proxy itself, never passed on to a hub, never answered. A proxy older than pings answers it
/// as a workspace it doesn't serve, which the link ignores.
pub const PROXY_PING: &str = "proxy.ping";

/// The most characters of a hub's words (a refusal, why it closed the connection) a note
/// takes.
pub const HUB_WORDS_MAX: usize = 300;

/// What a hub (or ssh) said, as a note may show it: without control or invisible characters
/// ([`twr_core::speech::clean`]), and at most [`HUB_WORDS_MAX`] characters.
pub fn hub_words(text: &str) -> String {
    let text = twr_core::speech::clean(text);
    if text.chars().count() <= HUB_WORDS_MAX {
        return text;
    }
    let mut cut: String = text.chars().take(HUB_WORDS_MAX).collect();
    cut.push('…');
    cut
}

/// Which workspaces a link reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspaces {
    /// The current one: a local link's `Paths` workspace; over SSH, the remote's default
    /// (its `$TWR_WORKSPACE`, else `default`).
    Current,
    /// This one.
    One(String),
    /// Every workspace (a local link looks for new ones every [`RETRY`]).
    All,
}

/// Makes a speaker's `speech.attach` params for a workspace, at the moment it attaches.
pub type ParamsFn = Arc<dyn Fn(&str) -> SpeechAttachParams + Send + Sync>;

/// What a link tells the speaker.
#[derive(Debug, Clone, PartialEq)]
pub enum LinkEvent {
    /// A workspace's hub attached the speaker.
    Attached {
        /// The workspace.
        ws: String,
        /// Its ack.
        ack: SpeechAttachAck,
    },
    /// An item of a workspace's feed.
    Item {
        /// The workspace.
        ws: String,
        /// The item.
        item: SpeechItem,
    },
    /// A workspace's connection ended (it is tried again).
    Detached {
        /// The workspace.
        ws: String,
        /// Why.
        why: String,
    },
    /// Something to tell the user (ssh's own words, a standby).
    Note(String),
}

/// Which link to start.
pub enum LinkSpec {
    /// This machine's hubs of the root `paths` belongs to.
    Local {
        /// Paths of the root (any workspace of it; [`Workspaces::Current`] is this one).
        paths: Paths,
        /// Which workspaces.
        workspaces: Workspaces,
    },
    /// A remote machine's hubs over SSH.
    Ssh(SshSpec),
    /// By hand ([`manual`]).
    Manual(ManualLink),
}

impl std::fmt::Debug for LinkSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkSpec::Local { paths, workspaces } => f
                .debug_struct("Local")
                .field("paths", paths)
                .field("workspaces", workspaces)
                .finish(),
            LinkSpec::Ssh(s) => f.debug_tuple("Ssh").field(s).finish(),
            LinkSpec::Manual(_) => f.write_str("Manual"),
        }
    }
}

impl LinkSpec {
    /// The host part of the live switches' keys: [`crate::state::LOCAL_HOST`], or the SSH host
    /// as given.
    pub fn host(&self) -> String {
        match self {
            LinkSpec::Ssh(s) => s.host.clone(),
            _ => crate::state::LOCAL_HOST.to_string(),
        }
    }
}

/// Why `host` can't be given to ssh as a host: empty, starting with `-` (an option), or holding
/// whitespace or a control character.
pub fn ssh_host_error(host: &str) -> Option<String> {
    if host.is_empty() {
        return Some("--ssh: name a host".into());
    }
    if host.starts_with('-') {
        return Some(format!("--ssh {host:?}: a host can't start with `-`"));
    }
    if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some(format!(
            "--ssh {host:?}: a host can't hold spaces or control characters"
        ));
    }
    None
}

/// An SSH link: the host and workspaces, and the `ssh` program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshSpec {
    host: String,
    workspaces: Workspaces,
    program: PathBuf,
}

impl SshSpec {
    /// `ssh` to `host` for `workspaces`. Fails on a host [`ssh_host_error`] refuses or a
    /// workspace name [`workspace_name_error`] refuses.
    pub fn new(host: &str, workspaces: Workspaces) -> anyhow::Result<SshSpec> {
        if let Some(e) = ssh_host_error(host) {
            bail!(e);
        }
        if let Workspaces::One(ws) = &workspaces {
            if let Some(e) = workspace_name_error(ws) {
                bail!(e);
            }
        }
        Ok(SshSpec {
            host: host.to_string(),
            workspaces,
            program: "ssh".into(),
        })
    }

    /// Runs `program` instead of `ssh` from `PATH` (tests).
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> SshSpec {
        self.program = program.into();
        self
    }

    /// The host.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// ssh's arguments: no forwarding, keepalives, `BatchMode=yes` when `batch` (a
    /// reconnection), the host, then the remote command (fixed words and the workspace as a
    /// shell word): `twr [-w WS] speech feed --proto [--all-workspaces]`.
    pub fn args(&self, batch: bool) -> Vec<String> {
        let mut a: Vec<String> = [
            "-T",
            "-o",
            "ForwardAgent=no",
            "-o",
            "ForwardX11=no",
            "-o",
            "ClearAllForwardings=yes",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
        ]
        .map(String::from)
        .into();
        if batch {
            a.extend(["-o".to_string(), "BatchMode=yes".to_string()]);
        }
        a.extend([self.host.clone(), "--".to_string(), "twr".to_string()]);
        if let Workspaces::One(ws) = &self.workspaces {
            a.extend(["-w".to_string(), sh_word(ws)]);
        }
        a.extend(["speech", "feed", "--proto"].map(String::from));
        if self.workspaces == Workspaces::All {
            a.push("--all-workspaces".into());
        }
        a
    }
}

/// The speaker's latest state per workspace, and a counter of changes: each connection sends
/// its workspace's when it changed.
struct Board {
    states: Mutex<HashMap<String, SpeechStateParams>>,
    version: watch::Sender<u64>,
    /// Where a place to start from is saved before the speaker hears of it ([`Persist`]).
    persist: PersistFn,
}

/// A place in a thread the speaker must have saved before the next attach is made: the head
/// an attach began at (the feed then reads the orchestrator from there), or a new orchestrator
/// the feed reads from its start. Saved by the link itself, before it passes the attach or the
/// item on: an attach made meanwhile (a quick reconnect) then already carries it.
#[derive(Debug, Clone, Copy)]
pub enum Persist<'a> {
    /// An attach's ack.
    Attached(&'a SpeechAttachAck),
    /// The feed follows a new orchestrator thread from its start.
    Orchestrator(&'a str),
}

/// Saves a [`Persist`] of a workspace; called on the link's own tasks.
pub type PersistFn = Arc<dyn Fn(&str, Persist<'_>) + Send + Sync>;

impl std::fmt::Debug for Board {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Board")
            .field("states", &self.states)
            .finish_non_exhaustive()
    }
}

impl Board {
    fn new(persist: PersistFn) -> Board {
        Board {
            states: Mutex::new(HashMap::new()),
            version: watch::channel(0).0,
            persist,
        }
    }

    /// Saves `p` of `ws` ([`Persist`]).
    fn persist(&self, ws: &str, p: Persist<'_>) {
        (self.persist)(ws, p);
    }

    /// Saves what an item of `ws` asks to be saved before the speaker hears of it.
    fn persist_item(&self, ws: &str, item: &SpeechItem) {
        if let SpeechItem::Orchestrator { thread } = item {
            self.persist(ws, Persist::Orchestrator(thread));
        }
    }

    fn get(&self, ws: &str) -> Option<SpeechStateParams> {
        self.states
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(ws)
            .cloned()
    }

    fn set(&self, ws: &str, state: SpeechStateParams) {
        let mut states = self.states.lock().unwrap_or_else(|p| p.into_inner());
        if states.get(ws) != Some(&state) {
            states.insert(ws.to_string(), state);
            drop(states);
            self.version.send_modify(|v| *v += 1);
        }
    }
}

/// A started link: its events, and where the speaker's state goes. Dropping it ends its
/// connections (and its ssh).
pub struct Link {
    events: mpsc::Receiver<LinkEvent>,
    board: Arc<Board>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").finish_non_exhaustive()
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Link {
    /// Starts `spec`'s connections; `params` makes each attach's params.
    pub fn start(spec: LinkSpec, params: ParamsFn) -> Link {
        Link::start_with(spec, params, Arc::new(|_: &str, _: Persist<'_>| {}))
    }

    /// [`Link::start`], with `persist` saving each [`Persist`] before the event that carries
    /// it is passed on.
    pub fn start_with(spec: LinkSpec, params: ParamsFn, persist: PersistFn) -> Link {
        let (tx, events) = mpsc::channel(1024);
        let board = Arc::new(Board::new(persist));
        let task = match spec {
            LinkSpec::Local { paths, workspaces } => {
                tokio::spawn(local(paths, workspaces, params, tx, board.clone()))
            }
            LinkSpec::Ssh(s) => tokio::spawn(ssh(s, params, tx, board.clone())),
            LinkSpec::Manual(m) => tokio::spawn(m.run(params, tx, board.clone())),
        };
        Link {
            events,
            board,
            tasks: vec![task],
        }
    }

    /// The next event; `None` when the link ended (never, but for a manual one).
    pub async fn next(&mut self) -> Option<LinkEvent> {
        self.events.recv().await
    }

    /// Reports the speaker's state for `ws` to its hub (now if connected, else once it is).
    pub fn report(&self, ws: &str, state: SpeechStateParams) {
        self.board.set(ws, state);
    }
}

// ---- Local ----

/// The local link: a connection task per workspace, and, for [`Workspaces::All`], a look for
/// new workspaces every [`RETRY`].
async fn local(
    paths: Paths,
    workspaces: Workspaces,
    params: ParamsFn,
    tx: mpsc::Sender<LinkEvent>,
    board: Arc<Board>,
) {
    let root = paths.root();
    let mut running: HashMap<String, AbortOnDrop> = HashMap::new();
    loop {
        let names = match &workspaces {
            Workspaces::Current => vec![paths.workspace_name().to_string()],
            Workspaces::One(ws) => vec![ws.clone()],
            Workspaces::All => {
                let mut names = vec![DEFAULT_WORKSPACE.to_string()];
                match root.named_workspaces() {
                    Ok(n) => {
                        names.extend(n);
                        // A workspace removed since (`twr ws rm`): its task ends, and the
                        // speaker hears it is gone.
                        let gone: Vec<String> = running
                            .keys()
                            .filter(|w| !names.contains(w))
                            .cloned()
                            .collect();
                        for ws in gone {
                            running.remove(&ws);
                            let why = "the workspace is gone".to_string();
                            let _ = tx.send(LinkEvent::Detached { ws, why }).await;
                        }
                    }
                    Err(e) => tracing::debug!("listing workspaces: {e}"),
                }
                names
            }
        };
        for ws in names {
            running.entry(ws.clone()).or_insert_with(|| {
                AbortOnDrop(tokio::spawn(local_ws(
                    root.clone(),
                    ws,
                    params.clone(),
                    tx.clone(),
                    board.clone(),
                )))
            });
        }
        if workspaces != Workspaces::All {
            // Nothing more to look for: wait until the link is dropped.
            return futures::future::pending().await;
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Aborts a task when dropped.
pub(crate) struct AbortOnDrop(pub(crate) JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// How one local attach ended.
enum Ended {
    /// The hub is not running (or not answering): silent.
    NotThere(String),
    /// Another speaker of this device is attached.
    Standby,
    /// The hub refused the attach otherwise (one older than `speech.attach`, say).
    Refused(String),
    /// It was attached, and the connection ended.
    Detached(String),
    /// The speaker is gone.
    Gone,
}

/// One workspace's connection, again every [`RETRY`] after it ends.
async fn local_ws(
    root: Paths,
    ws: String,
    params: ParamsFn,
    tx: mpsc::Sender<LinkEvent>,
    board: Arc<Board>,
) {
    let mut standby = false;
    let mut refused: Option<String> = None;
    loop {
        match local_once(&root, &ws, &params, &tx, &board).await {
            Ended::Gone => return,
            Ended::NotThere(why) => {
                tracing::debug!(ws, "hub not reachable: {why}");
            }
            Ended::Refused(why) => {
                // Said once (until it changes), then tried again quietly.
                if refused.as_deref() != Some(why.as_str()) {
                    let note = format!("{ws}: the hub refused the speaker: {why}");
                    if tx.send(LinkEvent::Note(note)).await.is_err() {
                        return;
                    }
                }
                refused = Some(why);
            }
            Ended::Standby => {
                if !standby {
                    let note =
                        format!("{ws}: another speaker of this device is attached; standing by");
                    if tx.send(LinkEvent::Note(note)).await.is_err() {
                        return;
                    }
                }
                standby = true;
            }
            Ended::Detached(why) => {
                standby = false;
                refused = None;
                let ev = LinkEvent::Detached {
                    ws: ws.clone(),
                    why,
                };
                if tx.send(ev).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// Connects to `ws`'s hub, attaches, and passes items on until the connection ends.
async fn local_once(
    root: &Paths,
    ws: &str,
    params: &ParamsFn,
    tx: &mpsc::Sender<LinkEvent>,
    board: &Board,
) -> Ended {
    // The hub's socket as `twr` finds it: the default workspace's may be `[hub] socket` in the
    // config (a named workspace ignores it, `Config::in_workspace`).
    let socket = match root.workspace(ws) {
        Ok(p) => match twr_core::config::Config::load_from(&p) {
            Ok(config) => config.socket_path(&p),
            Err(_) => p.socket,
        },
        Err(e) => return Ended::NotThere(format!("{e:#}")),
    };
    let greet = async {
        let mut c = Client::connect(&socket).await?;
        let hello = c.hello(json!({ "client": "speak" })).await?;
        match hello["workspace"].as_str() {
            Some(w) if w != ws => bail!("the hub at {} serves workspace {w}", socket.display()),
            None if ws != DEFAULT_WORKSPACE => {
                bail!("the hub at {} is older than workspaces", socket.display())
            }
            _ => {}
        }
        anyhow::Ok(c)
    };
    let client = match tokio::time::timeout(CONNECT_TIMEOUT, greet).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return Ended::NotThere(format!("{e:#}")),
        Err(_) => return Ended::NotThere("no answer".into()),
    };
    let p = params(ws);
    let (ack, mut link) = match SpeechLink::attach(client, &p).await {
        Ok(x) => x,
        Err(e) => {
            return match e.downcast_ref::<RpcError>() {
                Some(r) if r.code == codes::CONFLICT => Ended::Standby,
                Some(r) => Ended::Refused(hub_words(&r.message)),
                None => Ended::NotThere(format!("{e:#}")),
            }
        }
    };
    board.persist(ws, Persist::Attached(&ack));
    let ev = LinkEvent::Attached {
        ws: ws.to_string(),
        ack,
    };
    if tx.send(ev).await.is_err() {
        return Ended::Gone;
    }
    let mut version = board.version.subscribe();
    let mut sent: Option<SpeechStateParams> = None;
    loop {
        if let Some(s) = board.get(ws).filter(|s| sent.as_ref() != Some(s)) {
            if let Err(e) = link.state(&s).await {
                return Ended::Detached(hub_words(&format!("{e:#}")));
            }
            sent = Some(s);
        }
        tokio::select! {
            item = link.next() => match item {
                Some(Ok(item)) => {
                    board.persist_item(ws, &item);
                    let ev = LinkEvent::Item { ws: ws.to_string(), item };
                    if tx.send(ev).await.is_err() {
                        return Ended::Gone;
                    }
                }
                // It may quote the hub's error.
                Some(Err(e)) => return Ended::Detached(hub_words(&format!("{e:#}"))),
                None => return Ended::Detached("the hub closed the connection".into()),
            },
            changed = version.changed() => {
                if changed.is_err() {
                    return Ended::Gone;
                }
            }
        }
    }
}

// ---- SSH ----

/// The SSH link: runs ssh, a session per connection, again after a backoff.
async fn ssh(spec: SshSpec, params: ParamsFn, tx: mpsc::Sender<LinkEvent>, board: Arc<Board>) {
    let mut connected_once = false;
    let mut backoff = Duration::from_secs(1);
    loop {
        let started = tokio::time::Instant::now();
        let hello = match ssh_once(&spec, connected_once, &params, &tx, &board).await {
            Ok(hello) => hello,
            Err(e) => {
                let note = format!("ssh {}: {e:#}", spec.host);
                if tx.send(LinkEvent::Note(note)).await.is_err() {
                    return;
                }
                false
            }
        };
        if tx.is_closed() {
            return;
        }
        connected_once |= hello;
        if started.elapsed() >= BACKOFF_RESET {
            backoff = Duration::from_secs(1);
        }
        let note = format!(
            "ssh {}: connection ended; trying again in {} s",
            spec.host,
            backoff.as_secs()
        );
        if tx.send(LinkEvent::Note(note)).await.is_err() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

/// One ssh connection, until it ends; whether the remote proxy said hello.
async fn ssh_once(
    spec: &SshSpec,
    batch: bool,
    params: &ParamsFn,
    tx: &mpsc::Sender<LinkEvent>,
    board: &Board,
) -> anyhow::Result<bool> {
    let mut child = tokio::process::Command::new(&spec.program)
        .args(spec.args(batch))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("running {}: {e}", spec.program.display()))?;
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    let stderr = child.stderr.take().map(|err| {
        let (tx, host) = (tx.clone(), spec.host.clone());
        AbortOnDrop(tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = hub_words(&line);
                tracing::warn!(target: "ssh", "{host}: {line}");
                let _ = tx
                    .send(LinkEvent::Note(format!("ssh {host}: {line}")))
                    .await;
            }
        }))
    });
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        bail!("ssh's pipes are missing");
    };
    let hello = proxy_session(stdout, stdin, params, tx, board).await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    drop(stderr);
    Ok(hello)
}

/// What the speaker knows of a workspace behind the proxy.
#[derive(Debug, Default)]
struct Remote {
    /// The proxy is connected to its hub.
    up: bool,
    /// The id of the `speech.attach` waiting for its answer.
    attaching: Option<u64>,
    /// Attached.
    attached: bool,
    /// When to send `speech.attach` again (a standby, a refusal).
    retry_at: Option<tokio::time::Instant>,
    /// Standing by (said once).
    standby: bool,
    /// The last refusal said (once).
    refused: Option<String>,
    /// The state last sent.
    sent: Option<SpeechStateParams>,
}

/// One session with `twr speech feed --proto`: its lines from `reader`, this speaker's
/// requests to `writer` (see the module documentation). Attaches on each `up` of a workspace,
/// and again every [`RETRY`] while the hub refuses (`conflict`: standby); pings every
/// [`PING_EVERY`]. Ends when the proxy's output ends; whether it said hello.
async fn proxy_session<R, W>(
    reader: R,
    mut writer: W,
    params: &ParamsFn,
    tx: &mpsc::Sender<LinkEvent>,
    board: &Board,
) -> bool
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Unpin,
{
    // Lines are read by a task of their own: a read cut short by `select!` would lose a line.
    let (line_tx, mut lines) = mpsc::channel::<Value>(256);
    let _reader = AbortOnDrop(tokio::spawn(async move {
        let mut r = BufReader::new(reader);
        loop {
            match twr_core::proto::read_frame::<_, Value>(&mut r).await {
                Ok(Some(v)) => {
                    if line_tx.send(v).await.is_err() {
                        return;
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    tracing::warn!("reading the speech feed: {e}");
                    return;
                }
            }
        }
    }));
    let mut hello = false;
    let mut next_id = 1u64;
    let mut wss: HashMap<String, Remote> = HashMap::new();
    let mut version = board.version.subscribe();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING_EVERY, PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    macro_rules! send {
        ($ev:expr) => {
            if tx.send($ev).await.is_err() {
                break;
            }
        };
    }
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { break };
                let Ok(out) = serde_json::from_value::<ProxyOut>(line) else {
                    tracing::debug!("skipping a speech feed line");
                    continue;
                };
                match out {
                    ProxyOut::Hello { proxy, .. } => {
                        if proxy != PROXY_VERSION {
                            send!(LinkEvent::Note(format!(
                                "the remote twr speaks speech feed version {proxy}, this one \
                                 {PROXY_VERSION}: update twr on both"
                            )));
                            break;
                        }
                        hello = true;
                    }
                    ProxyOut::Up { ws, .. } => {
                        if let Some(e) = workspace_name_error(&ws) {
                            tracing::warn!("speech feed: {e}");
                            continue;
                        }
                        let r = wss.entry(ws.clone()).or_default();
                        let was = std::mem::take(&mut r.attached);
                        r.up = true;
                        r.sent = None;
                        if was {
                            send!(LinkEvent::Detached { ws: ws.clone(), why: "the hub reconnected".into() });
                        }
                        if write_attach(&mut writer, &ws, &mut next_id, &mut wss, params).await.is_err() {
                            break;
                        }
                    }
                    ProxyOut::Down { ws, .. } => {
                        if let Some(r) = wss.get_mut(&ws) {
                            let was = r.attached;
                            *r = Remote::default();
                            if was {
                                send!(LinkEvent::Detached { ws, why: "the hub went away".into() });
                            }
                        }
                    }
                    ProxyOut::Line { ws, msg } => {
                        let Some(r) = wss.get_mut(&ws) else { continue };
                        if msg.get("stream").and_then(Value::as_str) == Some("speech") {
                            if !r.attached {
                                continue;
                            }
                            match serde_json::from_value::<SpeechItem>(msg["item"].clone()) {
                                Ok(item) => {
                                    board.persist_item(&ws, &item);
                                    send!(LinkEvent::Item { ws, item })
                                }
                                Err(e) => tracing::debug!("skipping a speech item: {e}"),
                            }
                            continue;
                        }
                        let id = msg.get("id").and_then(Value::as_u64);
                        if id.is_some() && id == r.attaching {
                            r.attaching = None;
                            if let Some(err) = msg.get("error") {
                                let err: RpcError = serde_json::from_value(err.clone())
                                    .unwrap_or_else(|_| RpcError::new(codes::INTERNAL, err.to_string()));
                                r.retry_at = r.up.then(|| tokio::time::Instant::now() + RETRY);
                                if err.code == codes::CONFLICT {
                                    if !std::mem::replace(&mut r.standby, true) {
                                        send!(LinkEvent::Note(format!(
                                            "{ws}: another speaker of this device is attached; standing by"
                                        )));
                                    }
                                } else {
                                    let why = hub_words(&err.message);
                                    if r.refused.as_deref() != Some(why.as_str()) {
                                        send!(LinkEvent::Note(format!(
                                            "{ws}: the hub refused the speaker: {why}"
                                        )));
                                        r.refused = Some(why);
                                    }
                                }
                                continue;
                            }
                            match serde_json::from_value::<SpeechAttachAck>(msg["result"].clone()) {
                                Ok(ack) => {
                                    r.attached = true;
                                    r.standby = false;
                                    r.refused = None;
                                    r.sent = None;
                                    board.persist(&ws, Persist::Attached(&ack));
                                    send!(LinkEvent::Attached { ws: ws.clone(), ack });
                                }
                                Err(e) => {
                                    tracing::warn!(ws, "reading the speech.attach ack: {e}");
                                    r.retry_at = r.up.then(|| tokio::time::Instant::now() + RETRY);
                                }
                            }
                        } else if id == Some(0) && msg.get("error").is_some() && r.attached {
                            // The hub's error for the connection, which it closes next.
                            r.attached = false;
                            send!(LinkEvent::Detached { ws, why: "the hub closed the connection".into() });
                        }
                    }
                }
            }
            changed = version.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            _ = tick.tick() => {
                let now = tokio::time::Instant::now();
                let due: Vec<String> = wss
                    .iter()
                    .filter(|(_, r)| r.up && !r.attached && r.attaching.is_none()
                        && r.retry_at.is_some_and(|t| t <= now))
                    .map(|(ws, _)| ws.clone())
                    .collect();
                let mut failed = false;
                for ws in due {
                    failed |= write_attach(&mut writer, &ws, &mut next_id, &mut wss, params).await.is_err();
                }
                if failed {
                    break;
                }
            }
            _ = ping.tick() => {
                let req = ProxyIn {
                    ws: String::new(),
                    id: 0,
                    method: PROXY_PING.into(),
                    params: Value::Null,
                };
                if write_line(&mut writer, &req).await.is_err() {
                    break;
                }
            }
        }
        // States changed since last sent, to the workspaces attached.
        let mut failed = false;
        for (ws, r) in wss.iter_mut().filter(|(_, r)| r.attached) {
            let Some(s) = board.get(ws).filter(|s| r.sent.as_ref() != Some(s)) else {
                continue;
            };
            let req = ProxyIn {
                ws: ws.clone(),
                id: next_id,
                method: "speech.state".into(),
                params: serde_json::to_value(&s).unwrap_or(Value::Null),
            };
            next_id += 1;
            failed |= write_line(&mut writer, &req).await.is_err();
            r.sent = Some(s);
        }
        if failed {
            break;
        }
    }
    for (ws, r) in wss {
        if r.attached {
            let _ = tx
                .send(LinkEvent::Detached {
                    ws,
                    why: "the ssh connection ended".into(),
                })
                .await;
        }
    }
    hello
}

/// Sends `speech.attach` for `ws` through the proxy.
async fn write_attach<W: AsyncWrite + Unpin>(
    writer: &mut W,
    ws: &str,
    next_id: &mut u64,
    wss: &mut HashMap<String, Remote>,
    params: &ParamsFn,
) -> anyhow::Result<()> {
    let id = *next_id;
    *next_id += 1;
    let r = wss.entry(ws.to_string()).or_default();
    r.attaching = Some(id);
    r.retry_at = None;
    let req = ProxyIn {
        ws: ws.to_string(),
        id,
        method: "speech.attach".into(),
        params: serde_json::to_value(params(ws))?,
    };
    write_line(writer, &req).await
}

/// Writes `req` as one line.
async fn write_line<W: AsyncWrite + Unpin>(writer: &mut W, req: &ProxyIn) -> anyhow::Result<()> {
    let line = twr_core::proto::encode_line(req)?;
    writer.write_all(line.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

// ---- Manual ----

/// What a [`ManualHub`] makes its link do.
#[derive(Debug, Clone, PartialEq)]
pub enum ManualEvent {
    /// `ws`'s hub attaches the speaker with `ack` (its params go to [`ManualHub::attaches`]).
    Connect {
        /// The workspace.
        ws: String,
        /// The ack.
        ack: SpeechAttachAck,
    },
    /// An item of `ws`'s feed.
    Item {
        /// The workspace.
        ws: String,
        /// The item.
        item: SpeechItem,
    },
    /// `ws`'s connection ends.
    Disconnect {
        /// The workspace.
        ws: String,
    },
}

/// The speaker's side of a link driven by hand ([`manual`]).
#[derive(Debug)]
pub struct ManualLink {
    events: mpsc::UnboundedReceiver<ManualEvent>,
    attaches: mpsc::UnboundedSender<(String, SpeechAttachParams)>,
    states: mpsc::UnboundedSender<(String, SpeechStateParams)>,
}

/// The hand on a [`ManualLink`]: what its hubs do, and what the speaker sent them.
#[derive(Debug)]
pub struct ManualHub {
    /// Makes the link do things.
    pub events: mpsc::UnboundedSender<ManualEvent>,
    /// The params of each attach, by workspace.
    pub attaches: mpsc::UnboundedReceiver<(String, SpeechAttachParams)>,
    /// Each state report, by workspace (only to connected ones, only when it changed).
    pub states: mpsc::UnboundedReceiver<(String, SpeechStateParams)>,
}

/// A link driven by hand, and the hand.
pub fn manual() -> (ManualLink, ManualHub) {
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let (at_tx, at_rx) = mpsc::unbounded_channel();
    let (st_tx, st_rx) = mpsc::unbounded_channel();
    (
        ManualLink {
            events: ev_rx,
            attaches: at_tx,
            states: st_tx,
        },
        ManualHub {
            events: ev_tx,
            attaches: at_rx,
            states: st_rx,
        },
    )
}

impl ManualLink {
    /// Plays the hand's events as link events, and passes state reports on.
    async fn run(mut self, params: ParamsFn, tx: mpsc::Sender<LinkEvent>, board: Arc<Board>) {
        let mut connected: HashMap<String, Option<SpeechStateParams>> = HashMap::new();
        let mut version = board.version.subscribe();
        loop {
            tokio::select! {
                ev = self.events.recv() => {
                    let ev = match ev {
                        None => return,
                        Some(ManualEvent::Connect { ws, ack }) => {
                            let _ = self.attaches.send((ws.clone(), params(&ws)));
                            connected.insert(ws.clone(), None);
                            board.persist(&ws, Persist::Attached(&ack));
                            LinkEvent::Attached { ws, ack }
                        }
                        Some(ManualEvent::Item { ws, item }) => {
                            board.persist_item(&ws, &item);
                            LinkEvent::Item { ws, item }
                        }
                        Some(ManualEvent::Disconnect { ws }) => {
                            connected.remove(&ws);
                            LinkEvent::Detached { ws, why: "disconnected".into() }
                        }
                    };
                    if tx.send(ev).await.is_err() {
                        return;
                    }
                }
                changed = version.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            for (ws, sent) in connected.iter_mut() {
                if let Some(s) = board.get(ws).filter(|s| sent.as_ref() != Some(s)) {
                    let _ = self.states.send((ws.clone(), s.clone()));
                    *sent = Some(s);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use twr_core::speech::{CtlOp, SpeechAttachParams, TurnsRead};

    fn params() -> ParamsFn {
        Arc::new(|ws: &str| SpeechAttachParams {
            device: "dev".into(),
            name: format!("speaker of {ws}"),
            engine: "fake".into(),
            voice: None,
            live: false,
            read: twr_core::speech::default_read(),
            turn_from: TurnsRead::All,
            watermarks: Default::default(),
            catchup_secs: 120,
        })
    }

    #[test]
    fn ssh_arguments_hold_no_text_and_batch_only_on_reconnect() {
        let one = SshSpec::new("me@box", Workspaces::One("client-x".into())).unwrap();
        let first = one.args(false);
        assert_eq!(
            first.join(" "),
            "-T -o ForwardAgent=no -o ForwardX11=no -o ClearAllForwardings=yes \
             -o ServerAliveInterval=15 -o ServerAliveCountMax=3 me@box -- \
             twr -w client-x speech feed --proto"
        );
        assert!(one
            .args(true)
            .join(" ")
            .contains("-o BatchMode=yes me@box --"));
        assert!(!first.iter().any(|a| a.contains("BatchMode")));
        let all = SshSpec::new("box", Workspaces::All).unwrap().args(false);
        assert!(all
            .join(" ")
            .ends_with("box -- twr speech feed --proto --all-workspaces"));
        let cur = SshSpec::new("box", Workspaces::Current)
            .unwrap()
            .args(false);
        assert!(cur.join(" ").ends_with("box -- twr speech feed --proto"));
        for bad in ["", "-oProxyCommand=x", "a b", "a\nb", "a\u{7}"] {
            assert!(SshSpec::new(bad, Workspaces::All).is_err(), "{bad:?}");
        }
        assert!(SshSpec::new("box", Workspaces::One("Bad Name".into())).is_err());
        assert!(SshSpec::new("box", Workspaces::One("x'; rm -rf ~".into())).is_err());
    }

    /// Writes `v` as a line of the proxy's.
    async fn put<W: AsyncWrite + Unpin>(w: &mut W, v: Value) {
        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    /// A session against a scripted proxy: attach on `up`, items, standby, state, `down`.
    #[tokio::test(start_paused = true)]
    async fn a_proxy_session_attaches_on_each_up_and_passes_items() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(ours);
        let (tr, mut tw) = tokio::io::split(theirs);
        let mut from_speaker = BufReader::new(tr).lines();
        let (tx, mut events) = mpsc::channel(64);
        let board = Arc::new(Board::new(Arc::new(|_: &str, _: Persist<'_>| {})));
        let b2 = board.clone();
        let session = tokio::spawn(async move { proxy_session(r, w, &params(), &tx, &b2).await });
        put(
            &mut tw,
            json!({"proxy": 1, "host": "box", "workspaces": ["default"]}),
        )
        .await;
        put(&mut tw, json!({"ws": "default", "up": true})).await;
        let req: ProxyIn =
            serde_json::from_str(&from_speaker.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            (req.ws.as_str(), req.method.as_str()),
            ("default", "speech.attach")
        );
        assert_eq!(req.params["name"], "speaker of default");
        // Standby first: tried again after RETRY.
        let refuse = json!({"ws": "default", "msg": {"id": req.id, "error": {"code": "conflict", "message": "x"}}});
        put(&mut tw, refuse).await;
        assert!(
            matches!(events.recv().await, Some(LinkEvent::Note(n)) if n.contains("standing by"))
        );
        let again: ProxyIn =
            serde_json::from_str(&from_speaker.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(again.method, "speech.attach");
        assert!(again.id > req.id);
        let ok = json!({"ws": "default", "msg": {"id": again.id, "result": {"orchestrator": "T", "head": 9}}});
        put(&mut tw, ok).await;
        match events.recv().await {
            Some(LinkEvent::Attached { ws, ack }) => {
                assert_eq!(ws, "default");
                assert_eq!((ack.orchestrator.as_deref(), ack.head), (Some("T"), 9));
            }
            e => panic!("{e:?}"),
        }
        let item = json!({"ws": "default", "msg": {"stream": "speech", "item": {"item": "ctl", "op": "stop"}}});
        put(&mut tw, item).await;
        assert_eq!(
            events.recv().await,
            Some(LinkEvent::Item {
                ws: "default".into(),
                item: SpeechItem::Ctl {
                    op: CtlOp::Stop,
                    value: None
                }
            })
        );
        board.set(
            "default",
            SpeechStateParams {
                queued: 2,
                ..Default::default()
            },
        );
        let st: ProxyIn =
            serde_json::from_str(&from_speaker.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(
            (st.method.as_str(), &st.params["queued"]),
            ("speech.state", &json!(2))
        );
        // The proxy lost the hub, then has it again: attach anew.
        put(&mut tw, json!({"ws": "default", "down": true})).await;
        assert!(matches!(
            events.recv().await,
            Some(LinkEvent::Detached { .. })
        ));
        put(&mut tw, json!({"ws": "default", "up": true})).await;
        let third: ProxyIn =
            serde_json::from_str(&from_speaker.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(third.method, "speech.attach");
        // The proxy's output ends: so does the session, detaching what was attached.
        tw.shutdown().await.unwrap();
        assert!(session.await.unwrap(), "it said hello");
    }

    /// The session pings the proxy every [`PING_EVERY`] with a line for the proxy itself (no
    /// workspace, id 0), from the start and whatever else goes on.
    #[tokio::test(start_paused = true)]
    async fn a_proxy_session_pings_the_proxy() {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (r, w) = tokio::io::split(ours);
        let (tr, mut tw) = tokio::io::split(theirs);
        let mut from_speaker = BufReader::new(tr).lines();
        let (tx, _events) = mpsc::channel(64);
        let board = Arc::new(Board::new(Arc::new(|_: &str, _: Persist<'_>| {})));
        let session =
            tokio::spawn(async move { proxy_session(r, w, &params(), &tx, &board).await });
        put(
            &mut tw,
            json!({"proxy": 1, "host": "box", "workspaces": []}),
        )
        .await;
        let start = tokio::time::Instant::now();
        for n in 1..=3u32 {
            let line = from_speaker.next_line().await.unwrap().unwrap();
            let ping: ProxyIn = serde_json::from_str(&line).unwrap();
            assert_eq!(
                (ping.ws.as_str(), ping.id, ping.method.as_str()),
                ("", 0, PROXY_PING),
                "{line}"
            );
            assert!(ping.params.is_null(), "{line}");
            assert_eq!(start.elapsed().as_secs(), (PING_EVERY * n).as_secs());
        }
        // An old proxy answers it as a workspace it doesn't serve: ignored.
        put(
            &mut tw,
            json!({"ws": "", "msg": {"id": 0, "error": {"code": "not_found", "message": "x"}}}),
        )
        .await;
        tw.shutdown().await.unwrap();
        assert!(session.await.unwrap());
    }

    #[test]
    fn hub_words_are_clean_and_short() {
        assert_eq!(hub_words("a\u{1b}[2Jb\nc"), "a[2Jb c");
        let long = hub_words(&"x".repeat(1000));
        assert_eq!(long.chars().count(), HUB_WORDS_MAX + 1);
        assert!(long.ends_with('…'));
        assert_eq!(hub_words("short"), "short");
    }

    /// The next line the fake hub got, as JSON.
    async fn next<R: AsyncRead + Unpin>(lines: &mut tokio::io::Lines<BufReader<R>>) -> Value {
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
    }

    /// A local link against a fake hub on a Unix socket: hello as `speak` with the protocol,
    /// attach with the speaker's params, items in, state out, and a detach when the hub goes.
    /// (Real time: the connection is real I/O, which paused time would race with timeouts.)
    #[tokio::test]
    async fn a_local_link_greets_attaches_passes_items_and_reports_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(dir.path());
        let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
        let mut link = Link::start(
            LinkSpec::Local {
                paths: paths.clone(),
                workspaces: Workspaces::Current,
            },
            params(),
        );
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        let hello = next(&mut lines).await;
        assert_eq!(hello["method"], "hello");
        assert_eq!(hello["params"]["client"], "speak");
        assert_eq!(
            hello["params"]["protocol"],
            twr_core::proto::PROTOCOL_VERSION
        );
        let answer = json!({"id": hello["id"], "result": {"client": "speak", "identity": "user", "workspace": "default"}});
        put(&mut w, answer).await;
        let attach = next(&mut lines).await;
        assert_eq!(attach["method"], "speech.attach");
        assert_eq!(attach["params"]["name"], "speaker of default");
        put(
            &mut w,
            json!({"id": attach["id"], "result": {"orchestrator": "T", "head": 2}}),
        )
        .await;
        match link.next().await {
            Some(LinkEvent::Attached { ws, ack }) => {
                assert_eq!((ws.as_str(), ack.head), ("default", 2));
            }
            e => panic!("{e:?}"),
        }
        put(
            &mut w,
            json!({"stream": "speech", "item": {"item": "ctl", "op": "skip"}}),
        )
        .await;
        assert!(matches!(
            link.next().await,
            Some(LinkEvent::Item {
                item: SpeechItem::Ctl {
                    op: CtlOp::Skip,
                    ..
                },
                ..
            })
        ));
        link.report(
            "default",
            SpeechStateParams {
                queued: 3,
                ..Default::default()
            },
        );
        let state = next(&mut lines).await;
        assert_eq!(state["method"], "speech.state");
        assert_eq!(state["params"]["queued"], 3);
        drop(w);
        drop(lines);
        assert!(matches!(
            link.next().await,
            Some(LinkEvent::Detached { .. })
        ));
    }

    /// Real time: a real process runs, and paused time would race past the backoff's reset.
    #[tokio::test]
    async fn an_ssh_link_reconnects_with_batch_mode_and_backs_off() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("ssh");
        let log = dir.path().join("args");
        // A fake ssh: records its arguments, says hello as the proxy, then hangs up.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\necho \"$*\" >> {}\necho '{{\"proxy\": 1, \"host\": \"h\", \"workspaces\": []}}'\n",
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spec = SshSpec::new("h", Workspaces::All)
            .unwrap()
            .with_program(&fake);
        let mut link = Link::start(LinkSpec::Ssh(spec), params());
        let mut waits = Vec::new();
        while waits.len() < 2 {
            match link.next().await {
                Some(LinkEvent::Note(n)) if n.contains("trying again in") => waits.push(n),
                Some(_) => {}
                None => panic!("ended"),
            }
        }
        assert!(waits[0].ends_with("in 1 s"), "{waits:?}");
        assert!(waits[1].ends_with("in 2 s"), "{waits:?}");
        drop(link);
        let calls = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert!(!calls[0].contains("BatchMode"), "{calls:?}");
        assert!(calls[1].contains("-o BatchMode=yes h --"), "{calls:?}");
    }
}

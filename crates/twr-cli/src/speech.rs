//! `twr speech feed` (docs/read-aloud.md): the hub machine's end of reading aloud.
//!
//! - **`--proto`** (the default): the line protocol `twr speak --ssh` reads over its ssh
//!   connection. Its first line says the proxy's version, the host and the workspaces
//!   ([`ProxyOut::Hello`]); then, per workspace, `up` once its hub is connected (again),
//!   every line the hub sends (`{"ws", "msg"}`), and `down` when it is not running or went
//!   away (retried every [`RETRY`]). On stdin it takes requests for a workspace's hub
//!   ([`ProxyIn`]), only `speech.attach` and `speech.state` ([`PROXY_METHODS`]): anything else
//!   is answered with an error line and never reaches the hub, so the proxy cannot drive the
//!   hub. A ping (`twr_speech::link::PROXY_PING`) it keeps to itself, unanswered. It never
//!   starts a hub, and it ends when its stdin closes, or, once pinged, when nothing came on
//!   stdin for `twr_speech::link::PROXY_IDLE` (the speaker is gone, its sshd session not yet).
//! - **`--text`**: a speaker of its own that prints each message's sentences, one per line, as
//!   they are said: a public interface for a reader of your own. With several workspaces each
//!   line starts with `<workspace>: `. Live mode is always on; controls are ignored; messages
//!   asked for with `/read N` are printed too.
//!
//! And the listening end: `twr speak` ([`speak`]), a speaker on this machine
//! (`twr_speech::speaker::run`), and `twr remote` ([`remote`]), a mosh or ssh session with
//! `twr speak --ssh` beside it.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;
use twr_core::client::{Client, SpeechLink};
use twr_core::config::{Config, SpeechEngine};
use twr_core::paths::{Paths, DEFAULT_WORKSPACE};
use twr_core::proto::codes;
use twr_core::speech::{
    utterances, ProxyIn, ProxyOut, SpeakOpts, SpeechAttachParams, SpeechItem, TurnsRead,
    PROXY_METHODS, PROXY_VERSION,
};
use twr_speech::link::PROXY_PING;
use twr_speech::{LinkSpec, Reporter, SpeakerOpts, SshSpec, StateStore, Workspaces};

/// How long the proxy waits before it tries a hub that is not running again.
pub const RETRY: Duration = Duration::from_secs(5);

/// Lines of the proxy's output waiting for a stdout that no longer takes them, at most: past
/// that its speaker is taken to be gone (`proxy`).
const OUT_WAITING: usize = 4096;

/// The most bytes of one request line the proxy reads from stdin.
const LINE_MAX: usize = 1 << 20;

/// A test hook: how long the proxy, once pinged, waits for a line on stdin before it ends, in
/// milliseconds (else `twr_speech::link::PROXY_IDLE`).
const IDLE_ENV: &str = "TWR_SPEECH_FEED_IDLE_MS";

/// How long the proxy, once pinged, waits for a line on stdin: [`IDLE_ENV`]'s, else
/// `twr_speech::link::PROXY_IDLE`.
pub fn proxy_idle() -> Duration {
    std::env::var(IDLE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(twr_speech::link::PROXY_IDLE)
}

/// The workspaces the feed serves, each with its hub's socket: `names`, or every workspace
/// of the root with `all` (the default one first), or the current one.
pub fn workspace_sockets(
    paths: &Paths,
    names: &[String],
    all: bool,
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let root = paths.root();
    let mut wanted: Vec<String> = if all {
        let mut v = vec![DEFAULT_WORKSPACE.to_string()];
        v.extend(root.named_workspaces().context("listing the workspaces")?);
        v
    } else if names.is_empty() {
        vec![paths.workspace_name().to_string()]
    } else {
        names.to_vec()
    };
    wanted.dedup();
    let config = Config::load_from(&root).unwrap_or_default();
    let mut out = Vec::new();
    for name in wanted {
        if name != DEFAULT_WORKSPACE {
            if let Some(e) = twr_core::paths::workspace_name_error(&name) {
                anyhow::bail!("{e}");
            }
        }
        let ws = root.workspace(&name)?;
        if !ws.exists() {
            anyhow::bail!("there is no workspace {name} (`twr ws` lists them)");
        }
        let socket = if name == DEFAULT_WORKSPACE {
            config.socket_path(&ws)
        } else {
            ws.socket.clone()
        };
        out.push((name, socket));
    }
    Ok(out)
}

/// This machine's name, for the proxy's first line and a speaker's default name.
pub fn host_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is writable for its whole length; gethostname writes at most that many
    // bytes and we read up to the first NUL only.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    let name = if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    } else {
        String::new()
    };
    let name: String = twr_core::speech::clean(&name)
        .trim()
        .chars()
        .take(64)
        .collect();
    if name.is_empty() {
        "this machine".into()
    } else {
        name
    }
}

/// Connects to a hub as the proxy or the text feed: its own socket check, then `hello`.
async fn hello(socket: &PathBuf, client: &str) -> anyhow::Result<Client> {
    let mut c = Client::connect(socket).await?;
    c.hello(json!({ "client": client })).await?;
    Ok(c)
}

/// Writes one line to stdout and flushes it; `false` once stdout is gone.
fn print_line(line: &str) -> bool {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}").and_then(|()| out.flush()).is_ok()
}

/// `twr speech feed --proto`.
///
/// Every read is a task of its own that only reads, so none is ever cut off halfway through a
/// line by another branch of a `select!` (a read cut off would lose the part of the line it
/// had taken): stdin's lines come over one channel, each hub's over another. The loop below
/// is the only writer of stdout, and it writes its own refusals directly, so it never waits
/// on a channel only it reads.
///
/// A speaker that pings ([`PROXY_PING`]) arms a deadline of `idle`, moved by every line read:
/// one that pings no more (gone with its laptop, the sshd session still up) ends the proxy, and
/// its hubs let the device go. One older than pings never arms it.
pub async fn proxy(paths: &Paths, all: bool, idle: Duration) -> anyhow::Result<()> {
    let workspaces = workspace_sockets(paths, &[], all)?;
    let (out_tx, mut out_rx) = mpsc::channel::<ProxyOut>(1024);
    let hello = ProxyOut::Hello {
        proxy: PROXY_VERSION,
        host: host_name(),
        workspaces: workspaces.iter().map(|(w, _)| w.clone()).collect(),
    };
    if !print_line(&serde_json::to_string(&hello)?) {
        return Ok(());
    }
    let mut senders: BTreeMap<String, mpsc::Sender<Value>> = BTreeMap::new();
    let mut tasks = tokio::task::JoinSet::new();
    for (ws, socket) in workspaces {
        let (tx, rx) = mpsc::channel::<Value>(64);
        senders.insert(ws.clone(), tx);
        tasks.spawn(serve_workspace(ws, socket, rx, out_tx.clone()));
    }
    // Kept for workspaces found later; the output channel then never closes on its own, and
    // the loop ends with stdin.
    let out_tx_keep = out_tx;
    let (in_tx, mut in_rx) = mpsc::channel::<anyhow::Result<Option<Vec<u8>>>>(64);
    let stdin_task = tokio::spawn(async move {
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            let mut line = Vec::new();
            let r = read_bounded(&mut stdin, &mut line).await;
            let end = !matches!(r, Ok(Some(_)));
            let item = r.map(|r| r.map(|whole| if whole { line } else { Vec::new() }));
            if in_tx.send(item).await.is_err() || end {
                return;
            }
        }
    });
    // stdout is written by a thread of its own: a speaker that stops reading (an ssh
    // connection that no longer drains) fills the pipe and blocks that thread, never this
    // loop, which still sees the end of stdin and the idle limit. When even the lines waiting
    // for it pile up past [`OUT_WAITING`], the speaker is gone: the proxy ends, and its hubs
    // let the speaker go.
    let (lines_tx, lines_rx) = std::sync::mpsc::sync_channel::<String>(OUT_WAITING);
    // Said by the output thread when stdout is gone: the loop ends then too.
    let (out_gone_tx, mut out_gone) = tokio::sync::oneshot::channel::<()>();
    std::thread::Builder::new()
        .name("speech-feed-out".into())
        .spawn(move || {
            while let Ok(line) = lines_rx.recv() {
                if !print_line(&line) {
                    break;
                }
            }
            let _ = out_gone_tx.send(());
        })
        .context("starting the proxy's output")?;
    let write = |out: &ProxyOut| -> anyhow::Result<bool> {
        Ok(lines_tx.try_send(serde_json::to_string(out)?).is_ok())
    };
    let refusal = |ws: &str, id: u64, code: &str, message: &str| ProxyOut::Line {
        ws: ws.to_string(),
        msg: json!({"id": id, "error": {"code": code, "message": message}}),
    };
    // With every workspace, one made later (`twr ws new`) is served from then on: its `up`
    // tells the speaker to attach to it.
    let mut rediscover = tokio::time::interval(RETRY);
    rediscover.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let out_new = out_tx_keep;
    let mut pinged = false;
    let quiet = tokio::time::sleep(idle);
    tokio::pin!(quiet);
    loop {
        tokio::select! {
            () = &mut quiet, if pinged => {
                tracing::info!("no line from the speaker for {} s: it is gone", idle.as_secs());
                break;
            }
            // stdout is gone (a write failed): nobody reads what the hubs send.
            _ = &mut out_gone => break,
            _ = rediscover.tick(), if all => {
                // A listing that fails now is tried again at the next tick.
                if let Ok(found) = workspace_sockets(paths, &[], true) {
                    for (ws, socket) in &found {
                        if !senders.contains_key(ws) {
                            let (tx, rx) = mpsc::channel::<Value>(64);
                            senders.insert(ws.clone(), tx);
                            tasks.spawn(serve_workspace(ws.clone(), socket.clone(), rx, out_new.clone()));
                        }
                    }
                    // A workspace removed since: closing its requests ends its task.
                    let gone: Vec<String> = senders
                        .keys()
                        .filter(|w| !found.iter().any(|(f, _)| f == *w))
                        .cloned()
                        .collect();
                    for ws in gone {
                        senders.remove(&ws);
                        if !write(&ProxyOut::Down { ws, down: true })? {
                            break;
                        }
                    }
                }
            }
            out = out_rx.recv() => {
                let Some(out) = out else { break };
                if !write(&out)? {
                    break;
                }
            }
            line = in_rx.recv() => {
                // The end of stdin (or a failure to read it): the speaker is gone.
                let Some(Ok(Some(line))) = line else { break };
                quiet.as_mut().reset(tokio::time::Instant::now() + idle);
                let answer = if line.is_empty() {
                    Some(refusal("", 0, codes::BAD_REQUEST, "a request line is too long or empty"))
                } else {
                    match serde_json::from_slice::<ProxyIn>(&line) {
                        Err(e) => Some(refusal("", 0, codes::BAD_REQUEST, &format!("not a request: {e}"))),
                        // The speaker is there: kept to the proxy, never answered.
                        Ok(req) if req.method == PROXY_PING => {
                            pinged = true;
                            None
                        }
                        Ok(req) => match senders.get(&req.ws) {
                            None => Some(refusal(&req.ws, req.id, codes::NOT_FOUND, "this proxy does not serve that workspace")),
                            Some(_) if !PROXY_METHODS.contains(&req.method.as_str()) => Some(refusal(
                                &req.ws,
                                req.id,
                                codes::UNAUTHORIZED,
                                "the speech feed passes on speech.attach and speech.state only",
                            )),
                            Some(tx) => {
                                let request = json!({"id": req.id, "method": req.method, "params": req.params});
                                tx.try_send(request).err().map(|_| {
                                    refusal(&req.ws, req.id, codes::CONFLICT, "the workspace's hub is not connected")
                                })
                            }
                        },
                    }
                };
                if let Some(answer) = answer {
                    if !write(&answer)? {
                        break;
                    }
                }
            }
        }
    }
    stdin_task.abort();
    tasks.abort_all();
    Ok(())
}

/// Reads one line of at most [`LINE_MAX`] bytes into `line` (without its newline):
/// `Ok(None)` at the end of stdin, `Ok(Some(false))` for a line too long (skipped whole).
async fn read_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    line: &mut Vec<u8>,
) -> anyhow::Result<Option<bool>> {
    use tokio::io::AsyncReadExt;
    line.clear();
    let n = (&mut *r)
        .take(LINE_MAX as u64 + 1)
        .read_until(b'\n', line)
        .await
        .context("reading stdin")?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() == Some(&b'\n') {
        line.pop();
        return Ok(Some(true));
    }
    if line.len() <= LINE_MAX {
        // The last line, without a newline.
        return Ok(Some(true));
    }
    // Too long: skip to its end.
    let mut rest = Vec::new();
    loop {
        rest.clear();
        let n = (&mut *r)
            .take(64 * 1024)
            .read_until(b'\n', &mut rest)
            .await
            .context("reading stdin")?;
        if n == 0 || rest.last() == Some(&b'\n') {
            return Ok(Some(false));
        }
    }
}

/// One workspace of the proxy: connects to its hub (again, every [`RETRY`] while it is not
/// running), says `up`, passes the requests for it on and its lines back, says `down` when
/// it goes away. Requests that come while it is down are answered there. The hub's lines are
/// read by a task of its own (a read is never cut off by a request coming in).
async fn serve_workspace(
    ws: String,
    socket: PathBuf,
    mut requests: mpsc::Receiver<Value>,
    out: mpsc::Sender<ProxyOut>,
) {
    let refuse = |id: u64, message: &str| ProxyOut::Line {
        ws: ws.clone(),
        msg: json!({"id": id, "error": {"code": codes::CONFLICT, "message": message}}),
    };
    let mut said_down = false;
    loop {
        let client = match hello(&socket, "speak-proxy").await {
            Ok(c) => c,
            Err(e) => {
                if !said_down {
                    tracing::debug!("{ws}: {e:#}");
                    said_down = true;
                    if out
                        .send(ProxyOut::Down {
                            ws: ws.clone(),
                            down: true,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                // Requests meanwhile are refused: the hub would not see them.
                let deadline = tokio::time::sleep(RETRY);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        () = &mut deadline => break,
                        r = requests.recv() => match r {
                            Some(req) => {
                                let id = req["id"].as_u64().unwrap_or(0);
                                if out.send(refuse(id, "the workspace's hub is not running")).await.is_err() {
                                    return;
                                }
                            }
                            None => return,
                        },
                    }
                }
                continue;
            }
        };
        // A request still waiting was made for the connection before this one (the speaker
        // attaches after each `up`): passed on here it would attach a speaker the speaker no
        // longer waits for. Refused, before this connection's `up`.
        while let Ok(req) = requests.try_recv() {
            let id = req["id"].as_u64().unwrap_or(0);
            if out
                .send(refuse(id, "the workspace's hub reconnected meanwhile"))
                .await
                .is_err()
            {
                return;
            }
        }
        if out
            .send(ProxyOut::Up {
                ws: ws.clone(),
                up: true,
            })
            .await
            .is_err()
        {
            return;
        }
        let (mut reader, mut writer) = client.into_raw();
        let lines_out = out.clone();
        let lines_ws = ws.clone();
        let mut lines = tokio::spawn(async move {
            while let Ok(Some(msg)) = reader.next().await {
                let line = ProxyOut::Line {
                    ws: lines_ws.clone(),
                    msg,
                };
                if lines_out.send(line).await.is_err() {
                    return false;
                }
            }
            true
        });
        let gone = loop {
            tokio::select! {
                ended = &mut lines => break !ended.unwrap_or(false),
                r = requests.recv() => match r {
                    Some(req) => {
                        if writer.send(&req).await.is_err() {
                            break false;
                        }
                    }
                    None => break true,
                },
            }
        };
        lines.abort();
        if gone
            || out
                .send(ProxyOut::Down {
                    ws: ws.clone(),
                    down: true,
                })
                .await
                .is_err()
        {
            return;
        }
        said_down = true;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `twr speech feed --text`.
pub async fn text(workspaces: Vec<(String, PathBuf)>) -> anyhow::Result<()> {
    let prefix = workspaces.len() > 1;
    let mut random = [0u8; 8];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
        .map_err(|_| anyhow::anyhow!("no randomness for the feed's device id"))?;
    let device = format!("feed-{}", hex::encode(random));
    let (tx, mut rx) = mpsc::channel::<String>(1024);
    let mut tasks = tokio::task::JoinSet::new();
    for (ws, socket) in workspaces {
        let label = prefix.then(|| format!("{ws}: "));
        tasks.spawn(text_workspace(socket, device.clone(), label, tx.clone()));
    }
    drop(tx);
    while let Some(line) = rx.recv().await {
        if !print_line(&line) {
            break;
        }
    }
    tasks.abort_all();
    Ok(())
}

/// One workspace of the text feed: attached as a speaker with live mode on, again after its
/// hub went away, printing each message's sentences. Within one run it reads no message
/// twice: it keeps its place per thread across reconnects.
async fn text_workspace(
    socket: PathBuf,
    device: String,
    label: Option<String>,
    out: mpsc::Sender<String>,
) {
    let mut watermarks: BTreeMap<String, u64> = BTreeMap::new();
    let opts = SpeakOpts::default();
    loop {
        let params = SpeechAttachParams {
            device: device.clone(),
            name: "speech feed".into(),
            engine: "text".into(),
            voice: None,
            live: true,
            read: twr_core::speech::default_read(),
            turn_from: TurnsRead::All,
            watermarks: watermarks.clone(),
            catchup_secs: twr_core::speech::default_catchup(),
        };
        let attached = async {
            let c = hello(&socket, "speech-feed").await?;
            SpeechLink::attach(c, &params).await
        }
        .await;
        let (ack, mut link) = match attached {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!("{}: {e:#}", socket.display());
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        if let Some(orch) = ack.orchestrator {
            watermarks.retain(|t, _| *t == orch);
            watermarks.entry(orch).or_insert(ack.head);
        }
        while let Some(item) = link.next().await {
            let Ok(item) = item else { break };
            match item {
                SpeechItem::Msg {
                    thread,
                    seq,
                    mut text,
                    origin,
                    ..
                } => {
                    // No more than a hub sends, whatever this one sent.
                    if text.len() > twr_core::speech::TEXT_MAX {
                        let mut end = twr_core::speech::TEXT_MAX;
                        while !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        text.truncate(end);
                    }
                    if origin != twr_core::speech::Origin::Request {
                        let w = watermarks.entry(thread).or_insert(0);
                        if seq <= *w {
                            continue;
                        }
                        *w = seq;
                    }
                    for line in utterances(&text, &opts) {
                        let line = match &label {
                            Some(l) => format!("{l}{line}"),
                            None => line,
                        };
                        if out.send(line).await.is_err() {
                            return;
                        }
                    }
                }
                SpeechItem::Missed { thread, upto, .. } => {
                    let w = watermarks.entry(thread).or_insert(0);
                    *w = (*w).max(upto);
                }
                SpeechItem::Orchestrator { thread } => {
                    // The feed follows the current orchestrator only: the place in a retired
                    // one is no use any more (and the hub takes at most 512).
                    watermarks.retain(|t, _| *t == thread);
                    watermarks.entry(thread).or_insert(0);
                }
                SpeechItem::Ctl { .. } => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// ---- twr speak ----

/// `twr speak`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct Speak {
    /// `--ssh HOST`.
    pub ssh: Option<String>,
    /// The global `-w`, as given (not `$TWR_WORKSPACE`'s).
    pub workspace: Option<String>,
    /// `--all-workspaces`.
    pub all_workspaces: bool,
    /// `--name`.
    pub name: Option<String>,
    /// `--engine`: say, avspeech, spd or command.
    pub engine: Option<String>,
    /// `--voice`.
    pub voice: Option<String>,
    /// `--rate` (80 to 500, checked by clap).
    pub rate: Option<u32>,
    /// `--print-service`.
    pub print_service: bool,
}

/// What a speaker reads: this machine's hubs of `paths`'s root, or a remote one's over ssh.
fn speak_link(args: &Speak, paths: &Paths) -> anyhow::Result<LinkSpec> {
    if args.all_workspaces && args.workspace.is_some() {
        anyhow::bail!("-w names one workspace and --all-workspaces every one: give one of them");
    }
    Ok(match &args.ssh {
        Some(host) => {
            if let Some(e) = twr_speech::link::ssh_host_error(host) {
                anyhow::bail!("{e}");
            }
            // Over ssh every workspace of the other machine unless `-w` names one: the remote
            // TUI may be in any of them.
            let workspaces = match &args.workspace {
                Some(ws) => Workspaces::One(ws.clone()),
                None => Workspaces::All,
            };
            // `TWR_SSH` names the program, as for the hub's ssh boxes.
            LinkSpec::Ssh(
                SshSpec::new(host, workspaces)?.with_program(twr_hub::ssh_box::ssh_program()),
            )
        }
        // Every workspace of this machine unless `-w` names one, as over ssh and as the TUI's
        // own speaker: live reading is per workspace anyway, and a speaker kept to one would
        // keep this device's other speakers from the rest (one speaks for a machine's hubs).
        None => match &args.workspace {
            Some(ws) => {
                let one = paths.root().workspace(ws)?;
                if !one.exists() {
                    anyhow::bail!(
                        "there is no workspace {ws}: create it with `twr ws new {ws}` (`twr ws` \
                         lists them)"
                    );
                }
                LinkSpec::Local {
                    paths: paths.root(),
                    workspaces: Workspaces::One(ws.clone()),
                }
            }
            None => LinkSpec::Local {
                paths: paths.root(),
                workspaces: Workspaces::All,
            },
        },
    })
}

/// What the speaker listens to, for its start note.
fn listening_to(args: &Speak) -> String {
    let machine = args.ssh.as_deref().unwrap_or("this machine");
    match (&args.workspace, &args.ssh) {
        _ if args.all_workspaces => format!("every workspace of {machine}"),
        (Some(ws), _) => format!("workspace {ws} of {machine}"),
        (None, _) => format!("every workspace of {machine}"),
    }
}

/// `--engine`'s value as `[speech] engine` takes it.
fn parse_engine(name: &str) -> anyhow::Result<SpeechEngine> {
    serde_json::from_value(Value::String(name.to_string()))
        .map_err(|_| anyhow::anyhow!("--engine {name:?}: say, avspeech, spd or command"))
}

/// `twr speak`: a speaker on this machine until `q`, SIGINT, SIGTERM or SIGHUP; with
/// `--print-service`, the service that runs it.
pub async fn speak(args: Speak, paths: &Paths) -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};
    let link = speak_link(&args, paths)?;
    // As the speaker checks them (`twr_speech::run`), before a service is printed with them:
    // a hub refuses the attach of a voice it can't take, every time.
    if let Some(e) = args
        .name
        .as_deref()
        .and_then(|n| twr_core::speech::name_error("--name", n, 64))
    {
        anyhow::bail!("{e}");
    }
    if let Some(e) = args
        .voice
        .as_deref()
        .and_then(|v| twr_core::speech::name_error("--voice", v, twr_speech::speaker::VOICE_MAX))
    {
        anyhow::bail!("{e}");
    }
    let engine_override = args.engine.as_deref().map(parse_engine).transpose()?;
    if args.print_service {
        return print_service(&args);
    }
    let root = paths.root();
    let config = Config::load_from(&root)?;
    let engine = twr_speech::from_config(&config.speech, engine_override)?;
    let (mut sigint, mut sigterm, mut sighup) = (
        signal(SignalKind::interrupt()).context("handling SIGINT")?,
        signal(SignalKind::terminate()).context("handling SIGTERM")?,
        signal(SignalKind::hangup()).context("handling SIGHUP")?,
    );
    let shutdown = async move {
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
            _ = sighup.recv() => {}
        }
    };
    // Keys are read once a hub is attached: before that, ssh may ask for a passphrase or about
    // the host key on this terminal. Raw mode while they are: lines end with "\r\n" then.
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let report: Reporter = Arc::new(|note: &str| note_line(twr_speech::raw_mode(), note));
    let start = format!(
        "reading aloud what {} says, with {}",
        listening_to(&args),
        engine.name()
    );
    report(&start);
    let opts = SpeakerOpts {
        name: args.name.clone().unwrap_or_else(host_name),
        config: config.speech.clone(),
        voice: args.voice.clone(),
        rate: args.rate,
        state: StateStore::in_data_dir(&root.data_dir),
        report: Some(report),
        tty_keys_once_attached: tty,
    };
    // `run` restores the terminal before it returns, so an error prints on a normal one.
    twr_speech::run(opts, link, engine, None, shutdown).await
}

/// One of the speaker's notes on stderr: `\r\n`-ended in raw mode; with the time in front
/// when stderr is no terminal (a log: `speak.log`, a service's).
fn note_line(raw: bool, note: &str) {
    use std::io::IsTerminal;
    let mut err = std::io::stderr().lock();
    let note = crate::term::clean(note);
    let end = if raw { "\r\n" } else { "\n" };
    let _ = if err.is_terminal() {
        write!(err, "twr speak: {note}{end}")
    } else {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        write!(err, "{now} twr speak: {note}{end}")
    };
}

/// The heredoc delimiters of the service's snippet.
const UNIT_END: &str = "TWR_UNIT";
const PLIST_END: &str = "TWR_PLIST";

/// `twr speak`'s own arguments, as a service runs it: these, without `--print-service`.
fn service_args(args: &Speak) -> Vec<String> {
    let mut a = vec!["speak".to_string()];
    if let Some(host) = &args.ssh {
        a.extend(["--ssh".to_string(), host.clone()]);
    }
    if args.all_workspaces {
        a.push("--all-workspaces".into());
    } else if let Some(ws) = &args.workspace {
        a.extend(["-w".to_string(), ws.clone()]);
    }
    let options = [
        ("--name", args.name.clone()),
        ("--engine", args.engine.clone()),
        ("--voice", args.voice.clone()),
        ("--rate", args.rate.map(|r| r.to_string())),
    ];
    for (flag, value) in options {
        if let Some(v) = value {
            a.extend([flag.to_string(), v]);
        }
    }
    a
}

/// `twr speak --print-service`: the LaunchAgent (macOS) or systemd user unit (Linux) that runs
/// this `twr speak`, and the commands that install and start it, to paste. Installs nothing.
fn print_service(args: &Speak) -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .context("finding this twr's path")?
        .to_str()
        .filter(|p| p.starts_with('/'))
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!("this twr's path is not plain text: write the service yourself")
        })?;
    let words = service_args(args);
    let text = match std::env::consts::OS {
        "macos" => {
            let home = std::env::var("HOME")
                .ok()
                .filter(|h| h.starts_with('/'))
                .ok_or_else(|| anyhow::anyhow!("$HOME is not an absolute path"))?;
            let plist = twr_speech::service::launchd_plist(&exe, &words, &home)?;
            anyhow::ensure!(
                crate::dial_in::heredoc(&plist, PLIST_END),
                "the LaunchAgent can't be pasted: write it yourself"
            );
            let label = twr_speech::service::AGENT_LABEL;
            format!(
                "mkdir -p ~/Library/LaunchAgents ~/Library/Logs\n\
                 cat > ~/Library/LaunchAgents/{label}.plist <<'{PLIST_END}'\n\
                 {plist}{PLIST_END}\n\
                 # Pasted again: the speaker the last paste started stops first; none loaded is\n\
                 # no error.\n\
                 launchctl bootout gui/$(id -u)/{label} 2>/dev/null || true\n\
                 launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/{label}.plist\n"
            )
        }
        "linux" => {
            let unit = twr_speech::service::systemd_unit(&exe, &words)?;
            anyhow::ensure!(
                crate::dial_in::heredoc(&unit, UNIT_END),
                "the unit can't be pasted: write it yourself"
            );
            let config_home = std::env::var("XDG_CONFIG_HOME").ok();
            let dir = crate::dial_in::unit_dir(config_home.as_deref().filter(|c| !c.is_empty()));
            let name = twr_speech::service::UNIT_NAME;
            format!(
                "mkdir -p {dir}\n\
                 cat > {dir}/{name} <<'{UNIT_END}'\n\
                 {unit}{UNIT_END}\n\
                 systemctl --user daemon-reload\n\
                 systemctl --user enable {name}\n\
                 # Started, or restarted with these settings when pasted again.\n\
                 systemctl --user restart {name}\n"
            )
        }
        os => anyhow::bail!("twr speak --print-service writes a LaunchAgent (macOS) or a systemd unit (Linux), not a service for {os}"),
    };
    if std::env::var_os("TWR_HOME").is_some() {
        outln!(
            "# TWR_HOME is set here and the service runs without it: add it to the service to \
             use the same twr home."
        );
    }
    out!("{text}");
    Ok(())
}

// ---- twr remote ----

/// `twr remote`'s arguments.
#[derive(Debug, Clone)]
pub struct Remote {
    /// The host.
    pub host: String,
    /// `--ssh`: the session over `ssh -t`, not mosh.
    pub ssh: bool,
    /// `--name`, for the speaker.
    pub name: Option<String>,
    /// The global `-w`, as given: the speaker reads that workspace of the host only.
    pub workspace: Option<String>,
    /// The remote twr's arguments.
    pub args: Vec<String>,
}

/// The speaker's log, in the root's data directory.
pub const SPEAK_LOG: &str = "speak.log";

/// A test hook: the mosh program `twr remote` runs, when set and not empty (else `mosh` on
/// `PATH`). The tests run a recorder script instead of mosh; `TWR_SSH` does the same for ssh,
/// for the session and the speaker, as it does for the hub's ssh boxes.
const MOSH_ENV: &str = "TWR_MOSH";

/// How long the speaker gets to stop after SIGTERM before it is killed.
const SPEAKER_GRACE: Duration = Duration::from_secs(2);

/// How long the session gets to exit after a signal was passed on to it before it is killed.
const SESSION_GRACE: Duration = Duration::from_secs(10);

/// The session's program and arguments: `mosh HOST -- twr ARGS...` (mosh hands its command to
/// the remote as words: the arguments as they are), or `ssh -t HOST -- twr ARGS...` (ssh joins
/// its command into one line for the remote shell: each argument as a shell word).
fn session_command(
    program: OsString,
    ssh: bool,
    host: &str,
    args: &[String],
) -> (OsString, Vec<String>) {
    let mut a = Vec::new();
    if ssh {
        a.push("-t".to_string());
    }
    a.extend([host.to_string(), "--".to_string(), "twr".to_string()]);
    if ssh {
        a.extend(args.iter().map(|w| twr_core::paths::sh_word(w)));
    } else {
        a.extend(args.iter().cloned());
    }
    (program, a)
}

/// The mosh program: `TWR_MOSH` (a test hook), else `mosh`, found on `PATH`.
fn mosh_program() -> anyhow::Result<OsString> {
    let name = std::env::var(MOSH_ENV)
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "mosh".into());
    twr_speech::engine::which(&name, std::env::var_os("PATH").as_deref())
        .map(PathBuf::into_os_string)
        .ok_or_else(|| anyhow::anyhow!("mosh not found; use --ssh"))
}

/// Why this machine's `[speech]` gives the speaker no engine (it would stop at once), if so.
fn no_engine(root: &Paths) -> Option<String> {
    let config = match Config::load_from(root) {
        Ok(c) => c,
        Err(e) => return Some(format!("{e:#}")),
    };
    let path = std::env::var_os("PATH");
    twr_speech::engine::resolve(
        config.speech.engine,
        &config.speech.command,
        std::env::consts::OS,
        path.as_deref(),
    )
    .err()
    .map(|e| format!("{e:#}"))
}

/// Starts `twr [-w WS] speak --ssh HOST [--name N]` in a session of its own (no terminal),
/// stdin empty, its output appended to `log`.
fn start_speaker(args: &Remote, log: &std::fs::File) -> anyhow::Result<tokio::process::Child> {
    let exe = std::env::current_exe().context("finding this twr's path")?;
    let mut cmd = tokio::process::Command::new(exe);
    if let Some(ws) = &args.workspace {
        cmd.args(["-w", ws]);
    }
    cmd.args(["speak", "--ssh", &args.host]);
    if let Some(name) = &args.name {
        cmd.args(["--name", name]);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    // SAFETY: the closure runs in the child between fork and exec and calls only setsid, which
    // is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("starting the speaker")
}

/// Ends the speaker: SIGTERM to its process group (it leads one, [`start_speaker`]), then
/// SIGKILL after [`SPEAKER_GRACE`]; reaped either way.
async fn stop_speaker(speaker: &mut Option<tokio::process::Child>) {
    let Some(mut child) = speaker.take() else {
        return;
    };
    // `None` once reaped: its id may name another process by now.
    let Some(pid) = child.id().and_then(|p| i32::try_from(p).ok()) else {
        return;
    };
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    // SAFETY: a negative pid signals that process group; the leader is our child, not reaped
    // yet, so the id still names its group.
    unsafe {
        libc::kill(-pid, libc::SIGTERM);
    }
    if tokio::time::timeout(SPEAKER_GRACE, child.wait())
        .await
        .is_err()
    {
        // SAFETY: as above; still not reaped.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let _ = child.wait().await;
    }
}

/// A line on stderr while the session may hold the terminal in raw mode: `\r\n`-ended.
fn remote_note(line: &str) {
    let _ = write!(
        std::io::stderr().lock(),
        "twr remote: {}\r\n",
        crate::term::clean(line)
    );
}

/// `twr remote`: the session in the foreground with the terminal, the speaker beside it; its
/// exit code is the session's.
pub async fn remote(args: Remote, paths: &Paths) -> anyhow::Result<std::process::ExitCode> {
    use std::os::unix::process::ExitStatusExt;
    use tokio::signal::unix::{signal, SignalKind};
    if twr_speech::link::ssh_host_error(&args.host).is_some() {
        anyhow::bail!(
            "twr remote {:?}: not a host (empty, starting with `-`, or holding spaces or \
             control characters)",
            args.host
        );
    }
    let program = if args.ssh {
        twr_hub::ssh_box::ssh_program()
    } else {
        mosh_program()?
    };
    let (program, session_args) = session_command(program, args.ssh, &args.host, &args.args);
    let root = paths.root();
    let log_path = root.data_dir.join(SPEAK_LOG);
    // Installed before anything starts: a signal right away must not leave the speaker behind.
    let mut sigint = signal(SignalKind::interrupt()).context("handling SIGINT")?;
    let mut sigterm = signal(SignalKind::terminate()).context("handling SIGTERM")?;
    let mut sighup = signal(SignalKind::hangup()).context("handling SIGHUP")?;
    let mut sigquit = signal(SignalKind::quit()).context("handling SIGQUIT")?;
    let mut speaker = match no_engine(&root) {
        Some(why) => {
            errln!("twr remote: not reading aloud: {why}");
            None
        }
        None => {
            let started = twr_core::paths::ensure_private_dir(&root.data_dir).and_then(|()| {
                let mut log = twr_core::paths::open_private_append(&log_path)
                    .with_context(|| format!("opening {}", log_path.display()))?;
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                writeln!(log, "{now} twr remote {}: starting the speaker", args.host)?;
                start_speaker(&args, &log)
            });
            match started {
                Ok(child) => Some(child),
                Err(e) => {
                    errln!("twr remote: not reading aloud: {e:#}");
                    None
                }
            }
        }
    };
    let mut session = match tokio::process::Command::new(&program)
        .args(&session_args)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            stop_speaker(&mut speaker).await;
            let shown = PathBuf::from(&program);
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::bail!("{} not found", shown.display());
            }
            anyhow::bail!("running {}: {e}", shown.display());
        }
    };
    let session_pid = session.id().and_then(|p| i32::try_from(p).ok());
    let mut passed: Vec<i32> = Vec::new();
    let mut deadline: Option<tokio::time::Instant> = None;
    // A failure to wait for the session ends this too, after the speaker is stopped below.
    let status = loop {
        let sig = tokio::select! {
            st = session.wait() => break st.context("waiting for the session"),
            st = async { speaker.as_mut().expect("guarded").wait().await }, if speaker.is_some() => {
                speaker = None;
                let how = match st {
                    Ok(st) => st.to_string(),
                    Err(e) => e.to_string(),
                };
                remote_note(&format!(
                    "the speaker stopped ({how}); see {}",
                    log_path.display()
                ));
                continue;
            }
            _ = sigint.recv() => libc::SIGINT,
            _ = sigterm.recv() => libc::SIGTERM,
            _ = sighup.recv() => libc::SIGHUP,
            _ = sigquit.recv() => libc::SIGQUIT,
            () = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                let _ = session.start_kill();
                deadline = None;
                continue;
            }
        };
        // Stopped now: the session may take a while to go. The signal goes on to the session
        // (once of each kind), which may have had it from the terminal already.
        stop_speaker(&mut speaker).await;
        if !passed.contains(&sig) {
            passed.push(sig);
            if let Some(pid) = session_pid {
                // SAFETY: the session is our child, not reaped yet (this loop reaps it).
                unsafe {
                    libc::kill(pid, sig);
                }
            }
        }
        deadline.get_or_insert(tokio::time::Instant::now() + SESSION_GRACE);
    };
    // On every way out: the speaker (setsid'd, its own session) would outlive this otherwise.
    stop_speaker(&mut speaker).await;
    let status = status?;
    let code = match (status.code(), status.signal()) {
        (Some(c), _) => c,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    };
    Ok(std::process::ExitCode::from(
        u8::try_from(code).unwrap_or(1),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// mosh passes its command on as words; ssh joins it for the remote shell, so each word
    /// is quoted.
    #[test]
    fn the_session_quotes_for_ssh_only() {
        let args = words(&["-w", "work", "it's; rm -rf ~"]);
        let (p, a) = session_command("mosh".into(), false, "me@box", &args);
        assert_eq!(p, "mosh");
        assert_eq!(
            a,
            words(&["me@box", "--", "twr", "-w", "work", "it's; rm -rf ~"])
        );
        let (p, a) = session_command("ssh".into(), true, "me@box", &args);
        assert_eq!(p, "ssh");
        assert_eq!(
            a,
            words(&[
                "-t",
                "me@box",
                "--",
                "twr",
                "-w",
                "work",
                r"'it'\''s; rm -rf ~'"
            ])
        );
    }

    /// A service runs the same `twr speak` without `--print-service`: every workspace unless
    /// `-w` named one (a `$TWR_WORKSPACE` doesn't narrow it).
    #[test]
    fn service_args_are_the_command_line() {
        let home = tempfile::tempdir().unwrap();
        let root = Paths::with_home(home.path());
        let speak = Speak {
            name: Some("my mac".into()),
            rate: Some(200),
            print_service: true,
            ..Speak::default()
        };
        assert_eq!(
            service_args(&speak),
            words(&["speak", "--name", "my mac", "--rate", "200"])
        );
        let one = Speak {
            workspace: Some("work".into()),
            ..Speak::default()
        };
        assert_eq!(service_args(&one), words(&["speak", "-w", "work"]));
        let ssh = Speak {
            ssh: Some("me@box".into()),
            engine: Some("spd".into()),
            voice: Some("en".into()),
            ..Speak::default()
        };
        assert_eq!(
            service_args(&ssh),
            words(&["speak", "--ssh", "me@box", "--engine", "spd", "--voice", "en"])
        );
        let all = Speak {
            all_workspaces: true,
            ..Speak::default()
        };
        assert_eq!(service_args(&all), words(&["speak", "--all-workspaces"]));
        let both = Speak {
            workspace: Some("work".into()),
            all_workspaces: true,
            ..Speak::default()
        };
        assert!(speak_link(&both, &root).is_err());
    }
}

//! The hub side of the relay protocol, and small client helpers (docs/relay-protocol.md §5,
//! §8, §10). The phone gateway uses [`HubClient`]; `twr-link` uses the admission and
//! `/v1/info` helpers; [`phone_connect`] is the reference for what a phone does.

use crate::mux::{self, MuxStream};
use crate::proto::{self, Incoming, Msg, PhoneCred, PhoneRoute, Welcome};
use crate::url::{Host, RelayUrl};
use crate::{admit, names, tls, IdentityKey};
use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::{CertificateDer, ServerName};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::client::TlsStream;
use tokio_util::sync::CancellationToken;

pub use crate::mux::MuxStream as RelayIo;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PING_EVERY: Duration = Duration::from_secs(30);
const SILENT_LIMIT: Duration = Duration::from_secs(90);

/// How a hub reaches one relay.
#[derive(Debug, Clone)]
pub struct HubConfig {
    pub url: RelayUrl,
    /// Shown to the relay in `hello` (§5.3), for example `twr 0.7.0`.
    pub client: String,
    /// Extra roots for a relay under a private CA; empty in normal use.
    pub extra_roots: Vec<CertificateDer<'static>>,
}

impl HubConfig {
    pub fn new(url: RelayUrl, client: impl Into<String>) -> Self {
        Self { url, client: client.into(), extra_roots: Vec::new() }
    }
}

/// One phone connection the relay passed to the hub (§5.6). `io` carries the phone's bytes,
/// starting with its ClientHello: hand it to the gateway's TLS acceptor as is.
pub struct PhoneStream {
    pub phone: Option<String>,
    pub mode: String,
    pub remote: String,
    pub io: MuxStream,
}

impl std::fmt::Debug for PhoneStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhoneStream").field("phone", &self.phone).field("mode", &self.mode).finish()
    }
}

/// Why a session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// The relay sent a fatal error with this code (§5.8).
    Refused(String),
    /// The relay is draining; try again after this many seconds.
    Drain(u64),
    /// The connection dropped, timed out or broke the protocol.
    Lost(String),
    /// [`HubSession::close`] or the client's shutdown.
    Local,
}

type Pending = VecDeque<(&'static str, oneshot::Sender<Result<Msg>>)>;

/// One authenticated connection to a relay.
pub struct HubSession {
    pub welcome: Welcome,
    out: mpsc::Sender<Msg>,
    pending: Arc<Mutex<Pending>>,
    phones: mpsc::Receiver<PhoneStream>,
    closed: watch::Receiver<Option<Closed>>,
    cancel: CancellationToken,
}

impl Drop for HubSession {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn dial(url: &RelayUrl, cfg: rustls::ClientConfig) -> Result<TlsStream<TcpStream>> {
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(url.authority()))
        .await
        .context("connecting to the relay timed out")?
        .with_context(|| format!("connecting to {}", url.authority()))?;
    let _ = tcp.set_nodelay(true);
    let name = match &url.host {
        Host::Name(n) => ServerName::try_from(n.clone())?,
        Host::Ip(ip) => ServerName::IpAddress((*ip).into()),
    };
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(name, tcp))
        .await
        .context("the relay's TLS handshake timed out")?
        .context("TLS with the relay")?;
    Ok(tls)
}

fn relay_tls(url: &RelayUrl, extra_roots: &[CertificateDer<'static>], alpn: &[&[u8]]) -> rustls::ClientConfig {
    if url.pin.is_none() && !extra_roots.is_empty() {
        return tls::client_config_with_roots(extra_roots.to_vec(), alpn);
    }
    tls::client_config(url.pin, alpn)
}

impl HubSession {
    /// Connects, authenticates with `key` and waits for `welcome` (§5.1-§5.4).
    pub async fn connect(cfg: &HubConfig, key: &IdentityKey) -> Result<Self> {
        let tls_cfg = relay_tls(&cfg.url, &cfg.extra_roots, proto::HUB_ALPNS);
        let tls = dial(&cfg.url, tls_cfg).await?;
        let conn = tls.get_ref().1;
        let alpn = conn.alpn_protocol().map(|a| a.to_vec());
        if alpn.as_deref() != Some(proto::ALPN_V1) {
            bail!("the relay doesn't speak twr-relay/1");
        }
        let exporter: [u8; 32] = conn
            .export_keying_material([0u8; 32], proto::EXPORTER_LABEL, Some(proto::ALPN_V1))
            .map_err(|e| anyhow!("TLS exporter: {e}"))?;
        let sig = key.sign(&proto::hub_auth_message(&exporter))?;

        let cancel = CancellationToken::new();
        let (open_tx, open_rx) = mpsc::channel(4);
        let (in_tx, mut in_rx) = mpsc::channel::<MuxStream>(16);
        let driver_cancel = cancel.clone();
        let (closed_tx, closed_rx) = watch::channel(None::<Closed>);
        let closed_tx = Arc::new(closed_tx);
        let ctx = closed_tx.clone();
        tokio::spawn(async move {
            let r = mux::drive(tls, mux::hub_config(), yamux::Mode::Client, open_rx, in_tx, driver_cancel).await;
            let why = match r {
                Ok(()) => Closed::Lost("the relay closed the connection".into()),
                Err(e) => Closed::Lost(format!("connection error: {e}")),
            };
            ctx.send_if_modified(|c| {
                if c.is_none() {
                    *c = Some(why);
                    true
                } else {
                    false
                }
            });
        });

        let (tx, rx) = oneshot::channel();
        open_tx.send(tx).await.map_err(|_| anyhow!("connection closed"))?;
        let mut control = rx.await.map_err(|_| anyhow!("connection closed"))??;
        proto::write_frame(
            &mut control,
            &Msg::Hello {
                spki: names::b64(&key.spki()),
                sig: names::b64(&sig),
                client: cfg.client.chars().take(64).collect(),
                features: vec![],
            },
        )
        .await?;
        let welcome = match tokio::time::timeout(CONNECT_TIMEOUT, proto::read_frame(&mut control)).await {
            Err(_) => bail!("the relay sent no welcome"),
            Ok(r) => match r? {
                Some(Incoming::Msg(Msg::Welcome(w))) => w,
                Some(Incoming::Msg(Msg::Error { code, message, .. })) => {
                    bail!("the relay refused the hub: {code} ({message})")
                }
                other => bail!("unexpected answer to hello: {other:?}"),
            },
        };
        if welcome.hub != names::hub_name(&key.pin()) {
            bail!("the relay named this hub {} instead of its own name", welcome.hub);
        }

        // Control stream: one writer task, one reader task.
        let (out_tx, mut out_rx) = mpsc::channel::<Msg>(32);
        let pending: Arc<Mutex<Pending>> = Arc::default();
        let (mut ctl_r, mut ctl_w) = tokio::io::split(control);
        let wcancel = cancel.clone();
        tokio::spawn(async move {
            let mut ping = tokio::time::interval(PING_EVERY);
            ping.tick().await;
            let mut n = 0u64;
            loop {
                let msg = tokio::select! {
                    _ = wcancel.cancelled() => break,
                    _ = ping.tick() => { n += 1; Msg::Ping { n } }
                    m = out_rx.recv() => match m { Some(m) => m, None => break },
                };
                if proto::write_frame(&mut ctl_w, &msg).await.is_err() {
                    break;
                }
            }
            wcancel.cancel();
        });
        let rpending = pending.clone();
        let rcancel = cancel.clone();
        let rout = out_tx.clone();
        let rclosed = closed_tx.clone();
        tokio::spawn(async move {
            let why = loop {
                let frame = tokio::select! {
                    _ = rcancel.cancelled() => break Closed::Local,
                    f = tokio::time::timeout(SILENT_LIMIT, proto::read_frame(&mut ctl_r)) => f,
                };
                let msg = match frame {
                    Err(_) => break Closed::Lost("the relay went silent".into()),
                    Ok(Err(e)) => break Closed::Lost(format!("control stream: {e}")),
                    Ok(Ok(None)) => break Closed::Lost("the relay closed the control stream".into()),
                    Ok(Ok(Some(Incoming::Unknown(t)))) => {
                        let _ = rout
                            .send(Msg::Error { code: "unsupported".into(), message: String::new(), fatal: false, ref_: Some(t) })
                            .await;
                        continue;
                    }
                    Ok(Ok(Some(Incoming::Msg(m)))) => m,
                };
                match msg {
                    Msg::Ping { n } => {
                        let _ = rout.send(Msg::Pong { n }).await;
                    }
                    Msg::Pong { .. } => {}
                    Msg::Drain { retry_after_seconds, .. } => break Closed::Drain(retry_after_seconds),
                    Msg::Error { code, fatal: true, .. } => break Closed::Refused(code),
                    m @ (Msg::PhonesOk { .. } | Msg::MovedOk {}) => {
                        if let Some((_, tx)) = rpending.lock().unwrap().pop_front() {
                            let _ = tx.send(Ok(m));
                        }
                    }
                    Msg::Error { code, message, ref_, .. } => {
                        let mut p = rpending.lock().unwrap();
                        let answers = matches!(p.front(), Some((kind, _)) if ref_.as_deref() == Some(*kind));
                        if answers {
                            let (_, tx) = p.pop_front().unwrap();
                            let _ = tx.send(Err(anyhow!("{code}: {message}")));
                        }
                        // Otherwise a notice (for example `quota`): nothing to resolve.
                    }
                    _ => {}
                }
            };
            for (_, tx) in rpending.lock().unwrap().drain(..) {
                let _ = tx.send(Err(anyhow!("the relay session ended")));
            }
            rclosed.send_if_modified(|c| {
                if c.is_none() {
                    *c = Some(why);
                    true
                } else {
                    false
                }
            });
            rcancel.cancel();
        });

        // Phone streams: read each header (§5.6), then hand the stream on.
        let (phone_tx, phone_rx) = mpsc::channel::<PhoneStream>(16);
        let pcancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                let mut s = tokio::select! {
                    _ = pcancel.cancelled() => break,
                    s = in_rx.recv() => match s { Some(s) => s, None => break },
                };
                let tx = phone_tx.clone();
                tokio::spawn(async move {
                    let header = tokio::time::timeout(CONNECT_TIMEOUT, proto::read_frame(&mut s)).await;
                    if let Ok(Ok(Some(Incoming::Msg(Msg::Phone { phone, mode, remote })))) = header {
                        let _ = tx.send(PhoneStream { phone, mode, remote, io: s }).await;
                    }
                });
            }
        });

        Ok(Self { welcome, out: out_tx, pending, phones: phone_rx, closed: closed_rx, cancel })
    }

    async fn request(&self, kind: &'static str, msg: Msg) -> Result<Msg> {
        let (tx, rx) = oneshot::channel();
        // Queue the answer slot before the request can be answered.
        self.pending.lock().unwrap().push_back((kind, tx));
        self.out.send(msg).await.map_err(|_| anyhow!("the relay session ended"))?;
        tokio::time::timeout(CONNECT_TIMEOUT, rx)
            .await
            .context("the relay didn't answer")?
            .map_err(|_| anyhow!("the relay session ended"))?
    }

    /// Replaces the relay's set of phone credentials for this hub (§5.5).
    pub async fn set_phones(&self, phones: Vec<PhoneCred>) -> Result<u32> {
        match self.request("phones", Msg::Phones { phones }).await? {
            Msg::PhonesOk { count } => Ok(count),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// Leaves a signed moved note at this relay (§5.7); see [`sign_moved`].
    pub async fn moved(&self, body: String, sig: String) -> Result<()> {
        match self.request("moved", Msg::Moved { body, sig }).await? {
            Msg::MovedOk {} => Ok(()),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// The next phone stream, or `None` once the session has ended.
    pub async fn accept(&mut self) -> Option<PhoneStream> {
        tokio::select! {
            s = self.phones.recv() => s,
            _ = self.cancel.cancelled() => None,
        }
    }

    /// Waits until the session ends, and says why.
    pub async fn closed(&self) -> Closed {
        wait_closed(self.closed.clone()).await
    }

    pub fn close(&self) {
        self.cancel.cancel();
    }
}

async fn wait_closed(mut rx: watch::Receiver<Option<Closed>>) -> Closed {
        loop {
            if let Some(c) = rx.borrow().clone() {
                return c;
            }
            if rx.changed().await.is_err() {
                return Closed::Local;
            }
        }
}

/// A random phone credential (§3.3).
pub fn new_phone_credential() -> Result<(PhoneCred, [u8; 32])> {
    use ring::rand::SecureRandom;
    let rng = ring::rand::SystemRandom::new();
    let mut id = [0u8; 16];
    let mut key = [0u8; 32];
    rng.fill(&mut id).map_err(|_| anyhow!("no randomness"))?;
    rng.fill(&mut key).map_err(|_| anyhow!("no randomness"))?;
    Ok((PhoneCred { id: hex::encode(id), key: names::b64(&key) }, key))
}

/// A moved note's body and signature for `key`'s hub (§5.7). `routes` is the invitation's
/// route list, which the relay doesn't read.
pub fn sign_moved(key: &IdentityKey, issued: u64, routes: serde_json::Value) -> Result<(String, String)> {
    let body = serde_json::to_vec(&serde_json::json!({
        "hub": names::hub_name(&key.pin()),
        "issued": issued,
        "routes": routes,
    }))?;
    if body.len() > proto::MAX_MOVED_BODY {
        bail!("the moved note is over {} bytes", proto::MAX_MOVED_BODY);
    }
    let sig = key.sign(&proto::moved_message(&body))?;
    Ok((names::b64(&body), names::b64(&sig)))
}

/// A phone's check of a moved note fetched from `/v1/hubs/<hub>/moved` (§5.7, §8.3): the SPKI
/// must have `pin`, the signature must verify, and the body must name the hub. Returns the body.
pub fn verify_moved(pin: &[u8; 32], spki_b64: &str, body_b64: &str, sig_b64: &str) -> Option<serde_json::Value> {
    let spki = names::unb64(spki_b64)?;
    if &names::pin(&spki) != pin {
        return None;
    }
    let body = names::unb64(body_b64)?;
    let sig = names::unb64(sig_b64)?;
    if !crate::keys::verify(&spki, &proto::moved_message(&body), &sig) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&body).ok()?;
    (v.get("hub")?.as_str()? == names::hub_name(pin)).then_some(v)
}

// ---- Supervisor: one relay, reconnecting -------------------------------------------------

/// What [`HubClient`] reports about its relay.
#[derive(Debug, Clone, Default)]
pub struct HubStatus {
    pub welcome: Option<Welcome>,
    pub connected: bool,
    pub last_error: Option<String>,
}

/// Keeps a hub connected to one relay, as §5.8 asks: reconnects with backoff, resends the
/// phone set after every welcome and whenever it changes, and passes phone streams on.
pub struct HubClient {
    phones: watch::Sender<Vec<PhoneCred>>,
    status: watch::Receiver<HubStatus>,
    cancel: CancellationToken,
}

impl Drop for HubClient {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// The backoff after `failures` failed attempts in a row: 1 s doubling to 5 minutes, with up
/// to a quarter of jitter (§5.8).
pub fn backoff(failures: u32) -> Duration {
    let base = 1u64 << failures.min(9);
    let secs = base.min(300);
    let jitter = {
        use ring::rand::SecureRandom;
        let mut b = [0u8; 2];
        let _ = ring::rand::SystemRandom::new().fill(&mut b);
        u16::from_le_bytes(b) as u64
    };
    Duration::from_millis(secs * 1000 + jitter % (secs * 250 + 1))
}

impl HubClient {
    /// Starts the client. Phone streams arrive on the returned receiver.
    pub fn spawn(cfg: HubConfig, key: Arc<IdentityKey>, phones: Vec<PhoneCred>) -> (Self, mpsc::Receiver<PhoneStream>) {
        let (phones_tx, phones_rx) = watch::channel(phones);
        let (status_tx, status_rx) = watch::channel(HubStatus::default());
        let (stream_tx, stream_rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        let c = cancel.clone();
        tokio::spawn(async move {
            let mut failures = 0u32;
            loop {
                let wait = match Self::run_once(&cfg, &key, phones_rx.clone(), &status_tx, &stream_tx, &c).await {
                    Ok(Closed::Local) => return,
                    Ok(Closed::Drain(secs)) => {
                        failures = 0;
                        Duration::from_secs(secs.clamp(1, 3600))
                    }
                    Ok(Closed::Refused(code)) if code == "denied" => Duration::from_secs(3600),
                    Ok(Closed::Refused(code)) if code == "replaced" => {
                        // Another process holds this hub's key and took over: don't fight it.
                        failures = failures.saturating_add(1);
                        backoff(failures.max(6))
                    }
                    Ok(_) | Err(_) => {
                        failures = failures.saturating_add(1);
                        backoff(failures - 1)
                    }
                };
                tokio::select! {
                    _ = c.cancelled() => return,
                    _ = tokio::time::sleep(wait) => {}
                }
            }
        });
        (Self { phones: phones_tx, status: status_rx, cancel }, stream_rx)
    }

    async fn run_once(
        cfg: &HubConfig,
        key: &IdentityKey,
        mut phones: watch::Receiver<Vec<PhoneCred>>,
        status: &watch::Sender<HubStatus>,
        streams: &mpsc::Sender<PhoneStream>,
        cancel: &CancellationToken,
    ) -> Result<Closed> {
        let mut session = match HubSession::connect(cfg, key).await {
            Ok(s) => s,
            Err(e) => {
                status.send_modify(|s| {
                    s.connected = false;
                    s.last_error = Some(format!("{e:#}"));
                });
                return Err(e);
            }
        };
        let set = phones.borrow_and_update().clone();
        if let Err(e) = session.set_phones(set).await {
            status.send_modify(|s| s.last_error = Some(format!("{e:#}")));
        }
        status.send_modify(|s| {
            s.welcome = Some(session.welcome.clone());
            s.connected = true;
            s.last_error = None;
        });
        let closed = session.closed.clone();
        let closed_fut = wait_closed(closed.clone());
        tokio::pin!(closed_fut);
        let why = loop {
            tokio::select! {
                _ = cancel.cancelled() => break Closed::Local,
                c = &mut closed_fut => break c,
                ch = phones.changed() => {
                    if ch.is_err() { break Closed::Local; }
                    let set = phones.borrow_and_update().clone();
                    if let Err(e) = session.set_phones(set).await {
                        status.send_modify(|s| s.last_error = Some(format!("{e:#}")));
                    }
                }
                s = session.phones.recv() => match s {
                    Some(s) => { let _ = streams.send(s).await; }
                    None => break wait_closed(closed.clone()).await,
                },
            }
        };
        session.close();
        status.send_modify(|s| {
            s.connected = false;
            if let Closed::Refused(code) | Closed::Lost(code) = &why {
                s.last_error = Some(code.clone());
            }
        });
        Ok(why)
    }

    /// Replaces the phone set; the relay gets it at once if connected, or on reconnect.
    pub fn set_phones(&self, phones: Vec<PhoneCred>) {
        self.phones.send_replace(phones);
    }

    pub fn status(&self) -> watch::Receiver<HubStatus> {
        self.status.clone()
    }

    pub fn shutdown(&self) {
        self.cancel.cancel();
    }
}

// ---- HTTPS helpers -------------------------------------------------------------------------

/// A minimal HTTPS/1.1 request to a relay's API, checking the relay as its URL says (§3.4).
pub async fn https(
    url: &RelayUrl,
    extra_roots: &[CertificateDer<'static>],
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> Result<(u16, Vec<u8>)> {
    use http_body_util::{BodyExt, Full};
    let tls = dial(url, relay_tls(url, extra_roots, &[b"http/1.1"])).await?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(tls)).await?;
    tokio::spawn(conn);
    let host = match url.port {
        443 => url.host.to_string(),
        p => format!("{}:{p}", url.host),
    };
    let mut req = hyper::Request::builder().method(method).uri(path).header("host", host);
    if body.is_some() {
        req = req.header("content-type", "application/json");
    }
    let req = req.body(Full::new(bytes::Bytes::from(body.unwrap_or_default())))?;
    let resp = tokio::time::timeout(CONNECT_TIMEOUT, sender.send_request(req)).await.context("the relay didn't answer")??;
    let status = resp.status().as_u16();
    let body = tokio::time::timeout(CONNECT_TIMEOUT, resp.into_body().collect())
        .await
        .context("the relay's answer stalled")??
        .to_bytes();
    if body.len() > 1 << 20 {
        bail!("the relay's answer is too large");
    }
    Ok((status, body.to_vec()))
}

/// `/v1/info` (§8.1).
pub async fn info(url: &RelayUrl, extra_roots: &[CertificateDer<'static>]) -> Result<serde_json::Value> {
    let (status, body) = https(url, extra_roots, "GET", "/v1/info", None).await?;
    if status != 200 {
        bail!("/v1/info answered {status}");
    }
    Ok(serde_json::from_slice(&body)?)
}

/// Posts an admission (§10.2) and returns what the relay admitted.
pub async fn post_admission(
    url: &RelayUrl,
    extra_roots: &[CertificateDer<'static>],
    admission: &admit::Admission,
) -> Result<admit::AdmitAnswer> {
    let (status, body) = https(url, extra_roots, "POST", "/v1/iroh/admit", Some(serde_json::to_vec(admission)?)).await?;
    if status != 200 {
        let err = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or_default();
        bail!("admission refused ({status} {err})");
    }
    admit::parse_answer(&body)
}

/// A phone's status-page check (§8.2): `Some(connected)` if the relay knows this credential,
/// `None` if it answers "unknown".
pub async fn hub_status(
    url: &RelayUrl,
    extra_roots: &[CertificateDer<'static>],
    hub_name: &str,
    relay_key: &[u8; 32],
) -> Result<Option<bool>> {
    let tag = names::tag(relay_key, hub_name, names::window(crate::now_unix()));
    let path = format!("/v1/hubs/{}", names::label(hub_name, &tag));
    let (status, body) = https(url, extra_roots, "GET", &path, None).await?;
    match status {
        200 => Ok(serde_json::from_slice::<serde_json::Value>(&body)?.get("connected").and_then(|c| c.as_bool())),
        404 => Ok(None),
        s => bail!("status page answered {s}"),
    }
}

// ---- Phone side (reference) ------------------------------------------------------------------

/// What a phone does on a relay route (§3.3, §6): TCP to the route's host, TLS with the
/// phone's SNI, and the hub's pin as the only check. The phone apps do the same natively.
pub async fn phone_connect(
    route: &PhoneRoute,
    hub_pin: &[u8; 32],
    relay_key: &[u8; 32],
    alpn: &[&[u8]],
) -> Result<TlsStream<TcpStream>> {
    let hub = names::hub_name(hub_pin);
    let tag = names::tag(relay_key, &hub, names::window(crate::now_unix()));
    let sni = names::phone_sni(&hub, &tag, &route.suffix);
    let host = if route.host.contains(':') && !route.host.starts_with('[') {
        format!("[{}]", route.host)
    } else {
        route.host.clone()
    };
    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(format!("{host}:{}", route.port)))
        .await
        .context("connecting to the relay timed out")??;
    let _ = tcp.set_nodelay(true);
    let cfg = tls::client_config(Some(*hub_pin), alpn);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(cfg));
    let tls = tokio::time::timeout(CONNECT_TIMEOUT, connector.connect(ServerName::try_from(sni)?, tcp))
        .await
        .context("the hub's TLS handshake timed out")??;
    Ok(tls)
}

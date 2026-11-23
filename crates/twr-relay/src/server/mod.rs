//! `twr relay serve` (docs/relay-protocol.md, docs/relay.md).

pub mod certs;
pub mod config;
pub mod counters;
pub mod install;
pub mod limits;
pub mod probe;
pub mod stores;

mod api;
mod front;
mod hub;
mod iroh;
mod phone;

use self::config::Config;
use self::counters::Counters;
use self::limits::{Concurrent, PerIp, Prefix};
use self::stores::{Records, State};
use crate::{mux, names, proto};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// How long a hub's phone credentials outlive its session (§5.5).
const KEEP_CREDS_SECS: u64 = 7 * 86400;

struct Deny {
    hubs: HashSet<String>,
    endpoints: HashSet<[u8; 32]>,
    prefixes: Vec<Prefix>,
    /// `limits.exempt_addresses`.
    exempt: Vec<Prefix>,
}

impl Deny {
    fn ip(&self, ip: IpAddr) -> bool {
        self.prefixes.iter().any(|p| p.contains(ip))
    }

    fn exempt(&self, ip: IpAddr) -> bool {
        self.exempt.iter().any(|p| p.contains(ip))
    }
}

struct Limiters {
    phone_attempts: PerIp,
    record_puts: PerIp,
    pairing_admits: PerIp,
    pair_admits: PerIp,
    /// New hub names per address in the last hour (§11).
    hub_names: Mutex<HashMap<limits::IpKey, Vec<(String, Instant)>>>,
}

/// A live hub session.
struct Session {
    id: String,
    open: mpsc::Sender<mux::OpenRequest>,
    out: mpsc::Sender<proto::Msg>,
    cancel: CancellationToken,
}

#[derive(Default)]
struct PhoneSet {
    creds: Vec<(String, [u8; 32])>,
    /// Cancels a phone's open connections when its credential goes (§5.5).
    tokens: HashMap<String, CancellationToken>,
}

/// Everything the relay keeps about one hub name.
struct Hub {
    name: String,
    session: Mutex<Option<Session>>,
    phones: Mutex<PhoneSet>,
    active: AtomicU32,
    /// Unix time the last session ended; 0 while one is live.
    offline_since: Mutex<u64>,
}

impl Hub {
    fn new(name: String) -> Self {
        Self {
            name,
            session: Mutex::new(None),
            phones: Mutex::default(),
            active: AtomicU32::new(0),
            offline_since: Mutex::new(0),
        }
    }

    /// The credential id a tag belongs to, if any.
    fn match_tag(&self, tag: &str, now: u64) -> Option<String> {
        let w = names::window(now);
        let set = self.phones.lock().unwrap();
        let mut found = None;
        for (id, key) in &set.creds {
            // Check every credential, so timing doesn't say which one matched.
            if names::tag_matches(key, &self.name, tag, w) && found.is_none() {
                found = Some(id.clone());
            }
        }
        found
    }

    fn connected(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }
}

pub(crate) struct Shared {
    cfg: Config,
    version: String,
    started: Instant,
    counters: Arc<Counters>,
    certs: certs::Certs,
    own_tls: Arc<rustls::ServerConfig>,
    hubs: Mutex<HashMap<String, Arc<Hub>>>,
    records: Records,
    state: Arc<State>,
    limits: Limiters,
    pending: Arc<Concurrent>,
    deny: Arc<Deny>,
    iroh: Option<iroh::Iroh>,
    cancel: CancellationToken,
    draining: AtomicBool,
    suffixes: Vec<String>,
    own_host: Option<String>,
    iroh_host: Option<String>,
    public_host: String,
    public_port: u16,
    iroh_url: Option<String>,
    /// Hub name → its own port, for port mode (§7).
    port_routes: HashMap<String, u16>,
}

impl Shared {
    /// Whether a per-address limit allows `ip` one more event.
    fn allow(&self, limiter: &PerIp, ip: IpAddr) -> bool {
        self.deny.exempt(ip) || limiter.allow(ip)
    }

    fn hub(&self, name: &str) -> Option<Arc<Hub>> {
        self.hubs.lock().unwrap().get(name).cloned()
    }

    fn base_url(&self) -> String {
        let host = if self.public_host.contains(':') {
            format!("[{}]", self.public_host)
        } else {
            self.public_host.clone()
        };
        match self.public_port {
            443 => format!("https://{host}"),
            p => format!("https://{host}:{p}"),
        }
    }

    fn records_url(&self) -> String {
        format!("{}/pkarr", self.base_url())
    }

    fn relay_pin_text(&self) -> Option<String> {
        self.certs.self_pin.as_ref().map(names::pin_text)
    }

    fn limits_msg(&self) -> proto::Limits {
        let l = &self.cfg.limits;
        proto::Limits {
            phones: l.phones_per_hub,
            phone_connections: l.phone_connections_per_hub,
            bytes_per_second: l.bytes_per_second,
            monthly_bytes: l.monthly_bytes_per_hub,
            idle_seconds: l.idle_seconds,
        }
    }
}

/// A running relay.
pub struct Running {
    /// The main listeners' addresses.
    pub addrs: Vec<SocketAddr>,
    /// Port-mode listeners, by hub name.
    pub port_addrs: HashMap<String, SocketAddr>,
    pub iroh_addr: Option<SocketAddr>,
    pub quic_addr: Option<SocketAddr>,
    pub metrics_addr: Option<SocketAddr>,
    pub self_pin: Option<[u8; 32]>,
    shared: Arc<Shared>,
    tasks: tokio::task::JoinSet<()>,
}

impl Running {
    /// The URL hubs use for this relay (§3.4).
    pub fn relay_url(&self) -> crate::RelayUrl {
        let host = self
            .shared
            .own_host
            .clone()
            .unwrap_or_else(|| self.shared.public_host.clone());
        let host_text = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let mut url =
            crate::RelayUrl::parse(&format!("relay://{host_text}:{}", self.shared.public_port))
                .expect("the relay's own address parses");
        url.pin = self.self_pin;
        url
    }

    pub fn counters(&self) -> Arc<Counters> {
        self.shared.counters.clone()
    }

    /// Waits for SIGTERM or SIGINT, then shuts down.
    pub async fn run_until_signal(self) -> Result<()> {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
        tracing::info!("shutting down: draining hubs");
        self.shutdown().await;
        Ok(())
    }

    /// Drains hubs (§5.8), stops every listener and saves state.
    pub async fn shutdown(mut self) {
        self.shared.draining.store(true, Relaxed);
        let sessions: Vec<_> = {
            let hubs = self.shared.hubs.lock().unwrap();
            hubs.values()
                .filter_map(|h| {
                    h.session
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|s| (s.out.clone(), s.cancel.clone()))
                })
                .collect()
        };
        for (out, _) in &sessions {
            let _ = out.try_send(proto::Msg::Drain {
                reason: "restart".into(),
                retry_after_seconds: self.shared.cfg.relay.drain_retry_seconds,
            });
        }
        if !sessions.is_empty() {
            // Give hubs a moment to move; they reconnect elsewhere on drain.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.shared.cancel.cancel();
        if let Some(i) = &self.shared.iroh {
            i.shutdown().await;
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while self.tasks.join_next().await.is_some() {}
        })
        .await;
        self.tasks.abort_all();
        if let Err(e) = self.shared.state.save() {
            tracing::warn!("saving state: {e:#}");
        }
    }
}

/// Starts the relay: binds every listener and returns once they're open.
pub async fn start(cfg: Config, version: &str) -> Result<Running> {
    cfg.validate()?;
    std::fs::create_dir_all(&cfg.relay.state_dir)
        .with_context(|| format!("creating {}", cfg.relay.state_dir.display()))?;
    let cancel = CancellationToken::new();
    let counters = Arc::new(Counters::default());
    let certs = certs::Certs::start(&cfg, counters.clone(), cancel.clone())?;
    let own_tls = certs.own_config();
    let state = Arc::new(State::load(&cfg.relay.state_dir)?);
    let deny = Arc::new(Deny {
        hubs: cfg.deny.hubs.iter().cloned().collect(),
        endpoints: cfg
            .deny
            .endpoints
            .iter()
            .filter_map(|e| crate::admit::parse_endpoint(e))
            .collect(),
        prefixes: cfg
            .deny
            .addresses
            .iter()
            .map(|a| Prefix::parse(a))
            .collect::<Result<_>>()?,
        exempt: cfg
            .limits
            .exempt_addresses
            .iter()
            .map(|a| Prefix::parse(a))
            .collect::<Result<_>>()?,
    });

    let mut main = Vec::new();
    for addr in &cfg.relay.listen {
        main.push(bind(*addr).await?);
    }
    let addrs: Vec<SocketAddr> = main
        .iter()
        .map(|l| l.local_addr())
        .collect::<std::io::Result<_>>()?;
    let public_port = cfg.relay.public_port.unwrap_or(addrs[0].port());
    let public_host = cfg.public_host();

    let mut port_listeners = Vec::new();
    let mut port_routes = HashMap::new();
    let mut port_addrs = HashMap::new();
    for p in &cfg.port_hubs {
        let l = bind(p.listen).await?;
        let a = l.local_addr()?;
        port_routes.insert(p.hub.clone(), p.public_port.unwrap_or(a.port()));
        port_addrs.insert(p.hub.clone(), a);
        port_listeners.push((l, p.clone()));
    }

    let iroh_listener = match (cfg.iroh.enabled, cfg.iroh.listen) {
        (true, Some(a)) => Some(bind(a).await?),
        _ => None,
    };
    let iroh_addr = iroh_listener.as_ref().map(|l| l.local_addr()).transpose()?;
    let iroh_host = cfg.iroh_hostname();
    let iroh_url = if !cfg.iroh.enabled {
        None
    } else if let Some(h) = &iroh_host {
        Some(match public_port {
            443 => format!("https://{h}/"),
            p => format!("https://{h}:{p}/"),
        })
    } else {
        iroh_addr.map(|a| {
            let host = if public_host.contains(':') {
                format!("[{public_host}]")
            } else {
                public_host.clone()
            };
            format!("https://{host}:{}/", a.port())
        })
    };
    let iroh = if cfg.iroh.enabled {
        Some(iroh::Iroh::start(&cfg, &certs, state.clone(), deny.clone(), counters.clone()).await?)
    } else {
        None
    };
    let quic_addr = iroh.as_ref().and_then(|i| i.quic_addr());

    let limits = Limiters {
        phone_attempts: PerIp::new(
            cfg.limits.phone_attempts_per_ip_per_minute,
            Duration::from_secs(60),
        ),
        record_puts: PerIp::new(
            cfg.limits.record_puts_per_ip_per_minute,
            Duration::from_secs(60),
        ),
        pairing_admits: PerIp::new(
            cfg.limits.pairing_admissions_per_ip_per_hour,
            Duration::from_secs(3600),
        ),
        pair_admits: PerIp::new(
            cfg.limits.pair_admissions_per_ip_per_hour,
            Duration::from_secs(3600),
        ),
        hub_names: Mutex::default(),
    };
    let metrics_listener = match cfg.relay.metrics_listen {
        Some(a) => Some(bind(a).await?),
        None => None,
    };
    let metrics_addr = metrics_listener
        .as_ref()
        .map(|l| l.local_addr())
        .transpose()?;

    let shared = Arc::new(Shared {
        suffixes: cfg.phone_suffixes(),
        own_host: cfg.relay.hostname.clone(),
        iroh_host,
        public_host,
        public_port,
        iroh_url,
        port_routes,
        version: version.to_string(),
        started: Instant::now(),
        counters,
        own_tls,
        hubs: Mutex::default(),
        records: Records::default(),
        state,
        limits,
        pending: Arc::new(Concurrent::default()),
        deny,
        iroh,
        cancel: cancel.clone(),
        draining: AtomicBool::new(false),
        certs,
        cfg,
    });
    let self_pin = shared.certs.self_pin;

    let mut tasks = tokio::task::JoinSet::new();
    for l in main {
        tasks.spawn(front::accept_loop(shared.clone(), l, front::Kind::Main));
    }
    for (l, p) in port_listeners {
        tasks.spawn(front::accept_loop(shared.clone(), l, front::Kind::Port(p)));
    }
    if let Some(l) = iroh_listener {
        tasks.spawn(front::accept_loop(shared.clone(), l, front::Kind::Iroh));
    }
    if let Some(l) = metrics_listener {
        tasks.spawn(metrics_loop(shared.clone(), l));
    }
    tasks.spawn(housekeeping(shared.clone()));

    Ok(Running {
        addrs,
        port_addrs,
        iroh_addr,
        quic_addr,
        metrics_addr,
        self_pin,
        shared,
        tasks,
    })
}

async fn bind(addr: SocketAddr) -> Result<TcpListener> {
    let socket = if addr.is_ipv6() {
        tokio::net::TcpSocket::new_v6()?
    } else {
        tokio::net::TcpSocket::new_v4()?
    };
    socket.set_reuseaddr(true)?;
    // `[::]` also takes IPv4 under Linux's default `net.ipv6.bindv6only = 0`.
    socket
        .bind(addr)
        .with_context(|| format!("binding {addr}"))?;
    Ok(socket.listen(1024)?)
}

fn now() -> u64 {
    crate::now_unix()
}

async fn housekeeping(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            _ = shared.cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        let t = now();
        let n = shared
            .records
            .sweep(t, shared.cfg.limits.record_ttl_seconds);
        shared.counters.records_stored.store(n as i64, Relaxed);
        let a = shared.state.sweep(t);
        shared.counters.iroh_admitted.store(a as i64, Relaxed);
        for l in [
            &shared.limits.phone_attempts,
            &shared.limits.record_puts,
            &shared.limits.pairing_admits,
            &shared.limits.pair_admits,
        ] {
            l.sweep();
        }
        {
            let mut names = shared.limits.hub_names.lock().unwrap();
            for v in names.values_mut() {
                v.retain(|(_, at)| at.elapsed() < Duration::from_secs(3600));
            }
            names.retain(|_, v| !v.is_empty());
        }
        // Forget hubs offline for over a week, credentials and all.
        shared.hubs.lock().unwrap().retain(|_, h| {
            let off = *h.offline_since.lock().unwrap();
            h.connected() || off == 0 || off + KEEP_CREDS_SECS > t
        });
        if let Err(e) = shared.state.save() {
            tracing::warn!("saving state: {e:#}");
        }
    }
}

async fn metrics_loop(shared: Arc<Shared>, listener: TcpListener) {
    use http_body_util::Full;
    use hyper::service::service_fn;
    loop {
        let (tcp, _) = tokio::select! {
            _ = shared.cancel.cancelled() => return,
            r = listener.accept() => match r { Ok(x) => x, Err(_) => continue },
        };
        let s = shared.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let s = s.clone();
                async move {
                    let (status, body) = if req.uri().path() == "/metrics" {
                        (
                            200,
                            s.counters.render(
                                &s.version,
                                s.started.elapsed().as_secs(),
                                now() as i64,
                            ),
                        )
                    } else {
                        (404, "not found\n".to_string())
                    };
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(status)
                            .header("content-type", "text/plain; version=0.0.4")
                            .body(Full::new(bytes::Bytes::from(body)))
                            .unwrap(),
                    )
                }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(hyper_util::rt::TokioIo::new(tcp), svc)
                .await;
        });
    }
}

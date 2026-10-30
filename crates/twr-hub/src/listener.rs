//! The runner listener: a WebSocket server on `hub.listen` (off when unset) that hands every
//! accepted connection to [`Hub::serve_runner`].
//!
//! Transport: runners authenticate with a mutual challenge-response over their box's token
//! (the token itself never crosses the connection, see [`twr_core::runner`]) and then stream
//! the whole event log of their threads, so the listener refuses a non-loopback address unless
//! the connection is encrypted (`[hub] tls = {cert, key}`: `wss://`, served with rustls) or
//! the user explicitly accepts plain text (`[hub] allow_insecure_remote = true`). The
//! recommended deployment is a loopback listener reached through an SSH tunnel or Tailscale,
//! or TLS.
//!
//! TLS certificates and keys are PEM files (the certificate chain leaf first; the key PKCS#8,
//! PKCS#1 or SEC1). Messages are limited to [`MAX_FRAME_BYTES`] (16 MiB) either way, and to
//! [`HELLO_MAX_BYTES`] until the connection has authenticated.
//!
//! Connections that have not authenticated yet are limited to [`PRE_AUTH_CONNECTIONS`] at once,
//! and to [`PRE_AUTH_PER_SOURCE`] per source address that is not loopback (through an SSH
//! tunnel every runner comes from loopback); beyond that a new connection is closed right
//! away. Each gets one deadline from its accept (`HubOptions::hello_timeout`, 15 s by default)
//! for its TLS and WebSocket handshakes, its hello and its proof together, so unauthenticated
//! peers cannot make the hub hold many connections, or one for long. What the hub buffers to
//! send a connection is bounded too ([`pre_auth_ws_config`], [`runner_ws_config`]): a peer that
//! sends WebSocket pings without reading the pongs cannot grow the hub's memory.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use twr_core::config::{HubConfig, TlsConfig};
use twr_core::runner::MAX_FRAME_BYTES;

use crate::Hub;

/// Connections that have not authenticated yet, at most (see the module docs).
pub const PRE_AUTH_CONNECTIONS: usize = 16;

/// Connections from one source address that is not loopback that have not authenticated yet,
/// at most (see the module docs).
pub const PRE_AUTH_PER_SOURCE: usize = 4;

/// Largest message (and frame) a connection may send before it has authenticated: its hello
/// and its proof are small (the inventory comes after).
pub const HELLO_MAX_BYTES: usize = 8 * 1024;

/// Most bytes the hub buffers to send a connection that has not authenticated (its
/// challenge, its refusal, pongs).
pub const PRE_AUTH_WRITE_BUFFER: usize = 64 * 1024;

/// Most bytes the hub buffers to send an authenticated connection, beyond the largest
/// message: room for one message of [`MAX_FRAME_BYTES`] next to pongs not taken yet.
pub const WRITE_BUFFER_MARGIN: usize = 1 << 20;

/// A bound runner listener, not serving yet.
pub struct RunnerListener {
    tcp: TcpListener,
    tls: Option<TlsAcceptor>,
    addr: SocketAddr,
}

impl std::fmt::Debug for RunnerListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerListener")
            .field("addr", &self.addr)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

/// Refuses a non-loopback `addr` without TLS or the explicit opt-in.
pub fn check_bind(addr: &SocketAddr, cfg: &HubConfig) -> anyhow::Result<()> {
    if addr.ip().is_loopback() || cfg.tls.is_some() || cfg.allow_insecure_remote {
        return Ok(());
    }
    bail!(
        "refusing to listen for runners on {addr}: it is not a loopback address and the \
         listener has no TLS. Set [hub] tls = {{ cert = \"...pem\", key = \"...pem\" }} (wss://), \
         or listen on 127.0.0.1 and reach it through an SSH tunnel or Tailscale, or set [hub] \
         allow_insecure_remote = true to send runner tokens and events in clear text"
    )
}

/// A rustls server configuration from PEM files.
pub fn tls_acceptor(tls: &TlsConfig) -> anyhow::Result<TlsAcceptor> {
    let certs = CertificateDer::pem_file_iter(&tls.cert)
        .with_context(|| format!("reading the certificate {}", tls.cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing the certificate {}", tls.cert.display()))?;
    if certs.is_empty() {
        bail!("no certificate in {}", tls.cert.display());
    }
    let key = PrivateKeyDer::from_pem_file(&tls.key)
        .with_context(|| format!("reading the private key {}", tls.key.display()))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("TLS protocol versions")?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("the TLS certificate and key do not go together")?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

impl RunnerListener {
    /// Binds `hub.listen` (`None` when it is unset) after [`check_bind`] and loading the TLS
    /// files, so a bad configuration fails at startup.
    pub async fn bind(cfg: &HubConfig) -> anyhow::Result<Option<RunnerListener>> {
        let Some(listen) = cfg.listen.as_deref().filter(|l| !l.trim().is_empty()) else {
            return Ok(None);
        };
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host(listen)
            .await
            .with_context(|| format!("resolving hub.listen {listen:?}"))?
            .collect();
        let Some(addr) = addrs.first().copied() else {
            bail!("hub.listen {listen:?} resolves to no address");
        };
        for a in &addrs {
            check_bind(a, cfg)?;
        }
        let tls = cfg.tls.as_ref().map(tls_acceptor).transpose()?;
        let tcp = TcpListener::bind(addr)
            .await
            .with_context(|| format!("listening for runners on {addr}"))?;
        let addr = tcp.local_addr()?;
        Ok(Some(RunnerListener { tcp, tls, addr }))
    }

    /// The bound address (the real port when `listen` asked for port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Whether connections are TLS (`wss://`).
    pub fn is_tls(&self) -> bool {
        self.tls.is_some()
    }

    /// The URL runners connect to (`ws://` or `wss://`, with the bound address).
    pub fn url(&self) -> String {
        let scheme = if self.is_tls() { "wss" } else { "ws" };
        format!("{scheme}://{}", self.addr)
    }

    /// Serves `hub` in the background until the returned handle is dropped (which also ends
    /// every connection).
    pub fn serve(self, hub: Hub) -> RunnerServer {
        tracing::info!(addr = %self.addr, tls = self.is_tls(), "listening for runners");
        hub.set_listening(true);
        RunnerServer {
            addr: self.addr,
            hub: hub.clone(),
            task: tokio::spawn(accept_loop(self, hub)),
        }
    }
}

/// The serving runner listener. Dropping it stops accepting and closes every connection.
pub struct RunnerServer {
    addr: SocketAddr,
    /// Told when the listener stops serving it.
    hub: Hub,
    task: tokio::task::JoinHandle<()>,
}

impl RunnerServer {
    /// The bound address.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for RunnerServer {
    fn drop(&mut self) {
        self.hub.set_listening(false);
        self.task.abort();
    }
}

/// What a connection that has not authenticated yet holds until it has (or ends): its place
/// among the [`PRE_AUTH_CONNECTIONS`] (and its source address's [`PRE_AUTH_PER_SOURCE`]), and
/// the deadline by which it must have authenticated.
pub struct PreAuth {
    _slot: OwnedSemaphorePermit,
    _source: Option<SourceSlot>,
    deadline: tokio::time::Instant,
}

impl PreAuth {
    /// When the connection must have authenticated (counted from its accept).
    pub fn deadline(&self) -> tokio::time::Instant {
        self.deadline
    }
}

impl std::fmt::Debug for PreAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreAuth")
            .field("deadline", &self.deadline)
            .finish()
    }
}

/// The unauthenticated connections per source address ([`PRE_AUTH_PER_SOURCE`]).
#[derive(Clone, Default)]
struct SourceSlots(Arc<Mutex<HashMap<IpAddr, usize>>>);

/// One unauthenticated connection's place among its source address's; given back on drop.
struct SourceSlot {
    slots: SourceSlots,
    ip: IpAddr,
}

impl SourceSlots {
    /// A place for a connection from `ip`: `Ok(None)` for a loopback address (not counted),
    /// `Err(())` when `ip` already has [`PRE_AUTH_PER_SOURCE`].
    fn take(&self, ip: IpAddr) -> Result<Option<SourceSlot>, ()> {
        let ip = ip.to_canonical();
        if ip.is_loopback() {
            return Ok(None);
        }
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let n = m.entry(ip).or_insert(0);
        if *n >= PRE_AUTH_PER_SOURCE {
            return Err(());
        }
        *n += 1;
        Ok(Some(SourceSlot {
            slots: self.clone(),
            ip,
        }))
    }

    /// Source addresses with a connection counted.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl Drop for SourceSlot {
    fn drop(&mut self) {
        let mut m = self.slots.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = m.get_mut(&self.ip) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                m.remove(&self.ip);
            }
        }
    }
}

async fn accept_loop(l: RunnerListener, hub: Hub) {
    // Owned here: aborting this task drops the set, which aborts every connection.
    let mut conns = JoinSet::new();
    let pre_auth = Arc::new(Semaphore::new(PRE_AUTH_CONNECTIONS));
    let sources = SourceSlots::default();
    let timeout = hub.runner_hello_timeout();
    loop {
        tokio::select! {
            r = l.tcp.accept() => match r {
                Ok((tcp, peer)) => {
                    let deadline = tokio::time::Instant::now() + timeout;
                    // Held until the connection has authenticated (or it ends).
                    let Ok(source) = sources.take(peer.ip()) else {
                        tracing::warn!(%peer, "refusing a runner connection: {PRE_AUTH_PER_SOURCE} connections from its address are already authenticating");
                        drop(tcp);
                        continue;
                    };
                    let Ok(slot) = Arc::clone(&pre_auth).try_acquire_owned() else {
                        tracing::warn!(%peer, "refusing a runner connection: {PRE_AUTH_CONNECTIONS} connections are already authenticating");
                        drop(tcp);
                        continue;
                    };
                    let permit = PreAuth {
                        _slot: slot,
                        _source: source,
                        deadline,
                    };
                    let _ = tcp.set_nodelay(true);
                    let (hub, tls) = (hub.clone(), l.tls.clone());
                    conns.spawn(async move {
                        let peer = peer.to_string();
                        match tls {
                            Some(acceptor) => {
                                let tls = tokio::time::timeout_at(deadline, acceptor.accept(tcp)).await;
                                match tls {
                                    Ok(Ok(s)) => upgrade(hub, s, peer, permit).await,
                                    Ok(Err(e)) => tracing::warn!(peer, error = %e, "runner TLS handshake failed"),
                                    Err(_) => tracing::warn!(peer, "runner TLS handshake timed out"),
                                }
                            }
                            None => upgrade(hub, tcp, peer, permit).await,
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accepting a runner connection failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// The WebSocket limits of a connection that has not authenticated yet: small messages
/// ([`HELLO_MAX_BYTES`]), each written out at once, and at most [`PRE_AUTH_WRITE_BUFFER`]
/// buffered to send it. Pongs to its pings beyond that are not buffered (tungstenite keeps
/// only the latest one waiting), so a peer that pings without reading holds no more than that.
pub fn pre_auth_ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(HELLO_MAX_BYTES))
        .max_frame_size(Some(HELLO_MAX_BYTES))
        .write_buffer_size(0)
        .max_write_buffer_size(PRE_AUTH_WRITE_BUFFER)
}

/// The WebSocket limits of an authenticated runner connection: messages up to
/// [`MAX_FRAME_BYTES`] either way, and at most one such message and [`WRITE_BUFFER_MARGIN`]
/// buffered to send it (pongs beyond that are not buffered, as for
/// [`pre_auth_ws_config`]; a message the runner does not take closes the connection after
/// `runners::WRITE_TIMEOUT`).
pub fn runner_ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .max_message_size(Some(MAX_FRAME_BYTES))
        .max_frame_size(Some(MAX_FRAME_BYTES))
        .max_write_buffer_size(MAX_FRAME_BYTES + WRITE_BUFFER_MARGIN)
}

async fn upgrade<S>(hub: Hub, stream: S, peer: String, permit: PreAuth)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Small until the connection has authenticated (`Hub::serve_runner_gated` then raises it
    // to `runner_ws_config`).
    let ws = tokio::time::timeout_at(
        permit.deadline(),
        tokio_tungstenite::accept_async_with_config(stream, Some(pre_auth_ws_config())),
    )
    .await;
    match ws {
        Ok(Ok(ws)) => hub.serve_runner_gated(ws, peer, Some(permit)).await,
        Ok(Err(e)) => tracing::warn!(peer, error = %e, "runner WebSocket handshake failed"),
        Err(_) => tracing::warn!(peer, "runner WebSocket handshake timed out"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(tls: bool, insecure: bool) -> HubConfig {
        HubConfig {
            tls: tls.then(|| TlsConfig {
                cert: "/c".into(),
                key: "/k".into(),
            }),
            allow_insecure_remote: insecure,
            ..Default::default()
        }
    }

    /// A source address that is not loopback gets [`PRE_AUTH_PER_SOURCE`] unauthenticated
    /// connections at once, given back as they end; loopback (an SSH tunnel's) is not counted
    /// per address (only by [`PRE_AUTH_CONNECTIONS`]), an IPv4-mapped loopback address
    /// included.
    #[test]
    fn unauthenticated_connections_are_capped_per_source_address() {
        let slots = SourceSlots::default();
        let lan: IpAddr = "192.168.1.5".parse().unwrap();
        let other: IpAddr = "192.168.1.6".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.5".parse().unwrap();
        let mut held: Vec<SourceSlot> = (0..PRE_AUTH_PER_SOURCE)
            .map(|_| slots.take(lan).unwrap().unwrap())
            .collect();
        assert!(slots.take(lan).is_err());
        assert!(slots.take(mapped).is_err(), "the same address, mapped");
        let o = slots.take(other).unwrap().unwrap();
        for lo in ["127.0.0.1", "::1", "::ffff:127.0.0.1"] {
            for _ in 0..PRE_AUTH_PER_SOURCE * 2 {
                assert!(slots.take(lo.parse().unwrap()).unwrap().is_none(), "{lo}");
            }
        }
        held.pop();
        held.push(slots.take(lan).unwrap().unwrap());
        assert!(slots.take(lan).is_err());
        drop(held);
        drop(o);
        assert_eq!(slots.len(), 0, "nothing is left counted");
    }

    /// The write buffers are bounded (tungstenite's default is unbounded), and an
    /// authenticated connection's still takes the largest message.
    #[test]
    fn write_buffers_are_bounded() {
        let pre = pre_auth_ws_config();
        assert_eq!(pre.max_write_buffer_size, PRE_AUTH_WRITE_BUFFER);
        assert!(pre.max_write_buffer_size > pre.write_buffer_size);
        assert_eq!(pre.max_message_size, Some(HELLO_MAX_BYTES));
        let full = runner_ws_config();
        assert!(full.max_write_buffer_size > MAX_FRAME_BYTES + 16);
        assert!(full.max_write_buffer_size < 2 * MAX_FRAME_BYTES);
        assert!(full.max_write_buffer_size > full.write_buffer_size);
    }

    #[test]
    fn non_loopback_needs_tls_or_the_opt_in() {
        let lo: SocketAddr = "127.0.0.1:7878".parse().unwrap();
        let lo6: SocketAddr = "[::1]:7878".parse().unwrap();
        let any: SocketAddr = "0.0.0.0:7878".parse().unwrap();
        let lan: SocketAddr = "192.168.1.5:7878".parse().unwrap();
        for a in [lo, lo6] {
            check_bind(&a, &cfg(false, false)).unwrap();
        }
        for a in [any, lan] {
            let e = check_bind(&a, &cfg(false, false)).unwrap_err();
            assert!(e.to_string().contains("tls"), "{e}");
            check_bind(&a, &cfg(true, false)).unwrap();
            check_bind(&a, &cfg(false, true)).unwrap();
        }
    }
}

//! One port, routed by the ClientHello (docs/relay-protocol.md §4, §7).

use super::config::PortHub;
use super::counters::Refusal;
use super::{api, hub, phone, Shared};
use crate::clienthello::{self, Hello, Parse};
use crate::{names, proto};
use std::net::SocketAddr;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

pub enum Kind {
    Main,
    Port(PortHub),
    /// iroh's own port, for a relay without a DNS name (§10.1).
    Iroh,
}

struct ConnGuard(Arc<Shared>);
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.0.counters.connections_open.fetch_sub(1, Relaxed);
    }
}

pub async fn accept_loop(shared: Arc<Shared>, listener: TcpListener, kind: Kind) {
    let kind = Arc::new(kind);
    loop {
        let (tcp, peer) = tokio::select! {
            _ = shared.cancel.cancelled() => return,
            r = listener.accept() => match r {
                Ok(x) => x,
                Err(e) => {
                    // EMFILE and the like: back off instead of spinning.
                    tracing::warn!("accept: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
        };
        if shared.deny.ip(peer.ip()) {
            shared.counters.refuse(Refusal::Denied);
            continue;
        }
        let open = shared.counters.connections_open.fetch_add(1, Relaxed);
        let guard = ConnGuard(shared.clone());
        if open >= i64::from(shared.cfg.limits.max_connections) || shared.draining.load(Relaxed) {
            shared.counters.refuse(Refusal::Capacity);
            continue;
        }
        shared.counters.connections_total.fetch_add(1, Relaxed);
        let _ = tcp.set_nodelay(true);
        let s = shared.clone();
        let k = kind.clone();
        tokio::spawn(async move {
            let _guard = guard;
            let cancel = s.cancel.clone();
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = handle(s, tcp, peer, &k) => {}
            }
        });
    }
}

/// Reads the ClientHello without consuming it.
async fn peek_hello(tcp: &TcpStream, deadline: Instant) -> Result<Hello, Refusal> {
    let mut buf = vec![0u8; clienthello::PEEK_BUFFER];
    let mut last = 0usize;
    let mut nap = Duration::from_millis(2);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refusal::SlowHello);
        }
        let n = match tokio::time::timeout(left, tcp.peek(&mut buf)).await {
            Err(_) => return Err(Refusal::SlowHello),
            Ok(Err(_)) | Ok(Ok(0)) => return Err(Refusal::NotTls),
            Ok(Ok(n)) => n,
        };
        match clienthello::parse(&buf[..n]) {
            Parse::Done(h) => return Ok(h),
            Parse::Invalid => return Err(Refusal::NotTls),
            Parse::TooLarge => return Err(Refusal::TooLarge),
            Parse::Incomplete if n == buf.len() => return Err(Refusal::TooLarge),
            Parse::Incomplete => {
                // `peek` returns at once while bytes are waiting, so wait for more to arrive.
                if n == last {
                    tokio::time::sleep(nap.min(left)).await;
                    nap = (nap * 2).min(Duration::from_millis(50));
                } else {
                    nap = Duration::from_millis(2);
                }
                last = n;
            }
        }
    }
}

async fn handle(shared: Arc<Shared>, tcp: TcpStream, peer: SocketAddr, kind: &Kind) {
    let hello = {
        let Some(_pending) = shared.pending.enter(peer.ip(), shared.cfg.limits.pending_per_ip) else {
            shared.counters.refuse(Refusal::Pending);
            return;
        };
        let deadline = Instant::now() + Duration::from_secs(shared.cfg.limits.hello_seconds);
        match peek_hello(&tcp, deadline).await {
            Ok(h) => h,
            Err(r) => {
                shared.counters.refuse(r);
                return;
            }
        }
    };

    if let Kind::Iroh = kind {
        if let Some(i) = &shared.iroh {
            shared.counters.iroh_connections.fetch_add(1, Relaxed);
            i.serve(tcp).await;
        }
        return;
    }

    // Rule 2: ACME validations, whatever the name.
    if hello.offers_alpn(proto::ACME_ALPN) {
        shared.counters.acme_challenges.fetch_add(1, Relaxed);
        let acceptor = tokio_rustls::TlsAcceptor::from(shared.own_tls.clone());
        let _ = tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await;
        return;
    }

    // Rule 3: a phone.
    if let Some(sni) = &hello.sni {
        if let Some((first, rest)) = sni.split_once('.') {
            if shared.suffixes.iter().any(|s| s == rest) {
                match names::parse_label(first) {
                    Some((hub, tag)) => {
                        phone::serve(shared, tcp, peer, hub.to_string(), Some(tag.to_string()), "sni").await;
                    }
                    None if Some(sni) == shared.own_host.as_ref() || Some(sni) == shared.iroh_host.as_ref() => {
                        own(shared, tcp, peer, sni.clone(), &hello).await;
                    }
                    None => shared.counters.refuse(Refusal::BadLabel),
                }
                return;
            }
        }
    }

    match (kind, &hello.sni) {
        // Rule 3 for port mode: no name on a hub's own port.
        (Kind::Port(p), None) => {
            if p.allow_unauthenticated {
                phone::serve(shared, tcp, peer, p.hub.clone(), None, "port").await;
            } else {
                shared.counters.refuse(Refusal::PortNoCredential);
            }
        }
        (Kind::Port(_), Some(_)) => shared.counters.refuse(Refusal::UnknownName),
        (_, Some(sni)) if Some(sni) == shared.iroh_host.as_ref() => {
            // Rule 4.
            if let Some(i) = &shared.iroh {
                shared.counters.iroh_connections.fetch_add(1, Relaxed);
                i.serve(tcp).await;
            }
        }
        (_, Some(sni)) if Some(sni) == shared.own_host.as_ref() => own(shared, tcp, peer, sni.clone(), &hello).await,
        (_, None) => own(shared, tcp, peer, String::new(), &hello).await,
        (_, Some(_)) => shared.counters.refuse(Refusal::UnknownName),
    }
}

/// Rule 5: the relay's own TLS, then the hub protocol or the API.
async fn own(shared: Arc<Shared>, tcp: TcpStream, peer: SocketAddr, sni: String, hello: &Hello) {
    if !sni.is_empty() && Some(&sni) == shared.iroh_host.as_ref() {
        if let Some(i) = &shared.iroh {
            i.serve(tcp).await;
        }
        return;
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(shared.own_tls.clone());
    let timeout = Duration::from_secs(shared.cfg.limits.hub_hello_seconds);
    let tls = match tokio::time::timeout(timeout, acceptor.accept(tcp)).await {
        Ok(Ok(t)) => t,
        _ => {
            shared.counters.refuse(Refusal::TlsHandshake);
            return;
        }
    };
    let alpn = tls.get_ref().1.alpn_protocol().map(|a| a.to_vec());
    let _ = hello;
    if alpn.as_deref() == Some(proto::ALPN_V1) {
        hub::serve(shared, tls, peer).await;
    } else {
        api::serve(shared, tls, peer).await;
    }
}

//! iroh's relay server for linked runners, admitting only admitted endpoints
//! (docs/relay-protocol.md §10).

use super::certs::Certs;
use super::config::Config;
use super::counters::{Counters, Refusal};
use super::stores::State;
use super::Deny;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use http::{Request, StatusCode};
use iroh_relay::server::http_server::{BytesBody, Handlers, HyperResult, RelayService, TlsConfig};
use iroh_relay::server::{Access, AccessControl, ClientRateLimit, ClientRequest, Metrics};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;

#[derive(Debug)]
struct Admission {
    state: Arc<State>,
    deny: Arc<Deny>,
    counters: Arc<Counters>,
}

impl std::fmt::Debug for Deny {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Deny")
    }
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("State")
    }
}

impl AccessControl for Admission {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        let id = *request.endpoint_id().as_bytes();
        if !self.deny.endpoints.contains(&id) && self.state.is_admitted(&id, crate::now_unix()) {
            Access::Allow
        } else {
            self.counters.iroh_denied.fetch_add(1, Relaxed);
            self.counters.refuse(Refusal::IrohDenied);
            Access::Deny { reason: Some("not admitted; see /v1/iroh/admit".into()) }
        }
    }
}

pub struct Iroh {
    service: RelayService,
    tls: Arc<rustls::ServerConfig>,
    quic: Option<iroh_relay::server::Server>,
}

fn empty(status: StatusCode, response: http::response::Builder) -> HyperResult<http::Response<BytesBody>> {
    response
        .status(status)
        .body(Box::new(http_body_util::Full::new(Bytes::new())) as BytesBody)
        .map_err(|e| Box::new(e) as _)
}

impl Iroh {
    pub async fn start(cfg: &Config, certs: &Certs, state: Arc<State>, deny: Arc<Deny>, counters: Arc<Counters>) -> Result<Self> {
        let mut handlers = Handlers::default();
        handlers.insert(
            (http::Method::GET, iroh_relay::http::RELAY_PROBE_PATH),
            Box::new(|_r: Request<hyper::body::Incoming>, b| empty(StatusCode::OK, b)),
        );
        handlers.insert((http::Method::GET, "/healthz"), Box::new(|_r: Request<hyper::body::Incoming>, b| empty(StatusCode::OK, b)));
        handlers.insert(
            (http::Method::GET, "/generate_204"),
            Box::new(|_r: Request<hyper::body::Incoming>, b| empty(StatusCode::NO_CONTENT, b)),
        );
        let rate = NonZeroU32::new(u32::try_from(cfg.limits.bytes_per_second).unwrap_or(u32::MAX)).ok_or_else(|| anyhow!("zero rate"))?;
        let burst = NonZeroU32::new(u32::try_from(cfg.limits.burst_bytes).unwrap_or(u32::MAX));
        let mut limit = ClientRateLimit::new(rate);
        limit.max_burst_bytes = burst;
        let access = Arc::new(Admission { state, deny, counters });
        let service = RelayService::new(
            handlers,
            http::HeaderMap::new(),
            Some(limit),
            iroh_relay::KeyCache::new(cfg.limits.max_admitted as usize),
            access,
            Arc::new(Metrics::default()),
        );
        let tls = certs.iroh_config();
        let quic = match cfg.iroh.quic_listen {
            Some(addr) => {
                let mut q = iroh_relay::server::QuicConfig::new(addr);
                q.server_config = Some((*certs.iroh_config()).clone());
                let mut sc = iroh_relay::server::ServerConfig::default();
                sc.quic = Some(q);
                match iroh_relay::server::Server::spawn(sc).await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!("iroh address discovery (QUIC on {addr}) didn't start: {e}");
                        None
                    }
                }
            }
            None => None,
        };
        Ok(Self { service, tls, quic })
    }

    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.quic.as_ref().and_then(|q| q.quic_addr())
    }

    pub async fn serve(&self, tcp: TcpStream) {
        self.service.clone().handle_connection(tcp, Some(TlsConfig::new(self.tls.clone())), Duration::from_secs(30)).await;
    }

    pub async fn shutdown(&self) {
        self.service.shutdown().await;
    }
}

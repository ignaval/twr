//! Authenticated remote user transport. Runner tokens are never accepted here.
//!
//! The `twr-mobile-gateway` binary of the desktop bundle and `twr gateway` both run [`run`].
use crate::remote;
pub mod identity;
pub mod routes;
pub mod settings;
use anyhow::{Context, Result};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        ConnectInfo, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use remote::{
    auth::{Auth, Device, Standing},
    companion::Companion,
    terminal::Terminal,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock, Semaphore};
use twr_core::proto::codes;

#[derive(Parser, Debug)]
#[command(
    about = "Remote mobile clients for twr. Start from your own terminal or desktop launcher."
)]
pub struct Args {
    /// Where to listen. Default: 127.0.0.1:47321 with --tls-cert or --development; else the
    /// saved settings' addresses, or 0.0.0.0:47321 and [::]:47321.
    #[arg(long)]
    listen: Option<SocketAddr>,
    /// Serve this certificate (certificate mode), with --tls-key. Without them the gateway
    /// serves its own key, which phones pin (pinned mode).
    #[arg(long, requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    tls_key: Option<PathBuf>,
    /// Pinned mode: the address invitations offer first, a DNS name or a forwarded
    /// host:port; saved for later runs. An empty one offers none.
    #[arg(long, value_name = "HOST[:PORT]", conflicts_with_all = ["tls_cert", "development"])]
    address: Option<String>,
    /// Debug builds only: plaintext loopback for adb reverse and iOS Simulator.
    #[arg(long)]
    development: bool,
    /// Print a new one-use pairing code, valid for five minutes.
    #[arg(long)]
    pair: bool,
    /// Create a code for an already running gateway, then exit.
    #[arg(long, requires = "pair")]
    pair_only: bool,
    /// Grant read access only. Cannot be combined with terminal or admin access.
    #[arg(long, requires = "pair", conflicts_with_all = ["allow_terminal", "allow_admin"])]
    read_only: bool,
    /// Emit the short-lived invitation as JSON for native launchers.
    #[arg(long = "json", requires = "pair")]
    pair_json: bool,
    /// Cancel only the matching one-use invitation. Reads {code} from stdin.
    #[arg(long)]
    cancel_invitation: bool,
    /// Desktop launcher: bounded, private operational log.
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Grant remote shells, attach, jobs and interactive CLI flows.
    #[arg(long)]
    allow_terminal: bool,
    /// Grant accounts, boxes, configuration and modes. Configuration can name commands the
    /// hub runs, so admin includes command execution and needs --allow-terminal.
    #[arg(long, requires = "allow_terminal")]
    allow_admin: bool,
    #[arg(long)]
    revoke: Option<String>,
    #[arg(long)]
    list_devices: bool,
    /// Stop only the verified running gateway instance, leaving the hub alone.
    #[arg(long, requires = "expected_instance")]
    stop_gateway: bool,
    #[arg(long, requires = "stop_gateway")]
    expected_instance: Option<String>,
    /// Print this installation's verified gateway identity and process metadata, then exit.
    #[arg(long)]
    gateway_info: bool,
    #[arg(long)]
    binary_dir: Option<PathBuf>,
    #[arg(long)]
    push_config: Option<PathBuf>,
    #[arg(long)]
    push_test_log: Option<PathBuf>,
}
fn pairing_scopes(args: &Args) -> Vec<&'static str> {
    let mut scopes = vec!["read"];
    if !args.read_only {
        scopes.push("operate");
    }
    if args.allow_terminal {
        scopes.push("terminal");
    }
    if args.allow_admin {
        scopes.push("admin");
    }
    scopes
}
struct Server {
    /// Pairing attempts, per peer under a global ceiling.
    pairing: remote::limits::RateLimit,
    /// Socket upgrades refused as unknown tokens, per peer.
    refused_upgrades: remote::limits::RateLimit,
    /// Connections that are not yet an authenticated device's socket.
    connections: Arc<remote::limits::Connections>,
    terminal_slots: Arc<Semaphore>,
    binary: PathBuf,
    companion: remote::companion::Command,
    owner: Mutex<Option<twr_core::client::Client>>,
    auth: Arc<Auth>,
    slots: Arc<Semaphore>,
    development: bool,
    sessions: Mutex<HashMap<String, Arc<DeviceSession>>>,
    push: Arc<remote::push::Push>,
    observers: Mutex<std::collections::HashSet<String>>,
    receipts: remote::receipts::Receipts,
    receipt_waiters: Mutex<HashMap<String, tokio::sync::watch::Receiver<Option<Value>>>>,
    /// twr's natural voices, made here for phones that read aloud with one.
    renderer: Arc<remote::voice::Renderer>,
    /// Pinned mode: the pin it serves and where its routes come from.
    pinned: Option<Pinned>,
}
/// What a pinned gateway serves beside its key: the pin, and what its routes come from.
pub(crate) struct Pinned {
    pin: String,
    /// The gateway's directory, for its saved settings.
    dir: PathBuf,
    /// Its listeners, as bound.
    listeners: Vec<SocketAddr>,
}
impl Pinned {
    /// The routes it offers now ([`routes::current`]).
    fn routes(&self) -> Vec<String> {
        routes::current(&self.dir, &self.listeners)
            .iter()
            .map(ToString::to_string)
            .collect()
    }
}
type OrderedInput = (Call, mpsc::Sender<Value>, u64);
fn stamp(mut event: Value, generation: u64) -> Value {
    event["_bridge_generation"] = json!(generation);
    event
}
fn event_generation(event: &Value) -> Option<u64> {
    event["_bridge_generation"].as_u64()
}
fn outbound(mut event: Value, fragmented: bool) -> remote::wire::Outbound {
    let generation = event
        .as_object_mut()
        .and_then(|o| o.remove("_bridge_generation"))
        .and_then(|v| v.as_u64());
    remote::wire::Outbound::new(socket_message(&event), generation, fragmented)
}

fn authorize_mutation(device: &Device, call: &Call) -> Result<()> {
    let allowed = match call.method.as_str() {
        "rpc" => device.allows_params(
            call.params["method"].as_str().unwrap_or(""),
            &call.params["params"],
        ),
        "config-save" | "config-set" => device.scopes.iter().any(|s| s == "admin"),
        "terminal-open" => device.scopes.iter().any(|s| s == "terminal"),
        "cli" => device.allows("thread.new"),
        _ => false,
    };
    anyhow::ensure!(
        allowed,
        "This device does not have permission for this operation"
    );
    Ok(())
}

pub(crate) const MAX_SOCKET_REPLY: usize = 16 * 1024 * 1024;
pub(crate) const OVERSIZED_REPLY_ERROR: &str =
    "Hub response exceeds 16 MiB. Load a smaller transcript page or open it on desktop.";

/// Keep a large transcript or state event from tearing down a healthy native socket.
fn send_budget(message: &Message) -> Duration {
    let bytes = match message {
        Message::Text(text) => text.len(),
        Message::Binary(data) => data.len(),
        _ => 0,
    };
    Duration::from_secs(20 + (bytes as u64).div_ceil(32 * 1024))
}
pub(crate) fn socket_message(event: &Value) -> Message {
    let text = event.to_string();
    let text = if text.len() > MAX_SOCKET_REPLY {
        match event.get("id") {
            Some(id) => json!({"id":id,"error":OVERSIZED_REPLY_ERROR}).to_string(),
            None => json!({"type":"error","error":OVERSIZED_REPLY_ERROR}).to_string(),
        }
    } else {
        text
    };
    Message::Text(text.into())
}
#[cfg(test)]
mod socket_tests {
    use super::*;
    #[test]
    fn launcher_pairing_flags_cannot_expand_read_only_invitations() {
        let args =
            Args::try_parse_from(["gateway", "--pair", "--pair-only", "--json", "--read-only"])
                .unwrap();
        assert_eq!(pairing_scopes(&args), vec!["read"]);
        assert!(
            Args::try_parse_from(["gateway", "--pair", "--read-only", "--allow-terminal"]).is_err()
        );
        assert!(
            Args::try_parse_from(["gateway", "--pair", "--read-only", "--allow-admin"]).is_err()
        );
        assert!(Args::try_parse_from(["gateway", "--json"]).is_err());
        assert!(Args::try_parse_from(["gateway", "--read-only"]).is_err());
        let args = Args::try_parse_from(["gateway", "--pair", "--allow-terminal"]).unwrap();
        assert_eq!(pairing_scopes(&args), vec!["read", "operate", "terminal"]);
        // Admin can write configuration that names commands: it comes with terminal only.
        let error = Args::try_parse_from(["gateway", "--pair", "--allow-admin"])
            .err()
            .unwrap();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert!(error.to_string().contains("--allow-terminal"));
        let args = Args::try_parse_from(["gateway", "--pair", "--allow-admin", "--allow-terminal"])
            .unwrap();
        assert_eq!(
            pairing_scopes(&args),
            vec!["read", "operate", "terminal", "admin"]
        );
    }
    fn bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }
    #[test]
    fn only_an_unknown_token_is_refused_as_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let auth = Auth::open(dir.path()).unwrap();
        let code = auth.begin(vec!["read".into()]).unwrap();
        let (device, token) = auth.pair(&code, "phone").unwrap();
        let (found, presented) = authorize_upgrade(&auth, &bearer(&token), false).unwrap();
        assert_eq!(
            (found.id.as_str(), presented.as_str()),
            (device.id.as_str(), token.as_str())
        );
        for unknown in ["", "short", &"0".repeat(64)] {
            assert_eq!(
                authorize_upgrade(&auth, &bearer(unknown), false).err(),
                Some(StatusCode::UNAUTHORIZED)
            );
        }
        let mut foreign = bearer(&token);
        foreign.insert("origin", "https://example.com".parse().unwrap());
        assert_eq!(
            authorize_upgrade(&auth, &foreign, false).err(),
            Some(StatusCode::FORBIDDEN)
        );
        // A device list that cannot be read is no revocation: 503, and the session stays.
        let path = dir.path().join("devices.json");
        let saved = std::fs::read(&path).unwrap();
        std::fs::write(&path, b"{\"devices\":[").unwrap();
        for token in [token.as_str(), &"0".repeat(64)] {
            assert_eq!(
                authorize_upgrade(&auth, &bearer(token), false).err(),
                Some(StatusCode::SERVICE_UNAVAILABLE)
            );
        }
        assert_eq!(auth.current(&device), Standing::Unknown);
        let paired = paired_ids(&auth);
        assert!(paired.is_none());
        assert!(keep_session(&device.id, 100, paired.as_ref(), 200));
        assert!(!keep_session(&device.id, 100, paired.as_ref(), 100 + 1800));
        std::fs::write(&path, saved).unwrap();
        let paired = paired_ids(&auth);
        assert!(keep_session(&device.id, 100, paired.as_ref(), 200));
        auth.revoke(&device.id).unwrap();
        let paired = paired_ids(&auth);
        assert!(!keep_session(&device.id, 100, paired.as_ref(), 200));
        assert_eq!(
            authorize_upgrade(&auth, &bearer(&token), false).err(),
            Some(StatusCode::UNAUTHORIZED)
        );
    }
    const TEST_LIMITS: remote::limits::Limits = remote::limits::Limits {
        total: 128,
        per_peer: 16,
        first_byte: Duration::from_secs(1),
        lifetime: Duration::from_secs(4),
        header_read: Duration::from_secs(1),
    };
    /// A gateway with no hub behind it: nothing here may reach `owner_call`, which would
    /// connect to this machine's own hub.
    fn test_server(dir: &Path) -> Shared {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Arc::new(Server::new(
            Auth::open(dir).unwrap(),
            remote::push::Push::new(dir, None, None).unwrap(),
            remote::receipts::Receipts::new(dir).unwrap(),
            None,
            (
                dir.join("twr"),
                remote::companion::Command::program(dir.join("twr-desktop-bridge")),
            ),
            true,
            TEST_LIMITS,
            None,
        ))
    }
    async fn spawn_gateway(
        server: Shared,
        tls: Option<axum_server::tls_rustls::RustlsConfig>,
    ) -> SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let connections = server.connections.clone();
        let app = router(server).into_make_service_with_connect_info::<SocketAddr>();
        match tls {
            None => {
                let http = http_server(listener, axum_server::accept::DefaultAcceptor, connections)
                    .unwrap();
                tokio::spawn(http.serve(app));
            }
            Some(config) => {
                let tls = axum_server::tls_rustls::RustlsAcceptor::new(config);
                tokio::spawn(http_server(listener, tls, connections).unwrap().serve(app));
            }
        }
        address
    }
    /// Read until the gateway closes the connection; how long that took and what it sent.
    async fn until_closed(stream: &mut tokio::net::TcpStream) -> (Duration, String) {
        use tokio::io::AsyncReadExt;
        let started = std::time::Instant::now();
        let mut received = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            let mut buffer = [0; 4096];
            while let Ok(read @ 1..) = stream.read(&mut buffer).await {
                received.extend_from_slice(&buffer[..read]);
            }
        })
        .await;
        (
            started.elapsed(),
            String::from_utf8_lossy(&received).into_owned(),
        )
    }
    async fn status_of(address: SocketAddr, request: &str) -> String {
        use tokio::io::AsyncWriteExt;
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let (_, response) = until_closed(&mut stream).await;
        response.lines().next().unwrap_or("").to_owned()
    }
    #[tokio::test]
    async fn unauthenticated_connections_cannot_linger_or_crowd_out_others() {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let address = spawn_gateway(test_server(dir.path()), None).await;
        let connect = || tokio::net::TcpStream::connect(address);
        // Silent: closed at the first-byte deadline, long before its lifetime.
        let (waited, _) = until_closed(&mut connect().await.unwrap()).await;
        assert!(waited >= Duration::from_millis(900) && waited < Duration::from_secs(3));
        // A request head that never ends: hyper's header read timeout.
        let mut partial = connect().await.unwrap();
        partial
            .write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n")
            .await
            .unwrap();
        let (waited, _) = until_closed(&mut partial).await;
        assert!(waited < Duration::from_secs(3), "{waited:?}");
        // A finished request is answered; the idle keep-alive connection then closes.
        let mut idle = connect().await.unwrap();
        idle.write_all(b"GET /health HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let (waited, response) = until_closed(&mut idle).await;
        assert!(response.starts_with("HTTP/1.1 200") && response.contains("twr-mobile"));
        assert!(waited < Duration::from_secs(3), "{waited:?}");
        // One peer holds at most 16; the 17th is closed on arrival.
        let mut held = Vec::new();
        for _ in 0..16 {
            held.push(connect().await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (waited, response) = until_closed(&mut connect().await.unwrap()).await;
        assert!(response.is_empty() && waited < Duration::from_millis(800));
        drop(held);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let health = status_of(address, "GET /health HTTP/1.1\r\nConnection: close\r\n\r\n");
        assert_eq!(health.await, "HTTP/1.1 200 OK");
    }
    #[tokio::test]
    async fn a_silent_tls_connection_is_closed_after_its_handshake() {
        use rustls::pki_types::{pem::PemObject, CertificateDer};
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../twr-hub/tests/fixtures/tls");
        let dir = tempfile::tempdir().unwrap();
        let server = test_server(dir.path());
        let config = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            fixtures.join("hub.pem"),
            fixtures.join("hub.key"),
        )
        .await
        .unwrap();
        let address = spawn_gateway(server, Some(config)).await;
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_file(fixtures.join("ca.pem")).unwrap())
            .unwrap();
        let client = Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let (handshake, closed) = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let mut tcp = std::net::TcpStream::connect(address).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let name = "localhost".try_into().unwrap();
            let mut tls = rustls::ClientConnection::new(client, name).unwrap();
            while tls.is_handshaking() {
                tls.complete_io(&mut tcp).unwrap();
            }
            let handshake = std::time::Instant::now();
            let mut stream = rustls::Stream::new(&mut tls, &mut tcp);
            let read = stream.read(&mut [0; 64]);
            assert!(
                !matches!(&read, Ok(1..)),
                "the gateway sent data unasked: {read:?}"
            );
            assert!(
                !matches!(&read, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
                "still open after 10 seconds"
            );
            (handshake, std::time::Instant::now())
        })
        .await
        .unwrap();
        let waited = closed - handshake;
        assert!(waited < Duration::from_secs(3), "{waited:?}");
    }
    #[tokio::test]
    async fn guessing_tokens_is_slowed_per_peer_and_read_errors_count_for_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let address = spawn_gateway(test_server(dir.path()), None).await;
        let upgrade = format!(
            "GET /v1/socket HTTP/1.1\r\nHost: x\r\nConnection: Upgrade, close\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nAuthorization: Bearer {}\r\n\r\n",
            "0".repeat(64)
        );
        let devices = dir.path().join("devices.json");
        std::fs::write(&devices, b"{\"devices\":[").unwrap();
        std::fs::set_permissions(&devices, std::fs::Permissions::from_mode(0o600)).unwrap();
        for _ in 0..REFUSED_UPGRADES_PER_PEER + 5 {
            assert_eq!(
                status_of(address, &upgrade).await,
                "HTTP/1.1 503 Service Unavailable"
            );
        }
        std::fs::remove_file(&devices).unwrap();
        for _ in 0..REFUSED_UPGRADES_PER_PEER {
            assert_eq!(
                status_of(address, &upgrade).await,
                "HTTP/1.1 401 Unauthorized"
            );
        }
        assert_eq!(
            status_of(address, &upgrade).await,
            "HTTP/1.1 429 Too Many Requests"
        );
    }
    #[tokio::test]
    async fn one_peer_using_up_pairing_attempts_leaves_others_theirs() {
        let dir = tempfile::tempdir().unwrap();
        let server = test_server(dir.path());
        let attempt = |peer: &str| {
            let server = server.clone();
            let peer: SocketAddr = peer.parse().unwrap();
            async move {
                // Malformed, so that no attempt reaches the hub.
                let junk = Pair {
                    code: "x".into(),
                    name: "phone".into(),
                };
                pair(State(server), ConnectInfo(peer), Json(junk))
                    .await
                    .into_response()
                    .status()
            }
        };
        for port in 0..PAIRING_PER_PEER {
            let peer = format!("[2001:db8::{port}]:4000");
            assert_eq!(attempt(&peer).await, StatusCode::BAD_REQUEST);
        }
        // One /64 is one peer.
        assert_eq!(
            attempt("[2001:db8::ffff]:1").await,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(attempt("192.0.2.7:1").await, StatusCode::BAD_REQUEST);
        assert_eq!(attempt("[2001:db8:1::1]:1").await, StatusCode::BAD_REQUEST);
    }
    /// A hub that answers a gateway's owner calls from the device store in `dir`, as the hub
    /// does: `hello` as the user, `device.pair_proof` from the open invitation.
    async fn proof_hub(dir: &Path) -> twr_core::client::Client {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use twr_core::phone_pairing::{b64url_decode, b64url_encode, decode_pin};
        let path = dir.join("hub.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let store = dir.to_owned();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let store = store.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = socket.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let (id, p) = (&request["id"], &request["params"]);
                        let reply = match request["method"].as_str().unwrap() {
                            "hello" => json!({"id":id,"result":{"identity":"user:local"}}),
                            "device.pair_proof" => {
                                let pin = decode_pin(p["pin"].as_str().unwrap()).unwrap();
                                let nonce = b64url_decode(p["nonce"].as_str().unwrap()).unwrap();
                                let gateway = p["gateway_id"].as_str().unwrap();
                                match Auth::open(&store).unwrap().proof(&pin, &nonce, gateway) {
                                    Ok(Some(proof)) => {
                                        json!({"id":id,"result":{"proof":b64url_encode(&proof)}})
                                    }
                                    _ => {
                                        json!({"id":id,"error":{"code":"not_found","message":"none open"}})
                                    }
                                }
                            }
                            method => panic!("the gateway asked the hub for {method}"),
                        };
                        writer
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                });
            }
        });
        let mut client = twr_core::client::Client::connect(&path).await.unwrap();
        client
            .hello(json!({"client":"mobile-gateway"}))
            .await
            .unwrap();
        client
    }
    /// A pinned gateway over the device store in `dir`, with [`proof_hub`] as its hub.
    async fn pinned_server(dir: &Path) -> (Shared, identity::Identity) {
        use std::os::unix::fs::PermissionsExt;
        let _ = rustls::crypto::ring::default_provider().install_default();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let identity = identity::Identity::ensure(dir).unwrap();
        let server = Arc::new(Server::new(
            Auth::open(dir).unwrap(),
            remote::push::Push::new(dir, None, None).unwrap(),
            remote::receipts::Receipts::new(dir).unwrap(),
            Some(proof_hub(dir).await),
            (
                dir.join("twr"),
                remote::companion::Command::program(dir.join("twr-desktop-bridge")),
            ),
            false,
            TEST_LIMITS,
            Some(Pinned {
                pin: identity.pin.clone(),
                dir: dir.to_owned(),
                listeners: vec!["127.0.0.1:47321".parse().unwrap()],
            }),
        ));
        (server, identity)
    }
    async fn ask_proof(server: &Shared, peer: &str, nonce: &str) -> (StatusCode, Value) {
        let query = Ok(axum::extract::Query(ProofQuery {
            nonce: nonce.into(),
        }));
        let peer: SocketAddr = peer.parse().unwrap();
        let response = pair_proof(State(server.clone()), ConnectInfo(peer), query)
            .await
            .into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }
    /// `/v1/pair/proof`: 404 with no invitation (or no pin), 400 for a nonce that is not 32
    /// bytes of base64url, else a proof that checks against the code's key, this gateway's pin
    /// and id; within `/v1/pair`'s rate limit, which the two share.
    #[tokio::test]
    async fn the_proof_endpoint_proves_the_open_invitation_for_this_pin() {
        use twr_core::phone_pairing::{b64url_decode, b64url_encode, proof_key, verify_pin_proof};
        let dir = tempfile::tempdir().unwrap();
        let (server, identity) = pinned_server(dir.path()).await;
        let nonce = [3u8; 32];
        let encoded = b64url_encode(&nonce);
        let (status, _) = ask_proof(&server, "192.0.2.1:1", &encoded).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        for bad in [
            b64url_encode(&[3u8; 16]),
            "not base64!".into(),
            String::new(),
            "A".repeat(80),
        ] {
            let (status, _) = ask_proof(&server, "192.0.2.2:1", &bad).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
        }
        let code = server.auth.begin(vec!["read".into()]).unwrap();
        let (status, body) = ask_proof(&server, "192.0.2.3:1", &encoded).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let proof = b64url_decode(body["proof"].as_str().unwrap()).unwrap();
        let key = proof_key(&code).unwrap();
        let gateway = server.push.gateway_id.as_str();
        assert!(verify_pin_proof(
            &key,
            &identity.pin_raw,
            &nonce,
            gateway,
            &proof
        ));
        let other = twr_core::phone_pairing::pin_bytes(b"another key");
        assert!(!verify_pin_proof(&key, &other, &nonce, gateway, &proof));
        // Asking was no attempt at the code: it still pairs.
        assert!(server.auth.pair(&code, "phone").is_ok());
        let (status, _) = ask_proof(&server, "192.0.2.3:1", &encoded).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // One peer's attempts, proofs and pairings together, run out.
        let peer = "192.0.2.9:1";
        for _ in 0..PAIRING_PER_PEER - 1 {
            assert_eq!(
                ask_proof(&server, peer, "x").await.0,
                StatusCode::BAD_REQUEST
            );
        }
        let junk = Pair {
            code: "x".into(),
            name: "phone".into(),
        };
        let paired = pair(
            State(server.clone()),
            ConnectInfo(peer.parse().unwrap()),
            Json(junk),
        );
        assert_eq!(
            paired.await.into_response().status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ask_proof(&server, peer, &encoded).await.0,
            StatusCode::TOO_MANY_REQUESTS
        );
        // A gateway without a pin proves nothing.
        let plain = tempfile::tempdir().unwrap();
        let server = test_server(plain.path());
        server.auth.begin(vec!["read".into()]).unwrap();
        assert_eq!(
            ask_proof(&server, "192.0.2.1:1", &encoded).await.0,
            StatusCode::NOT_FOUND
        );
    }
    /// `/health` says protocol 2, and the pin in pinned mode only.
    #[tokio::test]
    async fn health_carries_the_pin_in_pinned_mode() {
        let dir = tempfile::tempdir().unwrap();
        let (server, identity) = pinned_server(dir.path()).await;
        let Json(pinned) = health(State(server.clone())).await;
        assert_eq!(pinned["protocol"], 2);
        assert_eq!(pinned["service"], "twr-mobile");
        assert_eq!(pinned["pin"], identity.pin.as_str());
        assert_eq!(pinned["gateway_id"], server.push.gateway_id.as_str());
        let plain = tempfile::tempdir().unwrap();
        let Json(plain) = health(State(test_server(plain.path()))).await;
        assert_eq!(plain["protocol"], 2);
        assert!(plain.get("pin").is_none());
    }
    /// `--pair --json` in pinned mode: today's fields, the version 2 invitation and its link;
    /// in certificate mode today's fields alone.
    #[test]
    fn pinned_pair_json_carries_the_invitation_and_its_link() {
        let gateway = "8b0f5c7e-2a44-4b8e-9d21-6a4e1c3f7b90";
        let plain = pair_json("ABCDEFGHJK23", 1_763_900_000, &["read"], gateway, None).unwrap();
        assert_eq!(
            plain,
            json!({"code":"ABCDEFGHJK23","expires_at":1_763_900_000u64,"scopes":["read"],"gateway_id":gateway})
        );
        let pinned = PinnedInvitation {
            pin: twr_core::phone_pairing::pin(b"spki"),
            iroh: "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6".into(),
            routes: vec![
                twr_core::phone_pairing::Route::parse("tls:192.168.1.20:47321").unwrap(),
                twr_core::phone_pairing::Route::parse("tls:[2001:db8::5]:47321").unwrap(),
            ],
        };
        let scopes = ["read", "operate"];
        let line = pair_json(
            "ABCDEFGHJK23",
            1_763_900_000,
            &scopes,
            gateway,
            Some(&pinned),
        )
        .unwrap();
        let keys: Vec<&str> = line
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "code",
                "expires_at",
                "gateway_id",
                "invitation",
                "link",
                "scopes"
            ]
        );
        let invitation = &line["invitation"];
        assert_eq!(invitation["type"], "twr-pair");
        assert_eq!(invitation["version"], 2);
        assert_eq!(invitation["id"], gateway);
        assert_eq!(invitation["code"], "ABCDEFGHJK23");
        assert_eq!(invitation["expiresAt"], 1_763_900_000_000u64);
        assert_eq!(invitation["pin"], pinned.pin.as_str());
        assert_eq!(invitation["iroh"], pinned.iroh.as_str());
        assert_eq!(
            invitation["routes"],
            json!(["tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321"])
        );
        assert_eq!(invitation["scopes"], json!(scopes));
        let link = line["link"].as_str().unwrap();
        let parsed = twr_core::phone_pairing::Invitation::parse(link).unwrap();
        assert_eq!(serde_json::to_value(&parsed).unwrap(), *invitation);
        // No route to offer: no invitation.
        let nowhere = PinnedInvitation {
            routes: Vec::new(),
            ..pinned
        };
        let error = pair_json("ABCDEFGHJK23", 1, &scopes, gateway, Some(&nowhere)).unwrap_err();
        assert!(error.to_string().contains("--address"), "{error}");
    }
    #[test]
    fn stopped_inbox_sources_require_a_typed_socket_error() {
        for kind in [
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::ConnectionRefused,
        ] {
            assert!(inbox_socket_stopped(
                &anyhow::Error::new(std::io::Error::new(kind, "socket"))
                    .context("connecting workspace")
            ));
        }
        assert!(!inbox_socket_stopped(&anyhow::anyhow!("No such file")));
        assert!(!inbox_socket_stopped(&anyhow::Error::new(
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "socket")
        )));
    }
    #[tokio::test]
    async fn inbox_deadline_returns_partial_results_and_drops_unfinished_readers() {
        let names: Vec<String> = (0..64)
            .map(|number| format!("workspace-{number}"))
            .collect();
        let alive = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        struct Reader(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Reader {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }
        let stream = futures::stream::iter(names.clone()).map(|workspace| {
            let alive = alive.clone();
            async move {
                alive.fetch_add(1, Ordering::SeqCst);
                let _reader = Reader(alive);
                if workspace != "workspace-0" { std::future::pending::<()>().await; }
                json!({"workspace":workspace,"attention":[{"attn_id":"ready","revision":7}],"threads":[]})
            }
        }).buffer_unordered(4);
        let started = tokio::time::Instant::now();
        let sources =
            collect_inbox_sources(stream, names, "test-gateway", Duration::from_millis(20)).await;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(sources.len(), 64);
        assert_eq!(sources[0]["attention"][0]["attn_id"], "ready");
        assert!(sources[1..].iter().all(|source| {
            source["cached"].as_bool() != Some(true)
                && source["attention"].as_array().unwrap().is_empty()
                && source["error"].as_str().unwrap().contains("deadline")
        }));
        assert_eq!(
            alive.load(Ordering::SeqCst),
            0,
            "unfinished socket readers must be cancelled"
        );
    }
    #[tokio::test]
    async fn inbox_reads_prove_device_and_workspace_without_switching_or_writing() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        for (identity, workspace, expected_calls) in [
            ("user:device:reader", "review", 4),
            ("user:local-owner", "review", 1),
            ("user:device:reader", "other", 2),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("hub.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let (reader, mut writer) = socket.into_split();
                let mut lines = BufReader::new(reader).lines();
                let mut methods = Vec::new();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let method = request["method"].as_str().unwrap();
                    methods.push(method.to_owned());
                    let result = match method {
                        "hello" => {
                            assert_eq!(request["params"]["device_token"], "device-secret");
                            json!({"identity":identity})
                        }
                        "hub.info" => json!({"workspace":workspace}),
                        "attention.list" => json!([{"attn_id":"permission","revision":8}]),
                        "thread.list" => json!([{"id":"worker"}]),
                        _ => panic!("inbox attempted a non-read operation: {method}"),
                    };
                    let line = json!({"id":request["id"],"result":result}).to_string() + "\n";
                    writer.write_all(line.as_bytes()).await.unwrap();
                }
                methods
            });
            let device = Device {
                id: "reader".into(),
                name: "Reader".into(),
                created: 0,
                token_hash: String::new(),
                scopes: vec!["read".into()],
            };
            let result = read_inbox_workspace(&path, "review", &device, "device-secret").await;
            assert_eq!(result.is_ok(), expected_calls == 4);
            let calls = server.await.unwrap();
            assert_eq!(calls.len(), expected_calls);
            assert_eq!(calls[0], "hello");
        }
    }
    #[test]
    fn stalled_uploads_cannot_exhaust_approval_request_slots() {
        let uploads = Arc::new(Semaphore::new(4));
        let general = Arc::new(Semaphore::new(64));
        let permits = (0..4)
            .map(|_| {
                operation_slots("upload-commit", &uploads, &general)
                    .try_acquire_owned()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert!(operation_slots("upload-begin", &uploads, &general)
            .try_acquire_owned()
            .is_err());
        assert!(operation_slots("rpc", &uploads, &general)
            .try_acquire_owned()
            .is_ok());
        assert_eq!(general.available_permits(), 64);
        drop(permits);
    }
    #[test]
    fn denied_and_unknown_rpc_calls_cannot_claim_receipts() {
        let mut device = Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: "read only".into(),
            created: 0,
            token_hash: String::new(),
            scopes: vec!["read".into()],
        };
        let call = Call {
            id: uuid::Uuid::new_v4().to_string(),
            method: "rpc".into(),
            params: json!({"method":"thread.send","params":{}}),
            workspace: None,
        };
        assert!(authorize_mutation(&device, &call).is_err());
        device.scopes.push("operate".into());
        assert!(authorize_mutation(&device, &call).is_ok());
        let unknown = Call {
            params: json!({"method":"unknown"}),
            ..call
        };
        assert!(authorize_mutation(&device, &unknown).is_err());
    }
    /// The `cli` call runs the twr CLI as the hub's user, so a device runs through it only the
    /// command lines its scope allows, spelled out: `ws list` for read, `ws new <name>` too for
    /// operate and wider. Every other subcommand, flag or argument is refused whatever the
    /// scope, a command line runtime, a mode change, a runtime move, another socket or
    /// workspace, and the CLI's file-writing commands among them.
    #[test]
    fn a_device_runs_only_its_scope_s_command_lines_through_cli() {
        let device = |scopes: &[&str]| Device {
            id: uuid::Uuid::new_v4().to_string(),
            name: "phone".into(),
            created: 0,
            token_hash: String::new(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        };
        let read = device(&["read"]);
        let operate = device(&["read", "operate"]);
        let terminal = device(&["read", "operate", "terminal"]);
        let admin = device(&["read", "operate", "terminal", "admin"]);
        let line = |s: &[&str]| s.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        for d in [&read, &operate, &terminal, &admin] {
            assert!(device_cli_args(d, &line(&["ws", "list"])).is_ok());
            assert!(device_cli_args(d, &line(&["ws", "list", "--json"])).is_ok());
        }
        let refused = device_cli_args(&read, &line(&["ws", "new", "work"])).unwrap_err();
        assert!(refused.to_string().contains("read-only"), "{refused}");
        for d in [&operate, &terminal, &admin] {
            assert!(device_cli_args(d, &line(&["ws", "new", "work"])).is_ok());
            assert!(device_cli_args(d, &line(&["ws", "new", "a-2"])).is_ok());
        }
        let refusals: &[&[&str]] = &[
            &[],
            &["ws"],
            &["ws", "ls"],
            &["ws", "list", "--json", "--json"],
            // What each hub lists, read with the gateway's own user authority: never a device's.
            &["ws", "list", "--json", "--detail"],
            &["ws", "list", "--detail", "--json"],
            &["ws", "list", "extra"],
            &["ws", "rm", "work"],
            &["ws", "rm", "work", "--yes"],
            &["ws", "new"],
            &["ws", "new", "a", "b"],
            &["ws", "new", "Work"],
            &["ws", "new", "../work"],
            &["ws", "new", "-work"],
            &["ws", "new", "--help"],
            &["ws", "new", "--", "work"],
            &["ws", "new", "work", "--socket", "/tmp/other.sock"],
            &["ws", "new", "work", "--socket=/tmp/other.sock"],
            &["ws", "list", "-w", "other"],
            &["ws", "list", "-wother"],
            &["ws", "list", "--workspace=other"],
            &["-w", "other", "ws", "list"],
            &["--socket", "/tmp/other.sock", "ws", "list"],
            &["--help"],
            &["--version"],
            &["thread", "new", "--runtime", "pty:/bin/sh"],
            &["thread", "new", "--runtime", "acp:/usr/bin/agent"],
            &["mode", "api", "bypass"],
            &["model", "api", "--runtime", "codex"],
            &["orchestrator", "use", "acp:/bin/true"],
            &["box", "add", "x", "--command", "sh"],
            &["context", "set", "api", "--file", "/etc/passwd"],
            &["memory", "edit"],
            &["job", "run", "--", "sh", "-c", "id"],
            &["attach", "api"],
            &["hub", "stop"],
            &["account", "add", "claude-code", "work"],
            &["say", "set api to bypass"],
        ];
        for args in refusals {
            for d in [&read, &operate, &terminal, &admin] {
                assert!(
                    device_cli_args(d, &line(args)).is_err(),
                    "{args:?} for {:?}",
                    d.scopes
                );
            }
        }
    }
    #[test]
    fn large_frames_get_a_slow_link_transfer_budget() {
        let frame = socket_message(&json!({"id":"large","result":"x".repeat(4*1024*1024)}));
        assert!(send_budget(&frame) >= Duration::from_secs(148));
        assert!(
            send_budget(&socket_message(&json!({"id":"small","result":true})))
                <= Duration::from_secs(21)
        );
    }
    #[test]
    fn oversized_replies_and_events_become_bounded_errors() {
        for value in [
            json!({"id":"request-id","result":"界".repeat(MAX_SOCKET_REPLY / 3 + 1)}),
            json!({"type":"state","state":"x".repeat(MAX_SOCKET_REPLY)}),
        ] {
            let Message::Text(text) = socket_message(&value) else {
                panic!("text frame required")
            };
            assert!(text.len() < MAX_SOCKET_REPLY);
            let result: Value = serde_json::from_str(&text).unwrap();
            assert!(result["error"].as_str().unwrap().contains("16 MiB"));
            if value.get("id").is_some() {
                assert_eq!(result["id"], value["id"]);
            }
        }
        let small = json!({"id":"next","result":{"ok":true}});
        let Message::Text(text) = socket_message(&small) else {
            panic!("text frame required")
        };
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), small);
    }
}
async fn owner_client() -> Result<twr_core::client::Client> {
    // A unit test's gateway has a hub of its own or none: never this machine's.
    if cfg!(test) {
        anyhow::bail!("a gateway in a unit test never connects to this machine's hub");
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
        let config = twr_core::config::Config::load_from(&paths)?;
        let mut client = twr_core::client::Client::connect(&config.socket_path(&paths)).await?;
        let hello = client.hello(json!({"client":"mobile-gateway"})).await?;
        anyhow::ensure!(
            hello["identity"]
                .as_str()
                .is_some_and(|id| id.starts_with("user:")),
            "Launch the gateway from your terminal or desktop launcher"
        );
        Ok(client)
    })
    .await
    .context("The hub did not authenticate the gateway within 30 seconds")?
}
/// Pairing attempts a minute from one peer, and from everyone.
const PAIRING_PER_PEER: u32 = 10;
const PAIRING_TOTAL: u32 = 60;
/// Socket upgrades refused as unknown tokens a minute from one peer before it is told to wait.
const REFUSED_UPGRADES_PER_PEER: u32 = 30;
/// How long `/v1/pair` may take, reading its body included.
const PAIR_DEADLINE: Duration = Duration::from_secs(35);
impl Server {
    #[allow(clippy::too_many_arguments)]
    fn new(
        auth: Auth,
        push: Arc<remote::push::Push>,
        receipts: remote::receipts::Receipts,
        owner: Option<twr_core::client::Client>,
        (binary, companion): (PathBuf, remote::companion::Command),
        development: bool,
        limits: remote::limits::Limits,
        pinned: Option<Pinned>,
    ) -> Self {
        let minute = Duration::from_secs(60);
        Self {
            pairing: remote::limits::RateLimit::new(minute, PAIRING_PER_PEER, PAIRING_TOTAL),
            // The global figure only bounds how many peers are remembered.
            refused_upgrades: remote::limits::RateLimit::new(
                minute,
                REFUSED_UPGRADES_PER_PEER,
                16 * 1024,
            ),
            connections: remote::limits::Connections::new(limits),
            terminal_slots: Arc::new(Semaphore::new(8)),
            renderer: remote::voice::Renderer::new(binary.clone()),
            binary,
            companion,
            owner: Mutex::new(owner),
            auth: Arc::new(auth),
            slots: Arc::new(Semaphore::new(32)),
            development,
            sessions: Mutex::new(HashMap::new()),
            push,
            observers: Mutex::new(std::collections::HashSet::new()),
            receipts,
            receipt_waiters: Mutex::new(HashMap::new()),
            pinned,
        }
    }
    async fn owner_call(&self, method: &str, params: Value) -> Result<Value> {
        let mut owner = self
            .owner
            .try_lock()
            .context("The hub is busy with another device operation; try again")?;
        for attempt in 0..2 {
            if owner.is_none() {
                *owner = Some(owner_client().await?);
            }
            let result = tokio::time::timeout(
                Duration::from_secs(30),
                owner.as_mut().unwrap().call(method, params.clone()),
            )
            .await;
            let result = match result {
                Ok(result) => result,
                Err(_) => {
                    *owner = None;
                    anyhow::bail!("The hub did not answer within 30 seconds; the operation may have completed")
                }
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let retry = error
                        .downcast_ref::<twr_core::client::HubClosed>()
                        .is_some_and(twr_core::client::HubClosed::resendable);
                    if error.downcast_ref::<twr_core::proto::RpcError>().is_none() {
                        *owner = None;
                    }
                    if attempt == 0 && retry {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        unreachable!()
    }
}
struct DeviceSession {
    upload_slots: Arc<Semaphore>,
    input: mpsc::Sender<OrderedInput>,
    session: Session,
    events: remote::events::Sender,
    terminal_events: broadcast::Sender<Value>,
    latest_state: Arc<std::sync::Mutex<Option<Value>>>,
    generation: AtomicU64,
    last_seen: AtomicU64,
    foreground: AtomicBool,
}
type Shared = Arc<Server>;
/// The paired device ids, or `None` when the list cannot be read.
fn paired_ids(auth: &Auth) -> Option<std::collections::HashSet<String>> {
    match auth.devices() {
        Ok(devices) => Some(
            devices
                .as_array()?
                .iter()
                .filter_map(|d| d["id"].as_str().map(str::to_owned))
                .collect(),
        ),
        Err(error) => {
            crate::gateway_log!("Paired devices could not be read; sessions are kept: {error:#}");
            None
        }
    }
}
/// A device session (its PTYs and pending uploads) ends when its device is no longer paired
/// or it has gone unseen for 30 minutes. A device list that cannot be read (`None`) proves no
/// revocation, so it ends nothing early.
fn keep_session(
    id: &str,
    last_seen: u64,
    paired: Option<&std::collections::HashSet<String>>,
    now: u64,
) -> bool {
    paired.is_none_or(|paired| paired.contains(id)) && now.saturating_sub(last_seen) < 1800
}
fn operation_slots(
    method: &str,
    uploads: &Arc<Semaphore>,
    general: &Arc<Semaphore>,
) -> Arc<Semaphore> {
    if matches!(
        method,
        "upload-begin" | "upload-status" | "upload-chunk" | "upload-commit" | "upload-cancel"
    ) {
        uploads.clone()
    } else {
        general.clone()
    }
}

#[derive(Deserialize)]
struct Pair {
    code: String,
    name: String,
}
/// Bound `/v1/pair` as a whole, a slow body included.
async fn pair_deadline(
    State(server): State<Shared>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    match tokio::time::timeout(PAIR_DEADLINE, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            // The hub call given up on may still answer: the next one gets a fresh connection.
            if let Ok(mut owner) = server.owner.try_lock() {
                *owner = None;
            }
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"error":"The hub did not answer in time. Create a new code and try again."})),
            )
                .into_response()
        }
    }
}
async fn pair(
    State(server): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(p): Json<Pair>,
) -> impl IntoResponse {
    if !server.pairing.take(peer.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"Too many pairing attempts; wait a minute"})),
        );
    }
    // A code as typed (`abcd-efgh-jk23`) is the code; anything else never reaches the hub.
    let code = twr_core::phone_pairing::normalize_code(&p.code);
    let (Some(code), false) = (code, p.name.is_empty() || p.name.len() > 80) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"Invalid pairing request"})),
        );
    };
    let result = server
        .owner_call("device.pair_complete", json!({"code":code,"name":p.name}))
        .await;
    match result {
        Ok(mut value) => {
            value["gateway_id"] = json!(server.push.gateway_id);
            (StatusCode::OK, Json(value))
        }
        Err(error)
            if error
                .downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|e| e.code == twr_core::proto::codes::UNAUTHORIZED) =>
        {
            (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error":"Pairing code invalid or expired"})),
            )
        }
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"The hub is unavailable or busy. Try again when it reconnects."})),
        ),
    }
}
#[derive(Deserialize)]
struct ProofQuery {
    nonce: String,
}
/// `GET /v1/pair/proof?nonce=`: the open invitation's proof for this gateway's pin, the
/// phone's nonce and the gateway's id, which a phone that typed the code checks before it
/// trusts the key it was shown (docs/design/phone-pairing.md). Within `/v1/pair`'s rate limits
/// and deadline; no attempt at the code.
async fn pair_proof(
    State(server): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    query: Result<axum::extract::Query<ProofQuery>, axum::extract::rejection::QueryRejection>,
) -> impl IntoResponse {
    use twr_core::phone_pairing::{b64url_decode, NONCE_LEN};
    if !server.pairing.take(peer.ip()) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error":"Too many pairing attempts; wait a minute"})),
        );
    }
    let nonce = query
        .ok()
        .map(|q| q.0.nonce)
        .filter(|n| n.len() < 64 && b64url_decode(n).is_ok_and(|n| n.len() == NONCE_LEN));
    let Some(nonce) = nonce else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"A nonce is 32 bytes, base64url without padding"})),
        );
    };
    let none_open = || {
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error":"No pairing invitation is open"})),
        )
    };
    let Some(pinned) = &server.pinned else {
        return none_open();
    };
    let params = json!({"nonce":nonce,"pin":pinned.pin,"gateway_id":server.push.gateway_id});
    match server.owner_call("device.pair_proof", params).await {
        Ok(value) if value["proof"].is_string() => {
            (StatusCode::OK, Json(json!({"proof":value["proof"]})))
        }
        Err(error)
            if error
                .downcast_ref::<twr_core::proto::RpcError>()
                .is_some_and(|e| e.code == twr_core::proto::codes::NOT_FOUND) =>
        {
            none_open()
        }
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error":"The hub is unavailable or busy. Try again when it reconnects."})),
        ),
    }
}
/// The device a socket upgrade belongs to, or the status it is refused with. Clients take 401
/// as a revocation and stop reconnecting, so it means only a token no paired device has. A
/// device list that cannot be read is 503 (try again); a foreign web origin is 403.
fn authorize_upgrade(
    auth: &Auth,
    headers: &HeaderMap,
    development: bool,
) -> Result<(Device, String), StatusCode> {
    if let Some(origin) = headers.get("origin") {
        let origin = origin.to_str().unwrap_or("");
        if ![
            "capacitor://localhost",
            "http://localhost",
            "https://localhost",
        ]
        .contains(&origin)
            && !(development
                && (origin.starts_with("http://127.0.0.1:")
                    || origin.starts_with("http://localhost:")))
        {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    match auth.authenticate(token) {
        Ok(Some(device)) => Ok((device, token.to_owned())),
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(error) => {
            crate::gateway_log!("Paired devices could not be read: {error:#}");
            Err(StatusCode::SERVICE_UNAVAILABLE)
        }
    }
}
/// Said to a device whose standing cannot be checked: its socket stays, its request waits for it.
const STANDING_UNKNOWN: &str = "The hub could not read its paired devices. Try again in a moment.";
async fn upgrade(
    State(server): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> axum::response::Response {
    // A peer guessing tokens is told to wait (429, never 401) before anything is checked.
    if server.refused_upgrades.exhausted(peer.ip()) {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    }
    let (device, token) = match authorize_upgrade(&server.auth, &headers, server.development) {
        Ok(found) => found,
        Err(status) => {
            if status == StatusCode::UNAUTHORIZED {
                server.refused_upgrades.spend(peer.ip());
            }
            return status.into_response();
        }
    };
    let fragmented = headers.get("x-twr-framing").and_then(|v| v.to_str().ok()) == Some("1");
    let Ok(slot) = server.slots.clone().try_acquire_owned() else {
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };
    // A paired device's socket: the 32 device slots bound it now, not the pre-auth limits.
    server.connections.authenticated(peer);
    ws.max_message_size(2 * 1024 * 1024)
        .max_frame_size(2 * 1024 * 1024)
        .on_upgrade(move |ws| async move {
            let _slot = slot;
            let _ = serve_socket(server, ws, device, token, fragmented).await;
        })
}
#[derive(Deserialize)]
struct Call {
    id: String,
    method: String,
    #[serde(default)]
    params: Value,
    workspace: Option<String>,
}
struct WorkspaceConnection {
    workspace: String,
    bridge: Option<Arc<Companion>>,
}
struct Session {
    inbox_reading: AtomicBool,
    terminal_slots: Arc<Semaphore>,
    device: Device,
    token: String,
    connection: RwLock<WorkspaceConnection>,
    connecting: Mutex<()>,
    bridge_generation: Arc<AtomicU64>,
    terminals: Mutex<HashMap<String, Terminal>>,
    events: mpsc::Sender<Value>,
    terminal_events: mpsc::Sender<Value>,
    uploads: Mutex<HashMap<String, remote::upload::Pending>>,
    upload_quota: std::sync::Mutex<remote::upload::Quota>,
    latest_state: Arc<std::sync::Mutex<Option<Value>>>,
    /// The phone's own speaker (`remote::voice`), when voice mode runs one.
    voice: remote::voice::DeviceVoice,
}
/// The workspace owns the device check too: an unrestricted gateway process must never
/// substitute its own local-user authority for a paired device's read grant.
async fn read_inbox_workspace(
    socket: &Path,
    workspace: &str,
    device: &Device,
    token: &str,
) -> Result<(Value, Value)> {
    let mut client = twr_core::client::Client::connect(socket).await?;
    let hello = client
        .hello(json!({"client":format!("inbox-{}", device.id),"device_token":token}))
        .await?;
    anyhow::ensure!(
        hello["identity"].as_str() == Some(format!("user:device:{}", device.id).as_str()),
        "Workspace did not authenticate this device"
    );
    let info = client.call("hub.info", json!({})).await?;
    anyhow::ensure!(
        info["workspace"].as_str() == Some(workspace),
        "Socket serves another workspace"
    );
    let attention = client.call("attention.list", json!({"all":true})).await?;
    let threads = client.call("thread.list", json!({})).await?;
    Ok((attention, threads))
}
fn inbox_socket_stopped(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
        })
}
async fn collect_inbox_sources<S: futures::Stream<Item = Value> + Unpin>(
    mut stream: S,
    names: Vec<String>,
    gateway_id: &str,
    budget: Duration,
) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut sources = Vec::new();
    while let Ok(Some(source)) = tokio::time::timeout_at(deadline, stream.next()).await {
        sources.push(source);
    }
    let checked: std::collections::HashSet<String> = sources
        .iter()
        .filter_map(|source| source["workspace"].as_str().map(str::to_owned))
        .collect();
    for workspace in names
        .into_iter()
        .filter(|workspace| !checked.contains(workspace))
    {
        sources.push(json!({"hubId":gateway_id,"hubName":"Remote hub","workspace":workspace,"attention":[],"threads":[],"error":"Summary not checked before refresh deadline"}));
    }
    sources
}
impl Session {
    /// Read other workspaces with the same device credential. These short-lived sockets
    /// never replace the active companion, subscribe, start a hub or emit client state.
    async fn inbox_sources(&self, server: &Server) -> Result<Value> {
        anyhow::ensure!(
            self.device.allows("attention.list") && self.device.allows("thread.list"),
            "This device does not have read access"
        );
        let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
        let mut workspaces = vec!["default".to_owned()];
        workspaces.extend(paths.named_workspaces()?);
        anyhow::ensure!(
            workspaces.len() <= 256,
            "Too many workspaces to summarize at once"
        );
        let names = workspaces.clone();
        let stream = futures::stream::iter(workspaces).map(|workspace| async move {
            let result = tokio::time::timeout(Duration::from_secs(3), async {
                let paths = twr_core::paths::Paths::from_env_in(Some(&workspace))?;
                let config = twr_core::config::Config::load_from(&paths)?;
                read_inbox_workspace(&config.socket_path(&paths), &workspace, &self.device, &self.token).await
            }).await;
            match result {
                Ok(Ok((attention, threads))) => json!({"hubId":server.push.gateway_id,"hubName":"Remote hub","workspace":workspace,"attention":attention,"threads":threads}),
                Ok(Err(error)) => {
                    let stopped = inbox_socket_stopped(&error);
                    if stopped {json!({"hubId":server.push.gateway_id,"hubName":"Remote hub","workspace":workspace,"attention":[],"threads":[],"running":false})}
                    else {json!({"hubId":server.push.gateway_id,"hubName":"Remote hub","workspace":workspace,"attention":[],"threads":[],"error":error.to_string()})}
                },
                Err(_) => json!({"hubId":server.push.gateway_id,"hubName":"Remote hub","workspace":workspace,"attention":[],"threads":[],"error":"Workspace did not answer within 3 seconds"}),
            }
        }).buffer_unordered(4);
        Ok(json!(
            collect_inbox_sources(
                stream,
                names,
                &server.push.gateway_id,
                Duration::from_secs(4)
            )
            .await
        ))
    }
    async fn call(&self, server: &Server, call: &Call) -> Result<Value> {
        let p = &call.params;
        let connection = self.connection.read().await;
        let workspace = connection.workspace.clone();
        let selected_bridge = connection.bridge.clone();
        drop(connection);
        let bridge = || {
            selected_bridge
                .clone()
                .context("Connect to a workspace first")
        };
        anyhow::ensure!(
            matches!(call.method.as_str(), "connect" | "bootstrap")
                || call
                    .workspace
                    .as_ref()
                    .is_none_or(|requested| requested == &workspace),
            "Workspace changed; review the destination before retrying"
        );
        match call.method.as_str() {
            "inbox-sources" => self.inbox_sources(server).await,
            "bootstrap" => Ok(
                json!({"workspace":workspace,"platform":"remote","home":std::env::var("HOME").unwrap_or_default(),"version":twr_core::version(),"theme":null,"terminals":self.terminals.lock().await.values().map(Terminal::summary).collect::<Vec<_>>(),"scopes":self.device.scopes,"allowed_methods":twr_core::devices::METHOD_SCOPES.iter().flat_map(|(_,m)|m.iter()).filter(|m|self.device.allows(m) && !m.starts_with("device.pair_")).collect::<Vec<_>>(),"read_methods":twr_core::devices::METHOD_SCOPES.iter().find(|(s,_)|*s=="read").map(|(_,m)|*m).unwrap_or_default()}),
            ),
            "connect" => {
                let _connecting = self.connecting.lock().await;
                let workspace = p["workspace"].as_str().context("Workspace required")?;
                if let Some(error) = twr_core::paths::workspace_name_error(workspace) {
                    anyhow::bail!(error);
                }
                let existing = {
                    let connection = self.connection.read().await;
                    (connection.workspace == workspace)
                        .then(|| connection.bridge.clone())
                        .flatten()
                };
                if let Some(bridge) = existing.filter(|bridge| bridge.is_alive()) {
                    if bridge.call("hub.info", json!({})).await.is_ok() {
                        let paths = bridge.call("desktop.paths", json!({})).await?;
                        let cached = self.latest_state.lock().unwrap().clone();
                        if let Some(state) = cached {
                            if event_generation(&state)
                                == Some(self.bridge_generation.load(Ordering::SeqCst))
                            {
                                let _ = self.events.send(state).await;
                            }
                        }
                        let _ = self
                            .events
                            .send(json!({"type":"connection","connected":true}))
                            .await;
                        return Ok(paths);
                    }
                }
                let generation = self.bridge_generation.load(Ordering::SeqCst) + 1;
                if self.device.scopes.iter().any(|s| s == "operate") {
                    let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
                    let config = twr_core::config::Config::load_from(&paths)?;
                    crate::runtime::ensure_hub(&paths, &config.socket_path(&paths), &server.binary)
                        .await?;
                }
                let (events, mut received) = mpsc::channel::<Value>(256);
                let output = self.events.clone();
                let current = self.bridge_generation.clone();
                let (ready, mut committed) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    let mut initial_state = None;
                    let mut initial_connection = None;
                    let mut initial_other = std::collections::VecDeque::new();
                    // Drain before commit so events cannot block the stdout reader
                    // that also delivers the authentication RPC replies.
                    loop {
                        tokio::select! {biased;
                            result=&mut committed=>{if result.is_err(){return;}break;},
                            event=received.recv()=>{
                                let Some(event)=event else{return;};
                                match event["type"].as_str() {
                                    Some("state")=>initial_state=Some(event),
                                    Some("connection")=>initial_connection=Some(event),
                                    Some("control")=>{},
                                    _=>{if initial_other.len()==32{initial_other.pop_front();}initial_other.push_back(event);}
                                }
                            }
                        }
                    }
                    for event in initial_connection
                        .into_iter()
                        .chain(initial_state)
                        .chain(initial_other)
                    {
                        if current.load(Ordering::SeqCst) == generation
                            && output.send(stamp(event, generation)).await.is_err()
                        {
                            return;
                        }
                    }
                    while let Some(event) = received.recv().await {
                        if current.load(Ordering::SeqCst) == generation
                            && output.send(stamp(event, generation)).await.is_err()
                        {
                            break;
                        }
                    }
                });
                let bridge = Companion::start(
                    &server.companion,
                    workspace,
                    &self.device.id,
                    &self.token,
                    events,
                )
                .await?;
                let paths = bridge.call("desktop.paths", json!({})).await?;
                // Prove device identity before presenting the connection as usable.
                bridge.call("hub.info", json!({})).await?;
                self.bridge_generation.store(generation, Ordering::SeqCst);
                self.latest_state.lock().unwrap().take();
                *self.connection.write().await = WorkspaceConnection {
                    workspace: workspace.into(),
                    bridge: Some(Arc::new(bridge)),
                };
                let _ = ready.send(());
                Ok(paths)
            }
            "rpc" => {
                let method = p["method"].as_str().context("Method required")?;
                anyhow::ensure!(
                    method != "thread.upload",
                    "Use the bounded attachment upload commands"
                );
                let private = [
                    "desktop.subscribe",
                    "desktop.preferences",
                    "desktop.retry_attention",
                    "desktop.parse_slash",
                    "desktop.slash_commands",
                ];
                anyhow::ensure!(
                    !method.starts_with("desktop.") || private.contains(&method),
                    "Private host operation"
                );
                anyhow::ensure!(
                    self.device.allows_params(method, &p["params"]),
                    "This device does not have permission for this operation"
                );
                bridge()?.call(method, p["params"].clone()).await
            }
            "catalog" => Ok(serde_json::from_str(include_str!(
                "../../../../clients/mobile/catalog.json"
            ))?),
            "directories" => {
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "operate"),
                    "Browsing directories requires operate permission"
                );
                let path = PathBuf::from(p["path"].as_str().context("Directory required")?);
                anyhow::ensure!(
                    path.is_absolute() && path.as_os_str().len() <= 4096,
                    "An absolute directory is required"
                );
                tokio::task::spawn_blocking(move||->Result<Value>{let path=std::fs::canonicalize(path)?;let mut entries=Vec::new();let mut truncated=false;for entry in std::fs::read_dir(&path)?{let entry=entry?;if !entry.file_type()?.is_dir(){continue}
if entries.len()>=200{truncated=true;break}entries.push(json!({"name":entry.file_name().to_string_lossy(),"path":entry.path()}));}entries.sort_by(|a,b|a["name"].as_str().cmp(&b["name"].as_str()));Ok(json!({"path":path,"parent":path.parent(),"entries":entries,"truncated":truncated}))}).await?
            }
            "unpair" => {
                server
                    .owner_call("device.revoke", json!({"id":self.device.id}))
                    .await
            }
            "push-status" => Ok(server.push.status()),
            "push-unregister" => {
                server.push.unregister(&self.device.id).await?;
                Ok(json!({"ok":true}))
            }
            "push-register" => {
                server
                    .push
                    .register(remote::push::Recipient {
                        device: self.device.id.clone(),
                        workspace: workspace.clone(),
                        platform: p["platform"]
                            .as_str()
                            .context("Push platform required")?
                            .to_owned(),
                        token: p["token"]
                            .as_str()
                            .context("Push token required")?
                            .to_owned(),
                    })
                    .await?;
                Ok(json!({"ok":true}))
            }
            "upload-begin" => {
                anyhow::ensure!(
                    self.device.allows("thread.upload"),
                    "Uploading requires operate permission"
                );
                let thread = p["thread"].as_str().context("Thread required")?;
                let sha256 = p["sha256"].as_str().context("Checksum required")?;
                let name = p["name"].as_str().context("Filename required")?;
                anyhow::ensure!(
                    !name.is_empty() && name.len() <= 120,
                    "Filename must contain 1–120 UTF-8 bytes"
                );
                let size = usize::try_from(p["size"].as_u64().context("File size required")?)?;
                anyhow::ensure!(
                    size <= remote::upload::MAX_FILE,
                    "Files can be at most 25 MiB"
                );
                let id = p["id"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                anyhow::ensure!(
                    id.len() <= 64
                        && !id.is_empty()
                        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
                    "Invalid upload ID"
                );
                let selected = bridge()?;
                let mut uploads = self.uploads.lock().await;
                let reserved = if let Some(upload) = uploads.get_mut(&id) {
                    anyhow::ensure!(
                        upload.workspace == workspace
                            && upload.thread == thread
                            && upload.sha256 == sha256
                            && upload.size == size,
                        "Upload destination or checksum changed"
                    );
                    anyhow::ensure!(
                        !upload.busy,
                        "An upload operation is in progress; check status shortly"
                    );
                    upload.busy = true;
                    false
                } else {
                    anyhow::ensure!(
                        uploads.values().filter(|u| !u.committed).count() < 4,
                        "Finish an existing upload first"
                    );
                    self.upload_quota.lock().unwrap().reserve(
                        &id,
                        size,
                        std::time::Instant::now(),
                    )?;
                    uploads.insert(
                        id.clone(),
                        remote::upload::Pending {
                            workspace: workspace.clone(),
                            thread: thread.into(),
                            sha256: sha256.into(),
                            size,
                            created: std::time::Instant::now(),
                            committed: false,
                            busy: true,
                        },
                    );
                    true
                };
                drop(uploads);
                let result = selected.call_with_timeout("thread.upload", json!({"thread":thread,"id":id,"sha256":sha256,"action":"begin","name":name,"size":size}), Duration::from_secs(12)).await;
                let mut uploads = self.uploads.lock().await;
                remote::upload::finish_begin(
                    &mut uploads,
                    &mut self.upload_quota.lock().unwrap(),
                    &id,
                    reserved,
                    &result,
                );
                result
            }
            method @ ("upload-status" | "upload-chunk" | "upload-commit" | "upload-cancel") => {
                anyhow::ensure!(
                    self.device.allows("thread.upload"),
                    "Uploading requires operate permission"
                );
                let id = p["id"].as_str().context("Upload ID required")?;
                let selected = bridge()?;
                let mut uploads = self.uploads.lock().await;
                if method == "upload-cancel" && !uploads.contains_key(id) {
                    drop(uploads);
                    let thread = p["thread"].as_str().context("Thread required")?;
                    let sha256 = p["sha256"].as_str().context("Checksum required")?;
                    let result = selected
                        .call_with_timeout(
                            "thread.upload",
                            json!({"thread":thread,"id":id,"sha256":sha256,"action":"cancel"}),
                            Duration::from_secs(12),
                        )
                        .await;
                    // A definite refusal cannot leave this gateway reservation alive.
                    // The host also expires staged files; a transport failure remains uncertain.
                    return result.or_else(|error| {
                        if error
                            .downcast_ref::<twr_core::proto::RpcError>()
                            .is_some_and(|rpc| {
                                rpc.code == codes::BAD_REQUEST || rpc.code == codes::UPLOAD_REJECTED
                            })
                        {
                            Ok(json!({"cancelled":true,"committed":false}))
                        } else {
                            Err(error)
                        }
                    });
                }
                let upload = uploads
                    .get_mut(id)
                    .context("Upload expired or was not found; start again")?;
                anyhow::ensure!(
                    upload.workspace == workspace
                        && p["thread"].as_str() == Some(&upload.thread)
                        && p["sha256"].as_str() == Some(&upload.sha256),
                    "Upload destination or checksum changed"
                );
                if upload.busy && method == "upload-status" {
                    return Ok(json!({"id":id,"busy":true}));
                }
                anyhow::ensure!(
                    !upload.busy,
                    "An upload operation is in progress; retry status or cancellation shortly"
                );
                upload.busy = true;
                upload.created = std::time::Instant::now();
                let mut params = json!({"thread":upload.thread,"id":id,"sha256":upload.sha256,"action":method.trim_start_matches("upload-")});
                if method == "upload-chunk" {
                    params["offset"] = p["offset"].clone();
                    params["data"] = p["data"].clone();
                }
                drop(uploads);
                let result = selected
                    .call_with_timeout("thread.upload", params, Duration::from_secs(12))
                    .await;
                let mut uploads = self.uploads.lock().await;
                remote::upload::finish_command(
                    &mut uploads,
                    &mut self.upload_quota.lock().unwrap(),
                    id,
                    method,
                    &result,
                );
                result
            }
            "cli" => {
                let args: Vec<String> = serde_json::from_value(p["args"].clone())?;
                device_cli_args(&self.device, &args)?;
                let out = tokio::process::Command::new(&server.binary)
                    .args(["-w", &workspace])
                    .args(args)
                    .env_remove("TWR_DEVICE_TOKEN")
                    .output()
                    .await?;
                Ok(
                    json!({"stdout":String::from_utf8_lossy(&out.stdout),"stderr":String::from_utf8_lossy(&out.stderr),"code":out.status.code().unwrap_or(1)}),
                )
            }
            "config-read" | "config-save" | "config-set" => {
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "admin"),
                    "Hub settings require admin permission for this device"
                );
                let method = match call.method.as_str() {
                    "config-read" => "desktop.config.read",
                    "config-save" => "desktop.config.save",
                    _ => "desktop.config.set",
                };
                bridge()?.call(method, p.clone()).await
            }
            "terminal-open" => {
                let device_permit = self
                    .terminal_slots
                    .clone()
                    .try_acquire_owned()
                    .context("Close a terminal first (maximum four)")?;
                let server_permit = server
                    .terminal_slots
                    .clone()
                    .try_acquire_owned()
                    .context("The hub has eight active mobile terminals. Close one first.")?;
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "terminal"),
                    "Remote terminals require terminal permission for this device"
                );
                anyhow::ensure!(
                    self.terminals
                        .lock()
                        .await
                        .values()
                        .filter(|t| !t.exited.load(Ordering::SeqCst))
                        .count()
                        < 4,
                    "Close a terminal first (maximum four)"
                );
                let shell = p["shell"].as_bool().unwrap_or(false);
                let args: Vec<String> = p
                    .get("args")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let args = if shell {
                    vec!["-l".to_owned()]
                } else {
                    twr_core::app_commands::plan(&args, &workspace)?
                };
                let binary = if shell {
                    PathBuf::from(std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()))
                } else {
                    server.binary.clone()
                };
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let cwd = Path::new(p["cwd"].as_str().unwrap_or(&home));
                anyhow::ensure!(
                    cwd.is_absolute() && cwd.is_dir(),
                    "Choose an existing absolute directory on the hub"
                );
                if !shell {
                    let out = tokio::process::Command::new(&binary)
                        .arg("--desktop-validate")
                        .args(&args)
                        .output()
                        .await?;
                    anyhow::ensure!(
                        out.status.success(),
                        "{}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                }
                let terminal = Terminal::open(
                    &binary,
                    &args,
                    cwd,
                    p["title"].as_str().unwrap_or("Remote terminal").to_owned(),
                    workspace.clone(),
                    self.terminal_events.clone(),
                    vec![device_permit, server_permit],
                )?;
                let summary = terminal.summary();
                let mut terminals = self.terminals.lock().await;
                if terminals.len() >= 16 {
                    let oldest = terminals
                        .iter()
                        .filter(|(_, t)| t.exited.load(Ordering::SeqCst))
                        .min_by_key(|(_, t)| t.started)
                        .map(|(id, _)| id.clone());
                    if let Some(id) = oldest {
                        terminals.remove(&id);
                        let _ = self
                            .events
                            .try_send(json!({"type":"terminal-removed","id":id}));
                    }
                }
                terminals.insert(terminal.id.clone(), terminal);
                Ok(summary)
            }
            "terminal-read" => Ok(self
                .terminals
                .lock()
                .await
                .get(p["id"].as_str().unwrap_or(""))
                .context("Terminal not found")?
                .read()),
            "terminal-write" => {
                self.terminals
                    .lock()
                    .await
                    .get_mut(p["id"].as_str().unwrap_or(""))
                    .context("Terminal not found")?
                    .write(p["data"].as_str().context("Input required")?)
                    .await?;
                Ok(Value::Null)
            }
            "terminal-resize" => {
                self.terminals
                    .lock()
                    .await
                    .get(p["id"].as_str().unwrap_or(""))
                    .context("Terminal not found")?
                    .resize(
                        u16::try_from(p["cols"].as_u64().unwrap_or(0))?,
                        u16::try_from(p["rows"].as_u64().unwrap_or(0))?,
                    )?;
                Ok(Value::Null)
            }
            "terminal-close" => {
                self.terminals
                    .lock()
                    .await
                    .remove(p["id"].as_str().unwrap_or(""));
                Ok(Value::Null)
            }
            "terminal-ack" | "terminal-focus" => Ok(Value::Null),
            "voice-speaker" => {
                remote::voice::allowed(&self.device)?;
                let op = remote::voice::Op::parse(p)?;
                self.voice
                    .control(op, || self.voice_start(server, &workspace))
                    .await
            }
            // The phone's read-aloud voices: its choice per language, and twr's natural voices
            // this machine can make for it.
            "voice-voices" => {
                remote::voice::allowed(&self.device)?;
                let chosen = self.voice_store(&workspace)?.load()?.voices;
                let natural = match remote::voice::natural_listing(&server.binary).await {
                    Ok(listing) => listing,
                    Err(e) => json!({"voices": [], "models": [], "error": format!("{e:#}")}),
                };
                Ok(json!({
                    "by_language": chosen,
                    "natural": natural,
                    "can_install": self.device.scopes.iter().any(|s| s == "operate"),
                }))
            }
            "voice-select" => {
                remote::voice::allowed(&self.device)?;
                let lang = p["lang"].as_str().context("Language required")?;
                let voice = p["voice"].as_str().filter(|v| !v.is_empty());
                let chosen = remote::voice::select(&self.voice_store(&workspace)?, lang, voice)?;
                Ok(json!({"by_language": chosen}))
            }
            // Downloads a model onto this machine: as much as `operate` may change here.
            "voice-install" => {
                remote::voice::allowed(&self.device)?;
                anyhow::ensure!(
                    self.device.scopes.iter().any(|s| s == "operate"),
                    "Downloading a voice on the hub's computer requires operate permission"
                );
                let model = p["model"].as_str().context("Model required")?;
                self.voice.start_install(server.binary.clone(), model)?;
                Ok(json!({"started": true}))
            }
            "voice-preview" => {
                remote::voice::allowed(&self.device)?;
                let voice = p["voice"].as_str().context("Voice required")?;
                let lang = p["lang"].as_str().unwrap_or("en");
                let id = self.voice.preview(&server.renderer, voice, lang)?;
                Ok(json!({"id": id}))
            }
            "voice-preview-stop" => {
                self.voice.stop_preview();
                Ok(Value::Null)
            }
            _ => anyhow::bail!("Unsupported mobile operation"),
        }
    }
}
impl Session {
    /// The state file of this device's speaker (its voices too).
    fn voice_store(&self, workspace: &str) -> Result<twr_speech::StateStore> {
        let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
        remote::voice::state_store(&paths.root().data_dir.join("mobile"), &self.device.id)
    }

    /// The phone's speaker for `workspace`: this machine's hub of it, the device's own state
    /// file, `[speech]`'s choices of what is read and attention announcements (its engine
    /// settings are this machine's and play no part: the phone speaks). It ends once the device is revoked or loses `read`.
    fn voice_start(&self, server: &Server, workspace: &str) -> Result<remote::voice::Start> {
        let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
        let root = paths.root();
        let mut config = twr_core::config::Config::load_from(&root)
            .map(|c| c.speech)
            .unwrap_or_default();
        // Voice mode also says when something needs you, as the desktop's speaker does
        // (`twr speak --announce`): a fixed sentence with a thread's name.
        if !config.read.contains(&twr_core::speech::ReadKind::Attention) {
            config.read.push(twr_core::speech::ReadKind::Attention);
        }
        let store = remote::voice::state_store(&root.data_dir.join("mobile"), &self.device.id)?;
        let (auth, device) = (server.auth.clone(), self.device.clone());
        Ok(remote::voice::Start {
            workspace: workspace.to_owned(),
            name: remote::voice::speaker_name(&self.device.name),
            store,
            link: twr_speech::LinkSpec::Local {
                paths,
                workspaces: twr_speech::Workspaces::One(workspace.to_owned()),
            },
            config,
            renderer: Some(server.renderer.clone()),
            allowed: Arc::new(move || {
                !auth.current(&device).revoked() && remote::voice::allowed(&device).is_ok()
            }),
        })
    }
}
/// The `cli` call: what a paired device may have the gateway run as `twr -w <its workspace>
/// <args>` (as the hub's user, so with the user's authority over the hub), the whole command
/// line spelled out, by scope:
///
/// - `read`: `ws list`, `ws list --json` (the workspaces and whether their hubs run; a read
///   device connects to any of them and reads it anyway);
/// - `operate` (and so `terminal` and `admin`): also `ws new <name>`, a valid workspace name
///   (connecting to a workspace with operate starts its hub, creating it).
///
/// Nothing else, whatever the scope: no other subcommand (a thread, a runtime, a mode, a box,
/// configuration), no other flag (`--socket`, `-w`, `--`, `--help`, `ws list --detail`, which
/// would print every hub's threads and requests as the user reads them), no other argument. What
/// else a device runs through the CLI it runs in an embedded terminal (`terminal-open`), which
/// needs `terminal`.
fn device_cli_args(device: &Device, args: &[String]) -> Result<()> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["ws", "list"] | ["ws", "list", "--json"] => Ok(()),
        ["ws", "new", name] => {
            anyhow::ensure!(device.allows("thread.new"), "Device is read-only");
            if let Some(error) = twr_core::paths::workspace_name_error(name) {
                anyhow::bail!(error);
            }
            Ok(())
        }
        _ => anyhow::bail!("Use an embedded terminal for this command"),
    }
}
async fn serve_socket(
    server: Shared,
    mut socket: WebSocket,
    device: Device,
    token: String,
    fragmented: bool,
) -> Result<()> {
    let mut hello = json!({"type":"authenticated","device":device.id,"scopes":device.scopes,"framing":fragmented});
    // A pinned gateway's routes now, which the phone merges into those it knows.
    if let Some(pinned) = &server.pinned {
        hello["routes"] = json!(pinned.routes());
    }
    socket.send(Message::Text(hello.to_string().into())).await?;
    let slot = {
        let mut sessions = server.sessions.lock().await;
        if let Some(slot) = sessions.get(&device.id) {
            slot.clone()
        } else {
            let (tx, mut rx) = mpsc::channel::<Value>(128);
            let events = remote::events::Sender::new();
            let (terminal_tx, mut terminal_rx) = mpsc::channel::<Value>(128);
            let (terminal_events, _) = broadcast::channel(256);
            let terminal_forward = terminal_events.clone();
            tokio::spawn(async move {
                while let Some(event) = terminal_rx.recv().await {
                    let _ = terminal_forward.send(event);
                }
            });
            let latest_state = Arc::new(std::sync::Mutex::new(None));
            let state_cache = latest_state.clone();
            let bridge_generation = Arc::new(AtomicU64::new(0));
            let current_generation = bridge_generation.clone();
            let forward = events.clone();
            tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if event_generation(&event)
                        .is_some_and(|g| g != current_generation.load(Ordering::SeqCst))
                    {
                        continue;
                    }
                    if event["type"] == "state" {
                        let mut cache = state_cache.lock().unwrap();
                        if event_generation(&event)
                            == Some(current_generation.load(Ordering::SeqCst))
                        {
                            *cache = Some(event.clone());
                        }
                    }
                    forward.send(event);
                }
            });
            let (input, mut input_rx) = mpsc::channel::<OrderedInput>(64);
            let slot = Arc::new(DeviceSession {
                upload_slots: Arc::new(Semaphore::new(4)),
                input,
                session: Session {
                    inbox_reading: AtomicBool::new(false),
                    terminal_slots: Arc::new(Semaphore::new(4)),
                    device: device.clone(),
                    token: token.clone(),
                    connection: RwLock::new(WorkspaceConnection {
                        workspace: "default".into(),
                        bridge: None,
                    }),
                    connecting: Mutex::new(()),
                    bridge_generation,
                    terminals: Mutex::new(HashMap::new()),
                    events: tx.clone(),
                    terminal_events: terminal_tx,
                    uploads: Mutex::new(HashMap::new()),
                    upload_quota: std::sync::Mutex::new(remote::upload::Quota::default()),
                    latest_state: latest_state.clone(),
                    voice: remote::voice::DeviceVoice::new(tx.clone()),
                },
                events,
                terminal_events,
                latest_state,
                generation: AtomicU64::new(0),
                last_seen: AtomicU64::new(remote::auth::now()),
                foreground: AtomicBool::new(false),
            });
            let weak_slot = Arc::downgrade(&slot);
            let weak_server = Arc::downgrade(&server);
            tokio::spawn(async move {
                while let Some((call, replies, generation)) = input_rx.recv().await {
                    let (Some(slot), Some(server)) = (weak_slot.upgrade(), weak_server.upgrade())
                    else {
                        break;
                    };
                    if slot.generation.load(Ordering::SeqCst) != generation {
                        continue;
                    }
                    match server.auth.current(&slot.session.device) {
                        Standing::Current => {}
                        Standing::Revoked => continue,
                        Standing::Unknown => {
                            let _ = replies
                                .send(json!({"id":call.id,"error":STANDING_UNKNOWN}))
                                .await;
                            continue;
                        }
                    }
                    if call.method == "inbox-sources" {
                        if slot.session.inbox_reading.swap(true, Ordering::SeqCst) {
                            let _ = replies.send(json!({"id":call.id,"error":"An inbox refresh is already running"})).await;
                            continue;
                        }
                        tokio::spawn(async move {
                            let result = tokio::time::timeout(
                                Duration::from_secs(5),
                                slot.session.inbox_sources(&server),
                            )
                            .await;
                            slot.session.inbox_reading.store(false, Ordering::SeqCst);
                            if slot.generation.load(Ordering::SeqCst) != generation
                                || server.auth.current(&slot.session.device).revoked()
                            {
                                return;
                            }
                            let reply = match result {
                                Ok(Ok(value)) => json!({"id":call.id,"result":value}),
                                Ok(Err(error)) => json!({"id":call.id,"error":error.to_string()}),
                                Err(_) => {
                                    json!({"id":call.id,"error":"Inbox refresh timed out after 5 seconds; live requests remain available"})
                                }
                            };
                            let _ = replies.send(reply).await;
                        });
                        continue;
                    }
                    let result = slot.session.call(&server, &call).await;
                    let reply = match result {
                        Ok(value) => json!({"id":call.id,"result":value}),
                        Err(error) => json!({"id":call.id,"error":error.to_string()}),
                    };
                    let _ = replies.send(reply).await;
                }
            });
            sessions.insert(device.id.clone(), slot.clone());
            slot
        }
    };
    let generation = slot.generation.fetch_add(1, Ordering::SeqCst) + 1;
    slot.last_seen.store(remote::auth::now(), Ordering::SeqCst);
    let (mut sink, mut incoming) = socket.split();
    let mut rx = slot.events.subscribe();
    // While it lives, the phone's speaker (if any) can reach it: utterances wait for one.
    let _voice_socket = slot.session.voice.socket();
    let mut terminal_rx = slot.terminal_events.subscribe();
    let state_cache = slot.latest_state.clone();
    let (replies, mut response_rx) = mpsc::channel::<Value>(32);
    let alive = Arc::new(AtomicBool::new(true));
    let connected = alive.clone();
    let (heartbeat_tx, mut heartbeat_rx) = mpsc::channel::<Value>(16);
    let bridge_epoch = slot.session.bridge_generation.clone();
    let writer = tokio::spawn(async move {
        use remote::wire::Outbound;
        // Two independent RPC replies, one ordered control stream and one
        // ordered PTY stream. Keepalives always pass between bounded frames.
        let mut lanes: [Option<Outbound>; 4] = [None, None, None, None];
        let mut cursor = 0;
        enum Ready {
            Heartbeat(Option<Value>),
            Reply(Option<Value>),
            Control(Result<remote::events::Event, broadcast::error::RecvError>),
            Terminal(Result<Value, broadcast::error::RecvError>),
            Send,
        }
        loop {
            let ready = tokio::select! {biased;
                beat=heartbeat_rx.recv()=>Ready::Heartbeat(beat),
                reply=response_rx.recv(),if lanes[0].is_none()||lanes[1].is_none()=>Ready::Reply(reply),
                event=rx.recv(),if lanes[2].is_none()=>Ready::Control(event),
                event=terminal_rx.recv(),if lanes[3].is_none()=>Ready::Terminal(event),
                _=std::future::ready(()),if lanes.iter().any(Option::is_some)=>Ready::Send,
            };
            let immediate = match ready {
                Ready::Heartbeat(Some(event)) => Some(socket_message(&event)),
                Ready::Heartbeat(None) | Ready::Reply(None) => break,
                Ready::Reply(Some(event)) => {
                    let index = if lanes[0].is_none() { 0 } else { 1 };
                    lanes[index] = Some(outbound(event, fragmented));
                    None
                }
                Ready::Control(Ok(event)) => {
                    lanes[2] = Some(event.outbound(fragmented));
                    None
                }
                Ready::Control(Err(broadcast::error::RecvError::Lagged(_))) => {
                    let snapshot = state_cache.lock().unwrap().clone().filter(|e| {
                        event_generation(e) == Some(bridge_epoch.load(Ordering::SeqCst))
                    });
                    let mut message = outbound(
                        snapshot.unwrap_or_else(|| json!({"type":"resync"})),
                        fragmented,
                    );
                    message
                        .frames
                        .push_back(socket_message(&json!({"type":"resync"})));
                    lanes[2] = Some(message);
                    None
                }
                Ready::Terminal(Ok(event)) => {
                    lanes[3] = Some(outbound(event, fragmented));
                    None
                }
                Ready::Terminal(Err(broadcast::error::RecvError::Lagged(_))) => {
                    lanes[3] = Some(outbound(json!({"type":"terminal-resync"}), fragmented));
                    None
                }
                Ready::Control(Err(_)) | Ready::Terminal(Err(_)) => break,
                Ready::Send => {
                    let index = (0..4)
                        .map(|offset| (cursor + offset) % 4)
                        .find(|i| lanes[*i].is_some())
                        .unwrap();
                    cursor = (index + 1) % 4;
                    let current = lanes[index].as_mut().unwrap();
                    if current
                        .generation
                        .is_some_and(|g| g != bridge_epoch.load(Ordering::SeqCst))
                    {
                        let cancellation = current.cancel();
                        lanes[index] = None;
                        cancellation
                    } else {
                        let frame = current.frames.pop_front();
                        if current.frames.is_empty() {
                            lanes[index] = None;
                        }
                        frame
                    }
                }
            };
            if let Some(frame) = immediate {
                if !matches!(
                    tokio::time::timeout(send_budget(&frame), sink.send(frame)).await,
                    Ok(Ok(()))
                ) {
                    break;
                }
            }
        }
        connected.store(false, Ordering::SeqCst);
    });
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut count = 0;
    let mut window = std::time::Instant::now();
    let calls = Arc::new(Semaphore::new(64));
    let mut heartbeat = tokio::time::Instant::now();
    let mut last_activity = tokio::time::Instant::now();
    loop {
        tokio::select! {
                  _=tick.tick()=>{
                    if !alive.load(Ordering::SeqCst)||server.auth.current(&device).revoked()||slot.generation.load(Ordering::SeqCst)!=generation||last_activity.elapsed()>Duration::from_secs(45){break}
                    slot.last_seen.store(remote::auth::now(),Ordering::SeqCst);
                    if heartbeat.elapsed()>Duration::from_secs(15){if heartbeat_tx.is_closed(){break}let _=heartbeat_tx.try_send(json!({"type":"heartbeat"}));heartbeat=tokio::time::Instant::now();}
                  }
                  message=incoming.next()=>{
                    let Some(Ok(message))=message else{break};
                    last_activity=tokio::time::Instant::now();
                    if server.auth.current(&device).revoked()||slot.generation.load(Ordering::SeqCst)!=generation{break}
                    let Message::Text(text)=message else{continue};
                    let Ok(call)=serde_json::from_str::<Call>(&text)else{let _=replies.try_send(json!({"type":"error","error":"Invalid request"}));continue};
                    if uuid::Uuid::parse_str(&call.id).is_err()||call.method.len()>80 {let _=replies.try_send(json!({"type":"error","error":"Invalid request ID or method"}));continue}
                    if window.elapsed()>Duration::from_secs(60){window=std::time::Instant::now();count=0}
                    count+=1;
                    if count>1200{let _=replies.try_send(json!({"id":call.id,"error":"Too many requests. Wait a moment before trying again."}));continue}
                    if call.method=="heartbeat"{let _=heartbeat_tx.try_send(json!({"id":call.id,"result":null}));continue}

                    // One bounded ordered input lane. A blocked PTY cannot stall RPCs,
            // heartbeats or revocation on the WebSocket read loop.
            if matches!(call.method.as_str(),"terminal-write"|"terminal-resize") {
                let id=call.id.clone();
                if slot.input.try_send((call,replies.clone(),generation)).is_err() {let _=replies.try_send(json!({"id":id,"error":"Terminal input queue is full"}));}
                continue;
            }
            let lane=operation_slots(&call.method,&slot.upload_slots,&calls);
            let Ok(permit)=lane.try_acquire_owned()else{let _=replies.try_send(json!({"id":call.id,"error":"Too many pending operations"}));continue};
                    let task_server=server.clone();let task_slot=slot.clone();let task_device=device.clone();let task_replies=replies.clone();
                    tokio::spawn(async move{
                      let _permit=permit;
                      if task_slot.generation.load(Ordering::SeqCst)!=generation{return}
                      match task_server.auth.current(&task_device){Standing::Current=>{},Standing::Revoked=>return,Standing::Unknown=>{let _=task_replies.send(json!({"id":call.id,"error":STANDING_UNKNOWN})).await;return}}
                      let mutation=remote::receipts::mutation(&call.method,&call.params);
                      let workspace=call.workspace.clone().unwrap_or(task_slot.session.connection.read().await.workspace.clone());
                      let receipt_key=format!("{}:{}",task_device.id,call.id);
                      let mut completion=None;
                      if mutation {
                        if let Err(error)=authorize_mutation(&task_device,&call) {let _=task_replies.send(json!({"id":call.id,"error":error.to_string()})).await;return;}
                        let mut waiters=task_server.receipt_waiters.lock().await;
                        match task_server.receipts.begin(&task_device.id,&call.id,&workspace,&call.method,&call.params){Ok(Some(reply))=>{
                          if let Some(mut receiver)=waiters.get(&receipt_key).cloned() {
                            drop(waiters);
                            let ready=tokio::time::timeout(Duration::from_secs(660),async {loop {if let Some(value)=receiver.borrow().clone(){return Some(value)}
        if receiver.changed().await.is_err(){return None}}}).await.ok().flatten();
                            let _=task_replies.send(ready.unwrap_or(reply)).await;
                          } else {drop(waiters);let _=task_replies.send(reply).await;}
                          return
                        },Ok(None)=>{let (sender,receiver)=tokio::sync::watch::channel(None);waiters.insert(receipt_key.clone(),receiver);completion=Some(sender);},Err(error)=>{drop(waiters);let _=task_replies.send(json!({"id":call.id,"error":error.to_string()})).await;return}}
                      }
                      let result=task_slot.session.call(&task_server,&call).await;
                      if result.is_ok()&&call.method=="connect"{let workspace=task_slot.session.connection.read().await.workspace.clone();task_slot.session.voice.workspace_changed(&workspace).await;start_observer(task_server.clone(),workspace).await;}
                      if result.is_ok()&&call.method=="rpc"&&call.params["method"]=="notify.looking"{task_slot.foreground.store(call.params["params"]["looking"].as_bool().unwrap_or(false),Ordering::SeqCst);}
                      let reply=match result{Ok(result)=>json!({"id":call.id,"result":result}),Err(error)=>json!({"id":call.id,"error":error.to_string()})};
                      if mutation{if let Err(error)=task_server.receipts.finish(&task_device.id,&call.id,&reply){crate::gateway_log!("Could not record request outcome: {error}");}}
                      if let Some(completion)=completion {completion.send_replace(Some(reply.clone()));task_server.receipt_waiters.lock().await.remove(&receipt_key);}
                      let _=task_replies.send(reply).await;
                    });
                  }
                }
    }
    if slot.generation.load(Ordering::SeqCst) == generation {
        slot.foreground.store(false, Ordering::SeqCst);
        if let Some(bridge) = slot.session.connection.read().await.bridge.as_ref() {
            let _ = bridge
                .call("notify.looking", json!({"looking":false}))
                .await;
        }
    }
    slot.last_seen.store(remote::auth::now(), Ordering::SeqCst);
    drop(replies);
    drop(heartbeat_tx);
    writer.abort();
    Ok(())
}
/// Where the gateway finds the programs it runs: `twr` (a hub, a device's shell, the CLI
/// calls of a device) and the per-device companion (the desktop bridge).
pub enum Programs {
    /// The desktop bundle: `twr` and `twr-desktop-bridge` beside this executable, or in
    /// `--binary-dir`.
    Bundle,
    /// `twr gateway`: this executable is `twr`, and its hidden `desktop-bridge` subcommand is
    /// the companion (or `twr` in `--binary-dir`, run the same way).
    Twr,
}

impl Programs {
    /// `twr` and the companion's command, from `binary_dir` when given.
    fn resolve(&self, binary_dir: Option<&Path>) -> Result<(PathBuf, remote::companion::Command)> {
        let this = std::env::current_exe()?;
        let dir = match binary_dir {
            Some(dir) => dir.to_owned(),
            None => this.parent().context("Binary directory")?.to_owned(),
        };
        Ok(match self {
            Programs::Bundle => (
                dir.join("twr"),
                remote::companion::Command::program(dir.join("twr-desktop-bridge")),
            ),
            Programs::Twr => {
                let twr = if binary_dir.is_some() {
                    dir.join("twr")
                } else {
                    this
                };
                let companion = remote::companion::Command {
                    program: twr.clone(),
                    args: vec![BRIDGE_SUBCOMMAND.into()],
                };
                (twr, companion)
            }
        })
    }
}

/// `twr`'s hidden subcommand that runs the desktop bridge ([`crate::bridge::run`]).
pub const BRIDGE_SUBCOMMAND: &str = "desktop-bridge";

/// The gateway, given its arguments: what `twr-mobile-gateway` and `twr gateway` run.
pub async fn run(args: Args, programs: Programs) -> Result<()> {
    // The app starts the gateway, which outlives it and may start a hub: neither may keep
    // the app's crash handler.
    crate::runtime::forget_inherited_crash_handler();
    let _ = rustls::crypto::ring::default_provider().install_default();
    if let Some(path) = args.log_file.clone() {
        remote::gateway_lifecycle::set_log_file(path);
    }
    let invitation_scopes = pairing_scopes(&args);
    let mode = if args.development {
        Mode::Development
    } else if args.tls_cert.is_some() {
        Mode::Certificate
    } else {
        Mode::Pinned
    };
    anyhow::ensure!(
        !args.development
            || (cfg!(debug_assertions) && args.listen.is_none_or(|l| l.ip().is_loopback())),
        "Cleartext development transport requires a debug build on loopback"
    );
    let (binary, companion) = programs.resolve(args.binary_dir.as_deref())?;
    let paths = twr_core::paths::Paths::from_env_in(Some("default"))?;
    let mobile_dir = paths.config_file.with_file_name("mobile");
    // Pinned mode keeps its settings: `--address` is saved for later runs.
    let settings = match mode {
        Mode::Pinned => Some(settings_with(&mobile_dir, args.address.as_deref())?),
        _ => None,
    };
    if args.gateway_info {
        println!("{}", gateway_info(&mobile_dir).await?);
        return Ok(());
    }
    if args.stop_gateway {
        remote::gateway_lifecycle::stop(
            &mobile_dir,
            args.expected_instance
                .as_deref()
                .context("Gateway instance required")?,
        )
        .await?;
        println!("{}", json!({"stopped":true}));
        return Ok(());
    }
    if args.cancel_invitation {
        use tokio::io::AsyncReadExt;
        let mut bytes = Vec::new();
        tokio::io::stdin()
            .take(1025)
            .read_to_end(&mut bytes)
            .await?;
        anyhow::ensure!(
            bytes.len() <= 1024,
            "Cancellation request exceeds its limit"
        );
        let request: Value = serde_json::from_slice(&bytes)?;
        let code = request["code"]
            .as_str()
            .context("Invitation code required")?;
        let cancelled = match remote::gateway_lifecycle::info(&mobile_dir).await {
            Ok(info) if info["running"] == true => {
                remote::gateway_lifecycle::cancel_invitation(&mobile_dir, code).await?
            }
            _ => {
                let mut owner = owner_client().await?;
                owner
                    .call("device.pair_cancel", json!({"code":code}))
                    .await?["cancelled"]
                    .as_bool()
                    .context("Invalid cancellation reply")?
            }
        };
        println!("{}", json!({"cancelled":cancelled}));
        return Ok(());
    }
    let auth = Auth::open(&mobile_dir)?;
    let config = twr_core::config::Config::load_from(&paths)?;
    crate::runtime::ensure_hub(&paths, &config.socket_path(&paths), &binary).await?;
    let mut local = owner_client().await?;
    if let Some(id) = args.revoke {
        local.call("device.revoke", json!({"id":id})).await?;
        return Ok(());
    }
    if args.list_devices {
        println!("{}", local.call("device.list", json!({})).await?);
        return Ok(());
    }
    // Only an invitation: now, for the gateway that runs (a pinned one's carries its pin and
    // routes). A gateway that serves too begins one only once nothing of its start can fail
    // any more (below), so one that is about to exit never shows a code.
    if args.pair && args.pair_only {
        let pinned = running_pinned(&mobile_dir).await?;
        let announced = announce_pairing(
            &mut local,
            &invitation_scopes,
            args.pair_json,
            &mobile_dir,
            pinned.as_ref(),
        );
        announced.await?;
        return Ok(());
    }
    let push =
        remote::push::Push::new(&mobile_dir, args.push_config.as_deref(), args.push_test_log)?;
    // Pinned mode: the identity, made the first time.
    let identity = match mode {
        Mode::Pinned => Some(identity::Identity::ensure(&mobile_dir)?),
        _ => None,
    };
    let default_port = |ip: IpAddr| SocketAddr::new(ip, DEFAULT_PORT);
    let wanted: Vec<(SocketAddr, bool)> = match (args.listen, &settings) {
        (Some(listen), _) => vec![(listen, true)],
        (None, Some(settings)) if !settings.listen.is_empty() => {
            settings.listen.iter().map(|l| (*l, true)).collect()
        }
        // Every IPv4 address, and IPv6's too where it binds.
        (None, Some(_)) => vec![
            (
                default_port(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                true,
            ),
            (
                default_port(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
                false,
            ),
        ],
        (None, None) => vec![(
            default_port(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
            true,
        )],
    };
    let mut listeners = Vec::new();
    for (address, required) in wanted {
        match bind(address) {
            Ok(listener) => listeners.push(listener),
            Err(error) if !required => {
                crate::gateway_log!("Not listening on {address}: {error}")
            }
            Err(error) => return Err(error).with_context(|| format!("listening on {address}")),
        }
    }
    let bound: Vec<SocketAddr> = listeners
        .iter()
        .map(std::net::TcpListener::local_addr)
        .collect::<std::io::Result<_>>()?;
    let listen = bound
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let (cert_path, key_path) = match (&identity, &args.tls_cert, &args.tls_key) {
        (Some(_), _, _) => (
            Some(mobile_dir.join(identity::CERT_FILE)),
            Some(mobile_dir.join(identity::KEY_FILE)),
        ),
        (None, cert, key) => (cert.clone(), key.clone()),
    };
    let mut info = remote::gateway_lifecycle::Info::new(
        push.gateway_id.clone(),
        listen.clone(),
        cert_path.as_deref(),
        key_path.as_deref(),
    )?;
    info.mode = mode.name().into();
    info.pin = identity.as_ref().map(|i| i.pin.clone());
    let mut lifecycle = remote::gateway_lifecycle::Lifecycle::acquire(&mobile_dir, info)?;
    let receipts = remote::receipts::Receipts::new(&paths.root().data_dir.join("mobile"))?;
    let pinned = identity.as_ref().map(|identity| Pinned {
        pin: identity.pin.clone(),
        dir: mobile_dir.clone(),
        listeners: bound.clone(),
    });
    let server = Arc::new(Server::new(
        auth,
        push,
        receipts,
        Some(local),
        (binary, companion),
        args.development,
        remote::limits::PRE_AUTH,
        pinned,
    ));
    start_observer(server.clone(), "default".into()).await;
    for workspace in server.push.workspaces().await {
        start_observer(server.clone(), workspace).await;
    }
    let cleanup = server.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(10));
        loop {
            tick.tick().await;
            let _ = cleanup.receipts.prune();
            let active: Vec<_> = cleanup.sessions.lock().await.values().cloned().collect();
            for slot in active {
                slot.session.uploads.lock().await.retain(|id, upload| {
                    let keep = upload.busy || upload.created.elapsed() < Duration::from_secs(900);
                    if !keep && !upload.committed {
                        slot.session.upload_quota.lock().unwrap().release(id);
                    }
                    keep
                });
            }
            let paired = paired_ids(&cleanup.auth);
            let mut sessions = cleanup.sessions.lock().await;
            sessions.retain(|id, slot| {
                keep_session(
                    id,
                    slot.last_seen.load(Ordering::SeqCst),
                    paired.as_ref(),
                    remote::auth::now(),
                )
            });
        }
    });
    let connections = server.connections.clone();
    let app = router(server).into_make_service_with_connect_info::<SocketAddr>();
    crate::gateway_log!(
        "twr mobile gateway listening on {listen} ({} mode)",
        mode.name()
    );
    // Every listener serves, until one fails or the gateway is asked to stop.
    type Serving = std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>>;
    let mut serving: Vec<Serving> = Vec::new();
    match mode {
        Mode::Development => {
            for listener in listeners {
                let http = http_server(
                    listener,
                    axum_server::accept::DefaultAcceptor,
                    connections.clone(),
                )?;
                serving.push(Box::pin(http.serve(app.clone())));
            }
        }
        Mode::Pinned => {
            let identity = identity.context("pinned mode has an identity")?;
            let config = axum_server::tls_rustls::RustlsConfig::from_pem(
                identity.cert_pem.clone().into_bytes(),
                identity.key_pem.clone().into_bytes(),
            )
            .await?;
            // The certificate is made again, for the same key, before it ends: the pin stays.
            let (reload, metadata, dir) =
                (config.clone(), lifecycle.metadata(), mobile_dir.clone());
            tokio::spawn(async move {
                let mut served = identity.cert_pem;
                let mut tick = tokio::time::interval(Duration::from_secs(6 * 3600));
                loop {
                    tick.tick().await;
                    let renewed = match identity::Identity::ensure(&dir) {
                        Ok(renewed) if renewed.cert_pem != served => renewed,
                        Ok(_) => continue,
                        Err(error) => {
                            crate::gateway_log!(
                                "The gateway's identity could not be checked: {error:#}"
                            );
                            continue;
                        }
                    };
                    let loaded = reload.reload_from_pem(
                        renewed.cert_pem.clone().into_bytes(),
                        renewed.key_pem.into_bytes(),
                    );
                    match loaded.await {
                        Ok(()) => {
                            use sha2::Digest;
                            let digest = hex::encode(sha2::Sha256::digest(&renewed.cert_pem));
                            let _ = metadata.certificate(
                                digest,
                                &dir.join(identity::CERT_FILE),
                                &dir.join(identity::KEY_FILE),
                            );
                            served = renewed.cert_pem;
                        }
                        Err(error) => crate::gateway_log!(
                            "The renewed identity certificate could not be loaded: {error}"
                        ),
                    }
                }
            });
            for listener in listeners {
                let tls = axum_server::tls_rustls::RustlsAcceptor::new(config.clone());
                let https = http_server(listener, tls, connections.clone())?;
                serving.push(Box::pin(https.serve(app.clone())));
            }
        }
        Mode::Certificate => {
            let cert = args
                .tls_cert
                .context("certificate mode has a certificate")?;
            let key = args.tls_key.context("certificate mode has a key")?;
            use sha2::Digest;
            let cert_bytes = std::fs::read(&cert)?;
            let key_bytes = std::fs::read(&key)?;
            let config = axum_server::tls_rustls::RustlsConfig::from_pem(
                cert_bytes.clone(),
                key_bytes.clone(),
            )
            .await?;
            let reload = config.clone();
            let metadata = lifecycle.metadata();
            metadata.certificate(hex::encode(sha2::Sha256::digest(&cert_bytes)), &cert, &key)?;
            let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .context("Register gateway TLS reload signal")?;
            tokio::spawn(async move {
                let signature = |certificate: &[u8], private_key: &[u8]| {
                    let mut digest = sha2::Sha256::new();
                    digest.update(certificate);
                    digest.update(private_key);
                    digest.finalize().to_vec()
                };
                let mut previous = signature(&cert_bytes, &key_bytes);
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                loop {
                    tokio::select! { _ = tick.tick() => {}, _ = hangup.recv() => {} }
                    let (Ok(certificate), Ok(private_key)) =
                        (std::fs::read(&cert), std::fs::read(&key))
                    else {
                        let _ = metadata.renewal("failed", Some("Certificate or private key could not be read; the previous TLS context remains active"));
                        crate::gateway_log!("Gateway TLS renewal files could not be read; the previous certificate remains active");
                        continue;
                    };
                    let current = signature(&certificate, &private_key);
                    if current == previous {
                        continue;
                    }
                    // Parse exactly the bytes whose digest is published. Certbot's symlink
                    // rotation cannot make metadata claim a certificate we have not loaded.
                    let _ = metadata.renewal("pending", None);
                    match reload
                        .reload_from_pem(certificate.clone(), private_key)
                        .await
                    {
                        Ok(()) => {
                            previous = current;
                            let _ = metadata.certificate(
                                hex::encode(sha2::Sha256::digest(&certificate)),
                                &cert,
                                &key,
                            );
                        }
                        Err(_) => {
                            let _ = metadata.renewal("failed", Some("Certificate or private key could not be loaded; the previous TLS context remains active"));
                            crate::gateway_log!("Gateway TLS renewal could not be loaded; the previous certificate remains active");
                        }
                    }
                }
            });
            for listener in listeners {
                let tls = axum_server::tls_rustls::RustlsAcceptor::new(config.clone());
                let https = http_server(listener, tls, connections.clone())?;
                serving.push(Box::pin(https.serve(app.clone())));
            }
        }
    }
    if args.pair {
        let pinned = running_pinned(&mobile_dir).await?;
        let mut owner = owner_client().await?;
        let announced = announce_pairing(
            &mut owner,
            &invitation_scopes,
            args.pair_json,
            &mobile_dir,
            pinned.as_ref(),
        );
        announced.await?;
    }
    tokio::select! {
        (result, _, _) = futures::future::select_all(serving) => { result?; },
        _ = lifecycle.stopping() => {},
    }
    Ok(())
}

/// The gateway's port when none is given.
pub const DEFAULT_PORT: u16 = 47321;

/// How a gateway serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Its own key, which phones pin (no `--tls-cert`).
    Pinned,
    /// `--tls-cert` and `--tls-key`, which phones trust as the system does.
    Certificate,
    /// Cleartext loopback, debug builds only.
    Development,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Pinned => "pinned",
            Mode::Certificate => "certificate",
            Mode::Development => "development",
        }
    }
}

/// A listener on `address`: an IPv6 one for IPv6 only, so that it and an IPv4 one on the same
/// port go together, and an IPv4 peer is never seen as a mapped IPv6 address.
fn bind(address: SocketAddr) -> std::io::Result<std::net::TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )?;
    if address.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.bind(&address.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// The pinned gateway's saved settings in `dir`, made (with autostart on) if there were none,
/// with `address` saved when given.
fn settings_with(dir: &Path, address: Option<&str>) -> Result<settings::Settings> {
    let saved = settings::Settings::load(dir)?;
    let mut settings = saved.clone().unwrap_or(settings::Settings {
        autostart: true,
        ..Default::default()
    });
    if let Some(address) = address {
        settings.set_address(address)?;
    }
    if saved.as_ref() != Some(&settings) {
        settings.save(dir)?;
    }
    Ok(settings)
}

/// What an invitation of a pinned gateway carries beside its code.
pub struct PinnedInvitation {
    pub pin: String,
    pub iroh: String,
    pub routes: Vec<twr_core::phone_pairing::Route>,
}

/// The listeners `listen` names (`Info::listen`: comma-separated).
fn listeners_of(listen: &str) -> Vec<SocketAddr> {
    listen
        .split(',')
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// The running gateway of `dir`'s invitation parts when it runs in pinned mode; `None` when
/// none runs, or one runs in another mode.
pub async fn running_pinned(dir: &Path) -> Result<Option<PinnedInvitation>> {
    let info = remote::gateway_lifecycle::info(dir).await?;
    if info["running"] != true || info["mode"] != "pinned" {
        return Ok(None);
    }
    let identity = identity::Identity::load(dir)?.context("the gateway's identity is missing")?;
    anyhow::ensure!(
        info["pin"] == identity.pin.as_str(),
        "the running gateway serves another key than its directory holds: restart it"
    );
    let listeners = listeners_of(info["listen"].as_str().unwrap_or(""));
    Ok(Some(PinnedInvitation {
        pin: identity.pin,
        iroh: identity.iroh,
        routes: routes::current(dir, &listeners),
    }))
}

/// `--gateway-info`: the lifecycle's record, and the pin and routes of a pinned gateway (one
/// that runs, or the one that would start when none does).
pub async fn gateway_info(dir: &Path) -> Result<Value> {
    let mut info = remote::gateway_lifecycle::info(dir).await?;
    let running = info["running"] == true;
    if running && info["mode"] != "pinned" {
        return Ok(info);
    }
    let listeners = if running {
        listeners_of(info["listen"].as_str().unwrap_or(""))
    } else {
        match settings::Settings::load(dir)? {
            Some(settings) if !settings.listen.is_empty() => settings.listen,
            _ => vec![
                SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), DEFAULT_PORT),
                SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), DEFAULT_PORT),
            ],
        }
    };
    if let Some(identity) = identity::Identity::load(dir)? {
        info["pin"] = json!(identity.pin);
        let routes: Vec<String> = routes::current(dir, &listeners)
            .iter()
            .map(ToString::to_string)
            .collect();
        info["routes"] = json!(routes);
    }
    Ok(info)
}

/// This machine's name, as an invitation shows it: its host name up to the first dot.
pub fn hub_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]);
    let short = name.split('.').next().unwrap_or("").trim();
    if ok && !short.is_empty() {
        short.chars().take(80).collect()
    } else {
        "twr".into()
    }
}

/// The JSON line `--pair --json` prints for code `code`, ending at `expires_at` (seconds),
/// with `scopes`: in pinned mode (`pinned`) it carries the version 2 invitation and its link
/// too (docs/design/phone-pairing.md#gateway-command-line-output).
pub fn pair_json(
    code: &str,
    expires_at: u64,
    scopes: &[&str],
    gateway_id: &str,
    pinned: Option<&PinnedInvitation>,
) -> Result<Value> {
    let mut line = json!({
        "code": code,
        "expires_at": expires_at,
        "scopes": scopes,
        "gateway_id": gateway_id,
    });
    if let Some(pinned) = pinned {
        anyhow::ensure!(
            !pinned.routes.is_empty(),
            "This machine has no address a phone could reach the gateway at: give one with \
             --address"
        );
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        let invitation = twr_core::phone_pairing::Invitation::new(
            gateway_id,
            &hub_name(),
            &pinned.pin,
            Some(pinned.iroh.clone()),
            code,
            expires_at * 1000,
            &pinned.routes,
            &scopes,
        )?;
        line["link"] = json!(invitation.to_link());
        line["invitation"] = serde_json::to_value(&invitation)?;
    }
    Ok(line)
}

/// Begins a device invitation with `scopes` on the hub (`device.pair_begin`, as the owner
/// through `owner`) and prints its code: as JSON with `json` (with the gateway id from the
/// gateway directory `dir`, and a pinned gateway's invitation), else as lines.
async fn announce_pairing(
    owner: &mut twr_core::client::Client,
    scopes: &[&str],
    json: bool,
    dir: &Path,
    pinned: Option<&PinnedInvitation>,
) -> Result<()> {
    let pairing_started = remote::auth::now();
    let begun = owner
        .call("device.pair_begin", json!({"scopes":scopes}))
        .await?;
    let code = begun["code"].as_str().context("Invalid pairing reply")?;
    let expires_at = pairing_started
        + begun["expires_in"]
            .as_u64()
            .unwrap_or(300)
            .min(twr_core::devices::PAIRING_SECONDS);
    let gateway_id = std::fs::read_to_string(dir.join("gateway-id")).unwrap_or_default();
    let line = pair_json(code, expires_at, scopes, gateway_id.trim(), pinned);
    let line = match line {
        Ok(line) => line,
        Err(error) => {
            // No invitation to show: none stays open.
            let _ = owner.call("device.pair_cancel", json!({"code":code})).await;
            return Err(error);
        }
    };
    if json {
        println!("{line}");
    } else {
        println!(
            "Pairing code (one use, five minutes): {}",
            twr_core::phone_pairing::display_code(code)
        );
        if let Some(link) = line["link"].as_str() {
            println!("Link: {link}");
            for route in line["invitation"]["routes"]
                .as_array()
                .into_iter()
                .flatten()
            {
                println!("Route: {}", route.as_str().unwrap_or(""));
            }
        }
    }
    Ok(())
}
/// `GET /health`: the gateway's id, and in pinned mode the pin it serves.
async fn health(State(server): State<Shared>) -> Json<Value> {
    let mut health =
        json!({"service":"twr-mobile","protocol":2,"gateway_id":server.push.gateway_id});
    if let Some(pinned) = &server.pinned {
        health["pin"] = json!(pinned.pin);
    }
    Json(health)
}
fn router(server: Shared) -> Router {
    Router::new()
        .route(
            "/v1/pair",
            post(pair).layer(axum::middleware::from_fn_with_state(
                server.clone(),
                pair_deadline,
            )),
        )
        .route(
            "/v1/pair/proof",
            get(pair_proof).layer(axum::middleware::from_fn_with_state(
                server.clone(),
                pair_deadline,
            )),
        )
        .route("/v1/socket", get(upgrade))
        .route("/health", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(4096))
        .with_state(server)
}
/// The gateway's HTTP server on `listener`: `inner` (TLS, or nothing on development loopback)
/// runs behind the unauthenticated connection limits, and hyper bounds reading a request
/// head, an idle keep-alive connection's included. hyper only runs that bound with a timer.
fn http_server<A>(
    listener: std::net::TcpListener,
    inner: A,
    connections: Arc<remote::limits::Connections>,
) -> std::io::Result<axum_server::Server<SocketAddr, remote::limits::Guarded<A>>> {
    listener.set_nonblocking(true)?;
    let limits = connections.limits();
    let mut server =
        axum_server::from_tcp(listener)?.acceptor(remote::limits::Guarded::new(inner, connections));
    server
        .http_builder()
        .http1()
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(limits.header_read);
    server
        .http_builder()
        .http2()
        .timer(hyper_util::rt::TokioTimer::new())
        .max_concurrent_streams(32);
    Ok(server)
}

async fn start_observer(server: Shared, workspace: String) {
    if !server.observers.lock().await.insert(workspace.clone()) {
        return;
    }
    tokio::spawn(async move {
        loop {
            if let Err(error) = observe_push(&server, &workspace).await {
                crate::gateway_log!("Push observer for {workspace}: {error}");
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    });
}
async fn observe_push(server: &Server, workspace: &str) -> Result<()> {
    use twr_core::{
        client::Client,
        client_state::{ClientFold, FOLD},
        config::Config,
        notify::{still_due, Held, Policy},
    };
    let paths = twr_core::paths::Paths::from_env_in(Some(workspace))?;
    let config = Config::load_from(&paths)?;
    let mut client = Client::connect(&config.socket_path(&paths)).await?;
    let hello = client.hello(json!({"client":"mobile-notify"})).await?;
    anyhow::ensure!(
        hello["identity"]
            .as_str()
            .is_some_and(|s| s.starts_with("user:")),
        "Push observer is not a user client"
    );
    let mut sub = client
        .subscribe(
            "control.subscribe",
            json!({"since":0,"compact":true,"snapshot":true}),
        )
        .await?;
    if sub.ack().get("snapshot").is_some() && sub.ack()["snapshot_fold"] != FOLD {
        let mut client = Client::connect(&config.socket_path(&paths)).await?;
        client.hello(json!({"client":"mobile-notify"})).await?;
        sub = client
            .subscribe("control.subscribe", json!({"since":0,"compact":true}))
            .await?;
    }
    let head = sub.ack()["head"].as_u64().unwrap_or(0);
    let mut fold: ClientFold = if let Some(snapshot) = sub.ack().get("snapshot") {
        serde_json::from_value(snapshot.clone())?
    } else {
        ClientFold::default()
    };
    let mut policy = Policy::adopt(&config.notify.events, &fold.policy);
    let mut held = Held::default();
    let mut last_sent = None;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
         item=sub.next()=>{let item=item?.context("Hub subscription closed")?;fold.apply(&item.event,item.gseq);if item.gseq.is_some_and(|g|g<=head){policy.recall(&item.event,item.gseq,&fold.registry,&fold.attention);}else if let Some(candidate)=policy.observe(&item.event,item.gseq,&fold.registry,&fold.attention){held.add(candidate,std::time::Instant::now());}}
         _=tick.tick()=>{
          let now = std::time::Instant::now();
          if held.next_due(last_sent).is_none_or(|due| due > now) {continue;}
          let mut due:Vec<_>=held.take_due(now).into_iter().filter(|c|still_due(c,&fold.registry,&fold.attention)).collect();
          due.sort_by_key(|c|c.event);
          if due.is_empty(){continue;}
          let slots:Vec<_>=server.sessions.lock().await.iter().map(|(id,slot)|(id.clone(),slot.clone())).collect();
          let mut exclude=std::collections::HashSet::new();
          for(id,slot)in slots {if slot.foreground.load(Ordering::SeqCst)&&slot.session.connection.read().await.workspace==workspace{exclude.insert(id);}}
          last_sent=Some(now);
          if let Err(error)=server.push.deliver(&server.auth,workspace,&exclude,&due).await{crate::gateway_log!("Push delivery failed: {error}");}

         }
        }
    }
}

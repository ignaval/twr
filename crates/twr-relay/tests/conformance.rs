//! The relay in-process on loopback, against the client, section by section of
//! docs/relay-protocol.md.

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use twr_relay::client::{self, Closed, HubConfig, HubSession};
use twr_relay::proto::{self, Incoming, Msg};
use twr_relay::server::config::Config;
use twr_relay::server::{self, probe, Running};
use twr_relay::{admit, names, IdentityKey, RelayUrl};

struct Relay {
    running: Option<Running>,
    url: RelayUrl,
    _dir: tempfile::TempDir,
}

impl Relay {
    /// The relay by its URL, connected through its loopback address.
    fn t(&self) -> client::Target {
        client::Target::new(self.url.clone()).connect_to(self.r().addrs[0])
    }
    fn r(&self) -> &Running {
        self.running.as_ref().unwrap()
    }
    async fn stop(mut self) {
        self.running.take().unwrap().shutdown().await;
    }
}

/// A relay with a DNS name (`relay.test`) used through 127.0.0.1, self-signed, iroh off.
async fn named(extra: &str) -> Relay {
    start(&format!(
        "[relay]\nhostname = \"relay.test\"\npublic_host = \"127.0.0.1\"\nlisten = [\"127.0.0.1:0\"]\nmetrics_listen = \"127.0.0.1:0\"\nregion = \"test\"\n[tls]\nmode = \"self-signed\"\n[iroh]\nenabled = false\n{extra}"
    ))
    .await
}

/// A relay without DNS: phones use `relay.invalid`, iroh has its own port.
async fn dnsless(extra: &str) -> Relay {
    start(&format!(
        "[relay]\npublic_host = \"127.0.0.1\"\nlisten = [\"127.0.0.1:0\"]\nmetrics_listen = \"127.0.0.1:0\"\n[tls]\nmode = \"self-signed\"\n[iroh]\nlisten = \"127.0.0.1:0\"\nquic_listen = \"127.0.0.1:0\"\n{extra}"
    ))
    .await
}

async fn start(text: &str) -> Relay {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg: Config = toml::from_str(text).unwrap();
    cfg.relay.state_dir = dir.path().to_path_buf();
    let running = server::start(cfg, "0.0.0-test").await.unwrap();
    let url = running.relay_url();
    Relay {
        running: Some(running),
        url,
        _dir: dir,
    }
}

fn hub_cfg(r: &Relay) -> HubConfig {
    HubConfig::new(r.t(), "twr test")
}

async fn connect(r: &Relay) -> (HubSession, Arc<IdentityKey>) {
    let key = Arc::new(IdentityKey::generate_p256().unwrap());
    let s = HubSession::connect(&hub_cfg(r), &key).await.unwrap();
    (s, key)
}

/// What the hub learned about a phone stream (§5.6).
struct Meta {
    phone: Option<String>,
    mode: String,
    remote: String,
}

/// Accepts one phone stream on the hub and echoes over the hub's pinned TLS.
fn echo_hub(
    mut session: HubSession,
    key: Arc<IdentityKey>,
) -> tokio::task::JoinHandle<(HubSession, Meta)> {
    tokio::spawn(async move {
        let s = session.accept().await.expect("a phone stream");
        let meta = Meta {
            phone: s.phone.clone(),
            mode: s.mode.clone(),
            remote: s.remote.clone(),
        };
        let tls = probe::identity_tls(&key, &[b"t"]).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let mut t = acceptor.accept(s.io).await.unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                match t.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if t.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                        let _ = t.flush().await;
                    }
                }
            }
        });
        (session, meta)
    })
}

async fn echo_once(
    route: &proto::PhoneRoute,
    pin: &[u8; 32],
    relay_key: &[u8; 32],
) -> anyhow::Result<()> {
    let mut t = client::phone_connect(route, pin, relay_key, &[b"t"]).await?;
    t.write_all(b"hello through the relay").await?;
    t.flush().await?;
    let mut buf = [0u8; 23];
    t.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"hello through the relay");
    Ok(())
}

async fn metrics(r: &Relay) -> String {
    let addr = r.r().metrics_addr.unwrap();
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(b"GET /metrics HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    out
}

fn refusals(text: &str, reason: &str) -> u64 {
    let needle = format!("twr_relay_refusals_total{{reason=\"{reason}\"}} ");
    text.lines()
        .find_map(|l| l.strip_prefix(&needle))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

// §5: hub sessions --------------------------------------------------------------------------

#[tokio::test]
async fn hub_session_and_welcome() {
    let r = named("").await;
    let (s, key) = connect(&r).await;
    let w = &s.welcome;
    assert_eq!(w.hub, names::hub_name(&key.pin()));
    assert_eq!(w.region, "test");
    assert_eq!(w.phone_routes.len(), 1);
    assert_eq!(w.phone_routes[0].suffix, "relay.test");
    assert_eq!(w.phone_routes[0].host, "127.0.0.1");
    assert_eq!(w.phone_routes[0].tags, vec![1]);
    assert_eq!(
        w.relay_pin.as_deref(),
        Some(names::pin_text(&r.r().self_pin.unwrap()).as_str())
    );
    assert!(w.records_url.as_deref().unwrap().ends_with("/pkarr"));
    assert_eq!(w.limits.phones, 32);
    assert_eq!(s.set_phones(vec![]).await.unwrap(), 0);
    let too_many: Vec<_> = (0..33)
        .map(|_| client::new_phone_credential().unwrap().0)
        .collect();
    let e = s.set_phones(too_many).await.unwrap_err().to_string();
    assert!(e.contains("too_many_phones"), "{e}");
    r.stop().await;
}

#[tokio::test]
async fn ed25519_hubs_work_too() {
    let r = named("").await;
    let key = IdentityKey::generate_ed25519().unwrap();
    let s = HubSession::connect(&hub_cfg(&r), &key).await.unwrap();
    assert_eq!(s.welcome.hub, names::hub_name(&key.pin()));
    r.stop().await;
}

/// A raw hub connection for protocol-level tests.
async fn raw_control(
    r: &Relay,
) -> (
    twr_relay::client::RelayIo,
    [u8; 32],
    tokio_util::sync::CancellationToken,
) {
    let cfg = twr_relay::tls::client_config(r.url.pin, &[proto::ALPN_V1]);
    let tcp = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    let ip = rustls::pki_types::ServerName::IpAddress(
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap().into(),
    );
    let tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(ip, tcp)
        .await
        .unwrap();
    let exporter: [u8; 32] = tls
        .get_ref()
        .1
        .export_keying_material([0u8; 32], proto::EXPORTER_LABEL, Some(proto::ALPN_V1))
        .unwrap();
    let (open_tx, open_rx) = tokio::sync::mpsc::channel(1);
    let (in_tx, _in_rx) = tokio::sync::mpsc::channel(1);
    let cancel = tokio_util::sync::CancellationToken::new();
    tokio::spawn(twr_relay::mux::drive(
        tls,
        twr_relay::mux::hub_config(),
        yamux::Mode::Client,
        open_rx,
        in_tx,
        cancel.clone(),
    ));
    let (tx, rx) = tokio::sync::oneshot::channel();
    open_tx.send(tx).await.unwrap();
    let ctl = rx.await.unwrap().unwrap();
    std::mem::forget(open_tx);
    std::mem::forget(_in_rx);
    (ctl, exporter, cancel)
}

#[tokio::test]
async fn bad_hello_is_refused() {
    let r = named("").await;
    let key = IdentityKey::generate_p256().unwrap();

    // A signature over the wrong exporter.
    let (mut ctl, _exporter, _c) = raw_control(&r).await;
    let sig = key.sign(&proto::hub_auth_message(&[0u8; 32])).unwrap();
    let hello = Msg::Hello {
        spki: names::b64(&key.spki()),
        sig: names::b64(&sig),
        client: "t".into(),
        features: vec![],
    };
    proto::write_frame(&mut ctl, &hello).await.unwrap();
    match proto::read_frame(&mut ctl).await.unwrap() {
        Some(Incoming::Msg(Msg::Error {
            code, fatal: true, ..
        })) => assert_eq!(code, "bad_signature"),
        other => panic!("{other:?}"),
    }

    // Something other than hello first.
    let (mut ctl, _, _c) = raw_control(&r).await;
    proto::write_frame(&mut ctl, &Msg::Ping { n: 1 })
        .await
        .unwrap();
    match proto::read_frame(&mut ctl).await.unwrap() {
        Some(Incoming::Msg(Msg::Error { code, .. })) => assert_eq!(code, "hello_required"),
        other => panic!("{other:?}"),
    }

    // A key type the protocol doesn't take.
    let (mut ctl, exporter, _c) = raw_control(&r).await;
    let sig = key.sign(&proto::hub_auth_message(&exporter)).unwrap();
    let mut spki = key.spki();
    spki[3] ^= 0xff;
    let hello = Msg::Hello {
        spki: names::b64(&spki),
        sig: names::b64(&sig),
        client: "t".into(),
        features: vec![],
    };
    proto::write_frame(&mut ctl, &hello).await.unwrap();
    match proto::read_frame(&mut ctl).await.unwrap() {
        Some(Incoming::Msg(Msg::Error { code, .. })) => assert_eq!(code, "bad_key"),
        other => panic!("{other:?}"),
    }

    // A correct one, then an unknown message type and a ping.
    let (mut ctl, exporter, _c) = raw_control(&r).await;
    let sig = key.sign(&proto::hub_auth_message(&exporter)).unwrap();
    let hello = Msg::Hello {
        spki: names::b64(&key.spki()),
        sig: names::b64(&sig),
        client: "t".into(),
        features: vec![],
    };
    proto::write_frame(&mut ctl, &hello).await.unwrap();
    assert!(matches!(
        proto::read_frame(&mut ctl).await.unwrap(),
        Some(Incoming::Msg(Msg::Welcome(_)))
    ));
    let body = br#"{"type":"from_the_future","x":1}"#;
    ctl.write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    ctl.write_all(body).await.unwrap();
    match proto::read_frame(&mut ctl).await.unwrap() {
        Some(Incoming::Msg(Msg::Error {
            code,
            fatal: false,
            ref_,
            ..
        })) => {
            assert_eq!(code, "unsupported");
            assert_eq!(ref_.as_deref(), Some("from_the_future"));
        }
        other => panic!("{other:?}"),
    }
    proto::write_frame(&mut ctl, &Msg::Ping { n: 9 })
        .await
        .unwrap();
    assert_eq!(
        proto::read_frame(&mut ctl).await.unwrap(),
        Some(Incoming::Msg(Msg::Pong { n: 9 }))
    );

    let m = metrics(&r).await;
    assert!(refusals(&m, "hub_auth") >= 3, "{m}");
    r.stop().await;
}

#[tokio::test]
async fn tls12_hubs_are_refused() {
    let r = named("").await;
    let mut cfg = rustls::ClientConfig::builder_with_provider(twr_relay::tls::provider())
        .with_protocol_versions(&[&rustls::version::TLS12])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(twr_relay::tls::PinVerifier::new(
            r.url.pin.unwrap(),
        )))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![proto::ALPN_V1.to_vec()];
    let tcp = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    let ip = rustls::pki_types::ServerName::IpAddress(
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap().into(),
    );
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(ip, tcp)
        .await
        .unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(5), tls.read(&mut buf))
        .await
        .unwrap()
        .unwrap_or(0);
    assert_eq!(n, 0, "the relay closes a TLS 1.2 hub connection");
    r.stop().await;
}

#[tokio::test]
async fn a_newer_session_replaces_the_old() {
    let r = named("").await;
    let key = Arc::new(IdentityKey::generate_p256().unwrap());
    let first = HubSession::connect(&hub_cfg(&r), &key).await.unwrap();
    let _second = HubSession::connect(&hub_cfg(&r), &key).await.unwrap();
    let why = tokio::time::timeout(Duration::from_secs(5), first.closed())
        .await
        .unwrap();
    assert_eq!(why, Closed::Refused("replaced".into()));
    r.stop().await;
}

#[tokio::test]
async fn new_hub_names_are_rate_limited_per_address() {
    let r = named("[limits]\nnew_hubs_per_ip_per_hour = 2\n").await;
    let _a = connect(&r).await;
    let (_b, kb) = connect(&r).await;
    let key = IdentityKey::generate_p256().unwrap();
    let e = HubSession::connect(&hub_cfg(&r), &key)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("rate_limited"), "{e}");
    // A name already seen from this address still connects.
    HubSession::connect(&hub_cfg(&r), &kb).await.unwrap();
    r.stop().await;
}

#[tokio::test]
async fn drain_on_shutdown() {
    let r = named("").await;
    let (s, _) = connect(&r).await;
    let mut r = r;
    let run = r.running.take().unwrap();
    let stop = tokio::spawn(run.shutdown());
    let why = tokio::time::timeout(Duration::from_secs(5), s.closed())
        .await
        .unwrap();
    assert_eq!(why, Closed::Drain(30));
    stop.await.unwrap();
}

// §6: phone passthrough --------------------------------------------------------------------

#[tokio::test]
async fn phone_round_trip_and_refusals() {
    let r = named("").await;
    let (s, key) = connect(&r).await;
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    let id = cred.id.clone();
    assert_eq!(s.set_phones(vec![cred]).await.unwrap(), 1);
    let route = s.welcome.phone_routes[0].clone();
    let pin = key.pin();
    let hub = echo_hub(s, key.clone());
    echo_once(&route, &pin, &relay_key).await.unwrap();
    let (s, meta) = hub.await.unwrap();
    assert_eq!(meta.phone.as_deref(), Some(id.as_str()));
    assert_eq!(meta.mode, "sni");
    assert!(meta.remote.starts_with("127.0.0.1:"));

    // A wrong relay key: the relay closes without a TLS answer.
    assert!(client::phone_connect(&route, &pin, &[7u8; 32], &[b"t"])
        .await
        .is_err());
    // An unknown hub.
    let other = IdentityKey::generate_p256().unwrap().pin();
    assert!(client::phone_connect(&route, &other, &relay_key, &[b"t"])
        .await
        .is_err());
    // A label that isn't one.
    let tcp = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    let cfg = twr_relay::tls::client_config(Some(pin), &[]);
    let bad = rustls::pki_types::ServerName::try_from("not-a-label.relay.test").unwrap();
    assert!(tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(bad, tcp)
        .await
        .is_err());
    // Not TLS at all.
    let mut tcp = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    tcp.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(tcp.read(&mut buf).await.unwrap_or(0), 0);

    let m = metrics(&r).await;
    assert_eq!(refusals(&m, "bad_tag"), 1, "{m}");
    assert_eq!(refusals(&m, "hub_offline"), 1);
    assert_eq!(refusals(&m, "bad_label"), 1);
    assert_eq!(refusals(&m, "not_tls"), 1);
    assert!(m.contains("twr_relay_phone_connections_total 1"));
    drop(s);
    r.stop().await;
}

#[tokio::test]
async fn revoking_a_phone_cuts_it_off() {
    let r = named("").await;
    let (s, key) = connect(&r).await;
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    s.set_phones(vec![cred]).await.unwrap();
    let route = s.welcome.phone_routes[0].clone();
    let pin = key.pin();
    let hub = echo_hub(s, key.clone());
    let mut t = client::phone_connect(&route, &pin, &relay_key, &[b"t"])
        .await
        .unwrap();
    t.write_all(b"x").await.unwrap();
    t.flush().await.unwrap();
    let mut b = [0u8; 1];
    t.read_exact(&mut b).await.unwrap();
    let (s, _) = hub.await.unwrap();
    s.set_phones(vec![]).await.unwrap();
    let n = tokio::time::timeout(Duration::from_secs(3), t.read(&mut b))
        .await
        .expect("closed within seconds");
    assert!(matches!(n, Ok(0) | Err(_)));
    // And it can't come back.
    assert!(client::phone_connect(&route, &pin, &relay_key, &[b"t"])
        .await
        .is_err());
    r.stop().await;
}

#[tokio::test]
async fn per_hub_connection_cap() {
    let r = named("[limits]\nphone_connections_per_hub = 1\n").await;
    let (s, key) = connect(&r).await;
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    s.set_phones(vec![cred]).await.unwrap();
    let route = s.welcome.phone_routes[0].clone();
    let pin = key.pin();
    let hub = echo_hub(s, key.clone());
    let mut first = client::phone_connect(&route, &pin, &relay_key, &[b"t"])
        .await
        .unwrap();
    first.write_all(b"x").await.unwrap();
    first.flush().await.unwrap();
    let mut b = [0u8; 1];
    first.read_exact(&mut b).await.unwrap();
    assert!(client::phone_connect(&route, &pin, &relay_key, &[b"t"])
        .await
        .is_err());
    let m = metrics(&r).await;
    assert_eq!(refusals(&m, "hub_busy"), 1);
    drop(hub);
    r.stop().await;
}

#[tokio::test]
async fn throughput_is_limited_per_connection() {
    // 1 MB/s with a 64 KiB burst: 512 KiB takes most of half a second.
    let r = named("[limits]\nbytes_per_second = 1000000\nburst_bytes = 65536\n").await;
    let (s, key) = connect(&r).await;
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    s.set_phones(vec![cred]).await.unwrap();
    let route = s.welcome.phone_routes[0].clone();
    let pin = key.pin();
    let hub = echo_hub(s, key.clone());
    let mut t = client::phone_connect(&route, &pin, &relay_key, &[b"t"])
        .await
        .unwrap();
    let data = vec![5u8; 512 * 1024];
    let start = std::time::Instant::now();
    let (mut rd, mut wr) = tokio::io::split(&mut t);
    let w = async {
        wr.write_all(&data).await.unwrap();
        wr.flush().await.unwrap();
    };
    let rr = async {
        let mut got = vec![0u8; data.len()];
        rd.read_exact(&mut got).await.unwrap();
        got
    };
    let (_, got) = tokio::join!(w, rr);
    assert_eq!(got, data);
    let took = start.elapsed();
    assert!(took >= Duration::from_millis(350), "took {took:?}");
    drop(hub);
    r.stop().await;
}

// §7: port mode and DNS-less SNI --------------------------------------------------------------

#[tokio::test]
async fn dnsless_relay_with_sni_and_port_mode() {
    // The hub's name must be in the config before start: make the key first.
    let key = Arc::new(IdentityKey::generate_p256().unwrap());
    let name = names::hub_name(&key.pin());
    let r = dnsless(&format!(
        "[[port_hub]]\nhub = \"{name}\"\nlisten = \"127.0.0.1:0\"\n"
    ))
    .await;
    assert!(r.url.pin.is_some());
    assert_eq!(r.url.server_name(), None);
    let s = HubSession::connect(&hub_cfg(&r), &key).await.unwrap();
    assert_eq!(s.welcome.phone_routes[0].suffix, "relay.invalid");
    let port = s.welcome.port_route.clone().expect("a port route");
    assert_eq!(port.port, r.r().port_addrs[&name].port());
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    s.set_phones(vec![cred]).await.unwrap();
    let route = s.welcome.phone_routes[0].clone();
    let pin = key.pin();

    // SNI with relay.invalid, connecting by IP.
    let hub = echo_hub(s, key.clone());
    echo_once(&route, &pin, &relay_key).await.unwrap();
    let (s, meta) = hub.await.unwrap();
    assert_eq!(meta.mode, "sni");

    // Port mode: no SNI at all.
    let hub = echo_hub(s, key.clone());
    let tcp = TcpStream::connect(("127.0.0.1", port.port)).await.unwrap();
    let ip = rustls::pki_types::ServerName::IpAddress(
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap().into(),
    );
    let cfg = twr_relay::tls::client_config(Some(pin), &[b"t"]);
    let mut t = tokio_rustls::TlsConnector::from(Arc::new(cfg))
        .connect(ip, tcp)
        .await
        .unwrap();
    t.write_all(b"ping").await.unwrap();
    t.flush().await.unwrap();
    let mut b = [0u8; 4];
    t.read_exact(&mut b).await.unwrap();
    assert_eq!(&b, b"ping");
    let (_s, meta) = hub.await.unwrap();
    assert_eq!(meta.mode, "port");
    assert_eq!(meta.phone, None);
    r.stop().await;
}

// §8: the HTTPS API --------------------------------------------------------------------------

#[tokio::test]
async fn info_status_and_moved() {
    let r = named("").await;
    let info = client::info(&r.t()).await.unwrap();
    assert_eq!(info["relay"], "twr-relay");
    assert_eq!(info["hub_protocols"][0], "twr-relay/1");
    assert_eq!(info["phone_suffixes"][0], "relay.test");

    let (s, key) = connect(&r).await;
    let name = s.welcome.hub.clone();
    let (cred, relay_key) = client::new_phone_credential().unwrap();
    s.set_phones(vec![cred]).await.unwrap();
    assert_eq!(
        client::hub_status(&r.t(), &name, &relay_key).await.unwrap(),
        Some(true)
    );
    assert_eq!(
        client::hub_status(&r.t(), &name, &[1u8; 32]).await.unwrap(),
        None
    );

    let routes = serde_json::json!([{ "kind": "relay", "url": "relay://elsewhere.example" }]);
    let (body, sig) = client::sign_moved(&key, twr_relay::now_unix(), routes.clone()).unwrap();
    s.moved(body, sig).await.unwrap();
    // A note signed for another time is refused.
    let (body, sig) = client::sign_moved(&key, 1, routes.clone()).unwrap();
    assert!(s.moved(body, sig).await.is_err());

    s.close();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        client::hub_status(&r.t(), &name, &relay_key).await.unwrap(),
        Some(false)
    );

    let (st, b) = client::https(&r.t(), "GET", &format!("/v1/hubs/{name}/moved"), None)
        .await
        .unwrap();
    assert_eq!(st, 200);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let got = client::verify_moved(
        &key.pin(),
        v["spki"].as_str().unwrap(),
        v["body"].as_str().unwrap(),
        v["sig"].as_str().unwrap(),
    )
    .expect("verifies");
    assert_eq!(got["routes"], routes);
    assert!(client::verify_moved(
        &[0; 32],
        v["spki"].as_str().unwrap(),
        v["body"].as_str().unwrap(),
        v["sig"].as_str().unwrap()
    )
    .is_none());
    let (st, _) = client::https(&r.t(), "GET", "/nothing/here", None)
        .await
        .unwrap();
    assert_eq!(st, 404);
    r.stop().await;
}

// §9: the record store ------------------------------------------------------------------------

fn pkarr_body(key: &IdentityKey, ts: u64, packet: &[u8]) -> Vec<u8> {
    let sig = key
        .sign(&twr_relay::server::stores::signable(ts, packet))
        .unwrap();
    [sig.as_slice(), &ts.to_be_bytes(), packet].concat()
}

#[tokio::test]
async fn record_store_semantics() {
    let r = named("").await;
    let key = IdentityKey::generate_ed25519().unwrap();
    let z = twr_relay::b32::zencode(key.public_raw());
    let path = format!("/pkarr/{z}");
    let (st, _) = client::https(&r.t(), "GET", &path, None).await.unwrap();
    assert_eq!(st, 404);
    let a = pkarr_body(&key, 2_000, &[0u8; 12]);
    assert_eq!(
        client::https(&r.t(), "PUT", &path, Some(a.clone()))
            .await
            .unwrap()
            .0,
        200
    );
    let (st, got) = client::https(&r.t(), "GET", &path, None).await.unwrap();
    assert_eq!((st, got), (200, a.clone()));
    assert_eq!(
        client::https(
            &r.t(),
            "PUT",
            &path,
            Some(pkarr_body(&key, 1_000, &[0u8; 12]))
        )
        .await
        .unwrap()
        .0,
        409
    );
    assert_eq!(
        client::https(
            &r.t(),
            "PUT",
            &path,
            Some(pkarr_body(&key, 2_000, &[1u8; 12]))
        )
        .await
        .unwrap()
        .0,
        428
    );
    let mut bad = pkarr_body(&key, 3_000, &[0u8; 12]);
    bad[5] ^= 1;
    assert_eq!(
        client::https(&r.t(), "PUT", &path, Some(bad))
            .await
            .unwrap()
            .0,
        400
    );
    assert_eq!(
        client::https(&r.t(), "PUT", "/pkarr/short", Some(a))
            .await
            .unwrap()
            .0,
        400
    );
    r.stop().await;
}

// §10: iroh admission -----------------------------------------------------------------------

#[tokio::test]
async fn admission_and_probe_dnsless() {
    let r = dnsless("").await;
    // A pair admission posted by either side admits both.
    let a = IdentityKey::ed25519_from_seed(&[1; 32]).unwrap();
    let b = IdentityKey::ed25519_from_seed(&[2; 32]).unwrap();
    let (ea, eb): ([u8; 32], [u8; 32]) = (
        a.public_raw().try_into().unwrap(),
        b.public_raw().try_into().unwrap(),
    );
    let exp = twr_relay::now_unix() + 3600;
    let adm = admit::pair_admission(
        &ea,
        &eb,
        exp,
        admit::sign_pair_half(&a, &ea, &eb, exp).unwrap(),
        admit::sign_pair_half(&b, &ea, &eb, exp).unwrap(),
    );
    let ans = client::post_admission(&r.t(), &adm).await.unwrap();
    assert_eq!(ans.until, exp);
    assert_eq!(ans.admitted.len(), 2);
    let stale = admit::sign_pairing(&a, 1).unwrap();
    assert!(client::post_admission(&r.t(), &stale)
        .await
        .unwrap_err()
        .to_string()
        .contains("stale"));

    // The full probe, iroh included, passes against this relay.
    let report = probe::probe(&probe::ProbeOptions {
        target: r.t(),
        client: "twr probe test".into(),
        timeout: Duration::from_secs(10),
        state: None,
    })
    .await;
    assert!(report.ok, "{report:#?}");
    let names: Vec<_> = report.checks.iter().map(|c| c.name).collect();
    assert!(
        names.contains(&"iroh_relay") && names.contains(&"phone_round_trip"),
        "{names:?}"
    );
    r.stop().await;
}

#[tokio::test]
async fn iroh_refuses_endpoints_not_admitted() {
    let r = dnsless("").await;
    let info = client::info(&r.t()).await.unwrap();
    let url: iroh_base::RelayUrl = info["iroh_url"].as_str().unwrap().parse().unwrap();
    let secret = iroh_base::SecretKey::from_bytes(&[9; 32]);
    let res =
        iroh_relay::client::ClientBuilder::new(url, secret, iroh_dns::dns::DnsResolver::new())
            .tls_client_config(twr_relay::tls::client_config(r.url.pin, &[]))
            .connect()
            .await;
    assert!(
        res.is_err(),
        "an endpoint that isn't admitted must be refused"
    );
    let m = metrics(&r).await;
    assert!(refusals(&m, "iroh_denied") >= 1, "{m}");
    r.stop().await;
}

#[tokio::test]
async fn admissions_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let text = "[relay]\npublic_host = \"127.0.0.1\"\nlisten = [\"127.0.0.1:0\"]\nmetrics_listen = \"127.0.0.1:0\"\n[tls]\nmode = \"self-signed\"\n[iroh]\nenabled = false\n";
    let mut cfg: Config = toml::from_str(text).unwrap();
    cfg.relay.state_dir = dir.path().to_path_buf();
    let run = server::start(cfg.clone(), "t").await.unwrap();
    let url = run.relay_url();
    let k = IdentityKey::ed25519_from_seed(&[4; 32]).unwrap();
    client::post_admission(
        &client::Target::new(url.clone()),
        &admit::sign_pairing(&k, twr_relay::now_unix()).unwrap(),
    )
    .await
    .unwrap();
    run.shutdown().await;
    let state = twr_relay::server::stores::State::load(dir.path()).unwrap();
    assert!(state.is_admitted(&k.public_raw().try_into().unwrap(), twr_relay::now_unix()));
    // The same self-signed key, so the same pin, after a restart.
    let run2 = server::start(cfg, "t").await.unwrap();
    assert_eq!(run2.self_pin, url.pin);
    run2.shutdown().await;
}

// §4: front-end limits ------------------------------------------------------------------------

#[tokio::test]
async fn slow_and_pending_connections() {
    let r = named("[limits]\nhello_seconds = 1\npending_per_ip = 2\n").await;
    let a = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    let b = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut c = TcpStream::connect(r.r().addrs[0]).await.unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), c.read(&mut buf))
            .await
            .unwrap()
            .unwrap_or(0),
        0
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let m = metrics(&r).await;
    assert_eq!(refusals(&m, "pending"), 1, "{m}");
    assert_eq!(refusals(&m, "slow_hello"), 2);
    drop((a, b));
    r.stop().await;
}

#[tokio::test]
async fn a_probe_every_minute_stays_within_the_limits() {
    // Tight limits: one new hub name and one pair admission per address per hour.
    let r = dnsless("[limits]\nnew_hubs_per_ip_per_hour = 1\npair_admissions_per_ip_per_hour = 3\npairing_admissions_per_ip_per_hour = 1\n").await;
    let dir = tempfile::tempdir().unwrap();
    let opts = probe::ProbeOptions {
        target: r.t(),
        client: "t".into(),
        timeout: Duration::from_secs(10),
        state: Some(dir.path().to_path_buf()),
    };
    for _ in 0..3 {
        let report = probe::probe(&opts).await;
        assert!(report.ok, "{report:#?}");
    }
    // A fourth pair admission from this address is over the limit, unless exempt.
    let report = probe::probe(&opts).await;
    assert!(!report.ok);
    r.stop().await;
    let r = dnsless(
        "[limits]\npair_admissions_per_ip_per_hour = 1\nexempt_addresses = [\"127.0.0.0/8\"]\n",
    )
    .await;
    let opts = probe::ProbeOptions {
        target: r.t(),
        ..opts
    };
    for _ in 0..3 {
        assert!(probe::probe(&opts).await.ok);
    }
    r.stop().await;
}

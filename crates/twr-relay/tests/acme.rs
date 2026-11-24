//! Let's Encrypt's path (tls-alpn-01 on the relay's own port) against a local pebble, the
//! ACME test server. Needs pebble running; `docs/tools/relay-acme-pebble.sh` starts one in
//! Docker on loopback and runs this. It never contacts a real ACME service.
//!
//! Variables: `TWR_PEBBLE_DIRECTORY` (pebble's directory URL), `TWR_PEBBLE_ROOT` (the PEM that
//! signs pebble's own HTTPS certificate), `TWR_PEBBLE_ISSUER` (a PEM of the roots pebble
//! issues under), and `TWR_PEBBLE_PORT` (the port pebble validates on, where the relay listens).

use std::sync::Arc;
use std::time::Duration;
use twr_relay::server::{self, config::Config};

#[tokio::test]
#[ignore = "needs pebble: docs/tools/relay-acme-pebble.sh"]
async fn acme_certificate_from_pebble() {
    let env = |k: &str| {
        std::env::var(k)
            .unwrap_or_else(|_| panic!("{k} is not set; run docs/tools/relay-acme-pebble.sh"))
    };
    let directory = env("TWR_PEBBLE_DIRECTORY");
    let root = env("TWR_PEBBLE_ROOT");
    let issuer = env("TWR_PEBBLE_ISSUER");
    let port: u16 = env("TWR_PEBBLE_PORT").parse().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let text = format!(
        "[relay]\nhostname = \"relay.test\"\npublic_host = \"127.0.0.1\"\nlisten = [\"127.0.0.1:{port}\"]\nmetrics_listen = \"127.0.0.1:0\"\n\
         [tls]\nmode = \"acme\"\nacme_contact = \"mailto:ops@relay.test\"\nacme_directory = \"{directory}\"\nacme_ca_roots = \"{root}\"\n\
         [iroh]\nquic_listen = \"127.0.0.1:0\"\n"
    );
    let mut cfg: Config = toml::from_str(&text).unwrap();
    cfg.relay.state_dir = dir.path().to_path_buf();
    let running = server::start(cfg, "0.0.0-acme").await.unwrap();
    let counters = running.counters();

    // Wait for the certificate (the expiry gauge turns positive).
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while counters
        .cert_not_after
        .load(std::sync::atomic::Ordering::Relaxed)
        == 0
    {
        assert!(
            std::time::Instant::now() < deadline,
            "no certificate within 60 s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        counters
            .acme_challenges
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1
    );

    // A client that trusts only pebble's issuing roots accepts both names.
    let roots: Vec<_> = rustls::pki_types::CertificateDer::pem_file_iter(&issuer)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for name in ["relay.test", "iroh.relay.test"] {
        let cfg = twr_relay::tls::client_config_with_roots(roots.clone(), &[b"http/1.1"]);
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        let sn = rustls::pki_types::ServerName::try_from(name).unwrap();
        tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(sn, tcp)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // And the client library, by WebPKI-style checking against those roots, not a pin.
    let url = twr_relay::RelayUrl::parse(&format!("relay://relay.test:{port}")).unwrap();
    let mut target = twr_relay::client::Target::new(url).connect_to(([127, 0, 0, 1], port).into());
    target.extra_roots = roots;
    let info = twr_relay::client::info(&target).await.unwrap();
    assert_eq!(info["relay_pin"], serde_json::Value::Null);

    // The account and certificate are cached in the state directory.
    assert!(std::fs::read_dir(dir.path().join("acme")).unwrap().count() >= 2);
    running.shutdown().await;
}

use rustls::pki_types::pem::PemObject as _;

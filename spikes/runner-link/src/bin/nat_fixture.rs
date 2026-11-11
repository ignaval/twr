//! Only the disposable Docker network-namespace harness runs this binary.
//! It never enables public lookup or uses a machine's real pairing state.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{ensure, Result};
use iroh::{endpoint::presets, Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl};
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use twr_runner_link_spike::{
    transfer,
    transport::{self, ALPN},
};

#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        std::env::var("TWR_LINK_NETNS_FIXTURE").as_deref() == Ok("1"),
        "run through netns_probe.py in its disposable container"
    );
    let status = std::fs::read_to_string("/proc/self/status")?;
    for field in ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"] {
        ensure!(
            status.lines().any(|line| line.starts_with(field)
                && line.split_whitespace().nth(1) == Some("0000000000000000")),
            "fixture workers require no capabilities"
        );
    }
    ensure!(
        status
            .lines()
            .any(|line| line.starts_with("NoNewPrivs:")
                && line.split_whitespace().nth(1) == Some("1")),
        "fixture requires no-new-privileges"
    );
    ensure!(
        status.lines().any(|line| line.starts_with("Uid:")
            && line.split_whitespace().skip(1).all(|uid| uid == "65534")),
        "fixture must run as nobody"
    );
    tokio::time::timeout(Duration::from_secs(120), run()).await?
}

async fn wait_file(path: &str) -> Result<String> {
    for _ in 0..1000 {
        if Path::new(path).exists() {
            return Ok(std::fs::read_to_string(path)?);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("fixture peer did not start")
}

async fn endpoint(relay: RelayUrl) -> Result<Endpoint> {
    Ok(Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .bind_addr(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))?
        .relay_mode(RelayMode::Custom([relay].into_iter().collect()))
        .bind()
        .await?)
}

async fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 2, "expected relay, server or client");
    let relay_url: RelayUrl = "http://10.66.0.1:3340".parse()?;
    match args[1].as_str() {
        "relay" => {
            let mut config = ServerConfig::default();
            config.relay = Some(RelayConfig::new(
                "10.66.0.1:3340".parse::<std::net::SocketAddr>()?,
            ));
            let server = Server::spawn(config).await?;
            std::fs::write("/tmp/relay-ready", b"ready")?;
            tokio::time::sleep(Duration::from_secs(110)).await;
            server.shutdown().await?;
        }
        "server" => {
            wait_file("/tmp/relay-ready").await?;
            let endpoint = endpoint(relay_url).await?;
            endpoint.online().await;
            std::fs::write("/tmp/runner-id", endpoint.id().to_string())?;
            let peer: EndpointId = wait_file("/tmp/hub-id").await?.parse()?;
            let connection = transport::accept_pinned(&endpoint, peer).await?;
            transfer::receive_probe(&connection).await?;
            endpoint.close().await;
        }
        "client" => {
            wait_file("/tmp/relay-ready").await?;
            let endpoint = endpoint(relay_url.clone()).await?;
            endpoint.online().await;
            std::fs::write("/tmp/hub-id", endpoint.id().to_string())?;
            let peer: EndpointId = wait_file("/tmp/runner-id").await?.parse()?;
            let started = Instant::now();
            let connection = endpoint
                .connect(EndpointAddr::new(peer).with_relay_url(relay_url), ALPN)
                .await?;
            let direct = transfer::send_probe(&connection).await?;
            println!(
                "{}",
                serde_json::json!({"two_nat_transfer":true,"bytes":transfer::PROBE_BYTES,"elapsed_ms":started.elapsed().as_millis(),"direct":direct,"public_infrastructure":false})
            );
            endpoint.close().await;
        }
        _ => anyhow::bail!("unknown fixture role"),
    }
    Ok(())
}

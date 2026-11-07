//! Run only a loopback transfer. No public relay/DNS, accounts or live agents.

use std::time::{Duration, Instant};

use anyhow::Result;
use twr_runner_link_spike::transport::{self, ALPN};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), probe()).await?
}

async fn probe() -> Result<()> {
    let started = Instant::now();
    let hub = transport::endpoint(None).await?;
    let runner = transport::endpoint(None).await?;
    let expected = hub.id();
    let address = runner.addr();
    let server = tokio::spawn(async move {
        let connection = transport::accept_pinned(&runner, expected).await?;
        let (mut send, mut recv) = connection.accept_bi().await?;
        let hello = transport::read_hello(&mut recv).await?;
        transport::write_hello(&mut send, &hello).await?;
        send.finish()?;
        let mut ack = [0];
        recv.read_exact(&mut ack).await?;
        runner.close().await;
        Ok::<_, anyhow::Error>(())
    });
    let connection = hub.connect(address, ALPN).await?;
    let (mut send, mut recv) = connection.open_bi().await?;
    transport::write_hello(&mut send, b"twr runner transport probe").await?;
    let response = transport::read_hello(&mut recv).await?;
    anyhow::ensure!(
        response == b"twr runner transport probe",
        "unexpected response"
    );
    let direct = connection
        .paths()
        .iter()
        .any(|p| p.is_selected() && !p.remote_addr().is_relay());
    send.write_all(&[1]).await?;
    send.finish()?;
    server.await??;
    hub.close().await;
    println!(
        "{}",
        serde_json::json!({"loopback_echo":true,"direct":direct,"elapsed_ms":started.elapsed().as_millis(),"public_infrastructure":false})
    );
    Ok(())
}

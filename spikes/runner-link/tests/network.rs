use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use anyhow::Result;
use iroh::{EndpointAddr, SecretKey};
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use tokio::io::AsyncWriteExt;
use twr_runner_link_spike::transport::{self, ALPN};

#[tokio::test]
async fn bulk_stream_exceeds_json_limit_without_replacing_control_session() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let hub = transport::endpoint(None).await?;
        let runner = transport::endpoint(None).await?;
        let address = runner.addr();
        let expected = hub.id();
        let server = tokio::spawn(async move {
            let connection = transport::accept_pinned(&runner, expected).await?;
            twr_runner_link_spike::transfer::receive_probe(&connection).await?;
            runner.close().await;
            Ok::<_, anyhow::Error>(())
        });
        let connection = hub.connect(address, ALPN).await?;
        twr_runner_link_spike::transfer::send_probe(&connection).await?;
        server.await??;
        hub.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn direct_session_keeps_working_after_local_relay_shutdown() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        let relay = Server::spawn(config).await?;
        let url: iroh::RelayUrl = format!("http://{}", relay.http_addr().unwrap()).parse()?;
        let hub = transport::endpoint(Some(url.clone())).await?;
        let runner = transport::endpoint(Some(url)).await?;
        hub.online().await;
        runner.online().await;
        // Explicit reachable addresses test direct-path continuity; this is not a
        // claim that loopback models Internet NAT hole punching.
        let address = runner.addr();
        let expected = hub.id();
        let server = tokio::spawn(async move {
            let connection = transport::accept_pinned(&runner, expected).await?;
            twr_runner_link_spike::transfer::receive_probe(&connection).await?;
            runner.close().await;
            Ok::<_, anyhow::Error>(())
        });
        let connection = hub.connect(address, ALPN).await?;
        for _ in 0..100 {
            if connection
                .paths()
                .iter()
                .any(|p| p.is_selected() && !p.remote_addr().is_relay())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(connection
            .paths()
            .iter()
            .any(|p| p.is_selected() && !p.remote_addr().is_relay()));
        relay.shutdown().await?;
        twr_runner_link_spike::transfer::send_probe(&connection).await?;
        server.await??;
        hub.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn direct_connection_authenticates_keys_and_bounds_hello() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let hub = transport::endpoint(None).await?;
        let runner = transport::endpoint(None).await?;
        let expected = hub.id();
        let address = runner.addr();
        assert!(!address.ip_addrs().collect::<Vec<_>>().is_empty());
        assert_eq!(address.relay_urls().count(), 0);
        let server = tokio::spawn(async move {
            let connection = transport::accept_pinned(&runner, expected).await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            let bytes = transport::read_hello(&mut recv).await?;
            assert_eq!(bytes, b"authenticated probe");
            transport::write_hello(&mut send, &bytes).await?;
            send.finish()?;
            let mut ack = [0];
            recv.read_exact(&mut ack).await?;
            runner.close().await;
            Ok::<_, anyhow::Error>(())
        });
        let connection = hub.connect(address, ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        transport::write_hello(&mut send, b"authenticated probe").await?;
        assert_eq!(
            transport::read_hello(&mut recv).await?,
            b"authenticated probe"
        );
        send.write_all(&[1]).await?;
        send.finish()?;
        server.await??;
        hub.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn unpaired_endpoint_is_rejected_before_reading_worker_data() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let intruder = transport::endpoint(None).await?;
        let runner = transport::endpoint(None).await?;
        let address = runner.addr();
        let expected = SecretKey::generate().public();
        let server_endpoint = runner.clone();
        let server = tokio::spawn(async move {
            let rejection = transport::accept_pinned(&server_endpoint, expected)
                .await
                .expect_err("unpaired endpoint was accepted");
            assert_eq!(rejection.to_string(), "unpaired endpoint");
        });
        let connection = intruder.connect(address, ALPN).await?;
        if let Ok((mut send, _)) = connection.open_bi().await {
            let _ = send.write_all(b"unauthorized worker bytes").await;
        }
        let iroh::endpoint::ConnectionError::ApplicationClosed(close) = connection.closed().await
        else {
            anyhow::bail!("expected explicit application rejection, not a timeout")
        };
        assert_eq!(close.error_code.into_inner(), 1);
        assert_eq!(close.reason.as_ref(), b"unpaired endpoint");
        server.await?;
        runner.close().await;
        intruder.close().await;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

#[tokio::test]
async fn oversized_and_truncated_hello_fail_before_authorization() -> Result<()> {
    let (mut send, mut recv) = tokio::io::duplex(32);
    send.write_u32(u32::MAX).await?;
    assert!(transport::read_hello(&mut recv).await.is_err());
    let (mut send, mut recv) = tokio::io::duplex(32);
    send.write_u32(4).await?;
    send.write_all(&[1]).await?;
    drop(send);
    assert!(transport::read_hello(&mut recv).await.is_err());
    Ok(())
}

#[tokio::test]
async fn relay_only_address_connects_without_public_discovery() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut config = ServerConfig::default();
        config.relay = Some(RelayConfig::new(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)));
        let relay = Server::spawn(config).await?;
        let url: iroh::RelayUrl = format!("http://{}", relay.http_addr().unwrap()).parse()?;
        let hub = transport::endpoint(Some(url.clone())).await?;
        let runner = transport::endpoint(Some(url.clone())).await?;
        hub.online().await;
        runner.online().await;
        let address = EndpointAddr::new(runner.id()).with_relay_url(url);
        let peer = hub.id();
        let server = tokio::spawn(async move {
            let connection = transport::accept_pinned(&runner, peer).await?;
            let (mut send, mut recv) = connection.accept_bi().await?;
            assert_eq!(transport::read_hello(&mut recv).await?, b"relay probe");
            transport::write_hello(&mut send, b"ok").await?;
            send.finish()?;
            let mut ack = [0];
            recv.read_exact(&mut ack).await?;
            runner.close().await;
            Ok::<_, anyhow::Error>(())
        });
        let connection = hub.connect(address, ALPN).await?;
        let (mut send, mut recv) = connection.open_bi().await?;
        transport::write_hello(&mut send, b"relay probe").await?;
        assert_eq!(transport::read_hello(&mut recv).await?, b"ok");
        send.write_all(&[1]).await?;
        send.finish()?;
        server.await??;
        hub.close().await;
        relay.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(())
}

//! Explicit addressing only. Minimal preset deliberately enables no public discovery.

use std::{
    net::{Ipv4Addr, SocketAddrV4},
    time::Duration,
};

use anyhow::{ensure, Result};
use iroh::{
    endpoint::{presets, Connection},
    Endpoint, EndpointId, RelayMode, RelayUrl,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const ALPN: &[u8] = b"twr/spike/1";
pub const MAX_HELLO: usize = 8192;

pub async fn endpoint(relay: Option<RelayUrl>) -> Result<Endpoint> {
    let mode = relay.map_or(RelayMode::Disabled, |url| {
        RelayMode::Custom([url].into_iter().collect())
    });
    Ok(Endpoint::builder(presets::Minimal)
        .alpns(vec![ALPN.to_vec()])
        .bind_addr(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?
        .relay_mode(mode)
        .bind()
        .await?)
}

pub async fn accept_pinned(endpoint: &Endpoint, expected: EndpointId) -> Result<Connection> {
    let connection = tokio::time::timeout(Duration::from_secs(5), async {
        let incoming = endpoint
            .accept()
            .await
            .ok_or_else(|| anyhow::anyhow!("endpoint closed"))?;
        Ok::<_, anyhow::Error>(incoming.await?)
    })
    .await??;
    if connection.remote_id() != expected {
        connection.close(1u32.into(), b"unpaired endpoint");
        anyhow::bail!("unpaired endpoint");
    }
    Ok(connection)
}

pub async fn read_hello(reader: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let len = reader.read_u32().await? as usize;
        ensure!(len <= MAX_HELLO, "hello too large");
        let mut bytes = vec![0; len];
        reader.read_exact(&mut bytes).await?;
        Ok(bytes)
    })
    .await?
}

pub async fn write_hello(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= MAX_HELLO, "hello too large");
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    Ok(())
}

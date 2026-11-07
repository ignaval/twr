//! A bulk-stream experiment, not the production sealed Git transfer protocol.

use anyhow::{ensure, Result};
use iroh::endpoint::Connection;
use ring::digest::{Context, SHA256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::transport::{read_hello, write_hello};

pub const PROBE_BYTES: usize = (16 << 20) + 123;
pub const MAX_BYTES: usize = 32 << 20;
pub const CHUNK: usize = 64 << 10;

pub async fn send_probe(connection: &Connection) -> Result<bool> {
    let (mut control_send, mut control_recv) = connection.open_bi().await?;
    write_hello(&mut control_send, b"control").await?;
    ensure!(
        read_hello(&mut control_recv).await? == b"ready",
        "control not ready"
    );
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_u32(PROBE_BYTES as u32).await?;
    let block = [42u8; CHUNK];
    let mut hash = Context::new(&SHA256);
    let mut remaining = PROBE_BYTES;
    while remaining > 0 {
        let size = remaining.min(CHUNK);
        send.write_all(&block[..size]).await?;
        hash.update(&block[..size]);
        remaining -= size;
    }
    send.finish()?;
    let mut digest = [0; 32];
    recv.read_exact(&mut digest).await?;
    ensure!(digest == hash.finish().as_ref(), "bulk digest mismatch");
    write_hello(&mut control_send, b"after-bulk").await?;
    ensure!(
        read_hello(&mut control_recv).await? == b"after-bulk",
        "control session was replaced"
    );
    // Observe the selected path before the final ACK allows the receiver to close.
    let direct = connection
        .paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| !p.remote_addr().is_relay())
        .ok_or_else(|| anyhow::anyhow!("no selected transport path"))?;
    control_send.write_all(&[1]).await?;
    control_send.finish()?;
    Ok(direct)
}

pub async fn receive_probe(connection: &Connection) -> Result<()> {
    let (mut control_send, mut control_recv) = connection.accept_bi().await?;
    ensure!(
        read_hello(&mut control_recv).await? == b"control",
        "unexpected control message"
    );
    write_hello(&mut control_send, b"ready").await?;
    let (mut send, mut recv) = connection.accept_bi().await?;
    let size = recv.read_u32().await? as usize;
    ensure!(size <= MAX_BYTES, "transfer too large");
    let mut hash = Context::new(&SHA256);
    let mut block = [0; CHUNK];
    let mut remaining = size;
    while remaining > 0 {
        let amount = remaining.min(CHUNK);
        recv.read_exact(&mut block[..amount]).await?;
        hash.update(&block[..amount]);
        remaining -= amount;
    }
    // Require EOF after the declared payload, instead of accepting trailing bytes.
    let mut extra = [0];
    ensure!(
        recv.read(&mut extra).await?.is_none(),
        "trailing transfer bytes"
    );
    send.write_all(hash.finish().as_ref()).await?;
    send.finish()?;
    ensure!(
        read_hello(&mut control_recv).await? == b"after-bulk",
        "control session lost"
    );
    write_hello(&mut control_send, b"after-bulk").await?;
    control_send.finish()?;
    let mut ack = [0];
    control_recv.read_exact(&mut ack).await?;
    Ok(())
}

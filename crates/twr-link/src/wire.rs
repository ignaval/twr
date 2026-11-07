//! Bounded control messages, with independent streaming for bulk payloads.
use anyhow::{ensure, Result};
use serde::{de::DeserializeOwned, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const MAX_CONTROL: usize = 32 * 1024;
pub const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
pub async fn read<T: DeserializeOwned>(r: &mut (impl AsyncRead + Unpin)) -> Result<T> {
    read_with_timeout(r, CONTROL_TIMEOUT).await
}
pub async fn read_with_timeout<T: DeserializeOwned>(
    r: &mut (impl AsyncRead + Unpin),
    timeout: std::time::Duration,
) -> Result<T> {
    tokio::time::timeout(timeout, async {
        let len = r.read_u32().await? as usize;
        ensure!(len <= MAX_CONTROL, "link control message exceeds limit");
        let mut bytes = vec![0; len];
        r.read_exact(&mut bytes).await?;
        Ok(serde_json::from_slice(&bytes)?)
    })
    .await?
}
pub async fn write<T: Serialize>(w: &mut (impl AsyncWrite + Unpin), message: &T) -> Result<()> {
    let bytes = serde_json::to_vec(message)?;
    ensure!(
        bytes.len() <= MAX_CONTROL,
        "link control message exceeds limit"
    );
    tokio::time::timeout(CONTROL_TIMEOUT, async {
        w.write_u32(bytes.len() as u32).await?;
        w.write_all(&bytes).await?;
        w.flush().await?;
        Ok(())
    })
    .await?
}

//! Per-workspace workers are separate daemons. Service reconnects preserve their runtimes.
use crate::state::{Machine, Worker};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
pub const MAX_EXECUTABLE: u64 = 1024 * 1024 * 1024;
pub const REQUIRED_FEATURES: &[&str] = &["managed_paths_v1", "global_leases_v1", "revocation_v1"];
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub sha256: String,
    pub size: u64,
    pub os: String,
    pub arch: String,
    pub protocol: u32,
}
impl Build {
    pub fn of(path: &Path) -> Result<Self> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        ensure!(
            file.metadata()?.is_file(),
            "worker executable must be a regular file"
        );
        let size = file.metadata()?.len();
        ensure!(size <= MAX_EXECUTABLE, "worker executable exceeds limit");
        let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
        let mut received = 0u64;
        let mut buf = [0; 65536];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            received += n as u64;
            ensure!(received <= size, "worker file changed while hashing");
            digest.update(&buf[..n]);
        }
        ensure!(received == size, "worker file changed while hashing");
        Ok(Self {
            sha256: hex::encode(digest.finish()),
            size,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            protocol: twr_core::runner::PROTOCOL_VERSION,
        })
    }
    pub async fn executable(path: &Path) -> Result<Self> {
        let mut build = Self::of(path)?;
        let mut cmd = tokio::process::Command::new(path);
        cmd.arg("--link-service-version")
            .env(twr_runner::reap::AGENT_ENV, "linked-worker-probe");
        let result = twr_adapters::process::run_bounded(
            cmd,
            std::time::Duration::from_secs(10),
            4096,
            |_| false,
        )
        .await;
        let twr_adapters::process::Bounded::Exited(Some(0), bytes) = result else {
            anyhow::bail!("this executable does not support managed linked workers")
        };
        let info: serde_json::Value = serde_json::from_slice(&bytes)?;
        build.protocol = info["worker_protocol"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .context("worker protocol missing")?;
        for feature in REQUIRED_FEATURES {
            ensure!(
                info["worker_features"]
                    .as_array()
                    .is_some_and(|features| features.iter().any(|f| f == feature)),
                "worker lacks a mandatory managed-runtime feature"
            );
        }
        Ok(build)
    }
    pub fn matches(&self, path: &Path) -> bool {
        Self::of(path).is_ok_and(|actual| actual.sha256 == self.sha256 && actual.size == self.size)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.sha256.len() == 64
                && self
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid worker digest"
        );
        ensure!(
            self.size > 0 && self.size <= MAX_EXECUTABLE,
            "worker size exceeds limit"
        );
        ensure!(
            self.os == std::env::consts::OS && self.arch == std::env::consts::ARCH,
            "worker target differs; a native source build is required"
        );
        ensure!(
            self.protocol >= 7,
            "worker predates managed linked-worker support"
        );
        Ok(())
    }
    pub fn path(&self, machine: &Machine, scope: &str) -> PathBuf {
        machine
            .dir
            .join("runner/hubs")
            .join(scope)
            .join("builds")
            .join(&self.sha256)
            .join("twr")
    }
}
pub fn options(machine: &Machine, worker: &Worker) -> Result<twr_runner::relay::RelayOptions> {
    let key = twr_core::sealed::parse_hub_public(&worker.workspace_key)?;
    let root = machine
        .dir
        .join("runner/hubs")
        .join(&worker.scope)
        .join("boxes")
        .join(&worker.box_name);
    twr_core::paths::ensure_private_dir(&root)?;
    let digest = hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        format!("{}\0{}\0{}", worker.scope, worker.box_name, worker.epoch).as_bytes(),
    ));
    let sockets = machine
        .socket
        .parent()
        .context("socket parent missing")?
        .join("lr");
    twr_core::paths::ensure_private_dir(&sockets)?;
    let mut opts = twr_runner::relay::RelayOptions::new(&worker.box_name, key, false)?;
    opts.paths = twr_runner::relay::DaemonPaths {
        socket: sockets.join(format!("{}.sock", &digest[..20])),
        lock: sockets.join(format!("{}.lock", &digest[..20])),
        data_dir: root.clone(),
        log: root.join("daemon.log"),
    };
    opts.program = worker.build.path(machine, &worker.scope);
    opts.managed = Some(twr_runner::managed::Managed {
        root,
        resources: machine.dir.join("runner/leases"),
        revocation: machine.fence(&worker.machine, worker.epoch),
    });
    Ok(opts)
}
pub async fn relay<IO: AsyncRead + AsyncWrite + Unpin>(
    machine: &Machine,
    worker: &Worker,
    replace: bool,
    io: IO,
) -> Result<()> {
    let mut options = options(machine, worker)?;
    options.replace = replace;
    twr_runner::relay::relay_with(options, io, &mut |_| {}).await
}
pub async fn stop(machine: &Machine, worker: &Worker) -> Result<()> {
    twr_runner::relay::stop(&options(machine, worker)?, &mut |_| {}).await
}
pub async fn receive(
    r: &mut (impl AsyncRead + Unpin),
    path: &Path,
    build: &Build,
) -> Result<tempfile::NamedTempFile> {
    let parent = path.parent().context("worker path missing parent")?;
    twr_core::paths::ensure_private_dir(parent)?;
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    let mut file = tokio::fs::File::from_std(temp.reopen()?);
    let mut digest = ring::digest::Context::new(&ring::digest::SHA256);
    let mut remaining = build.size;
    let mut buf = vec![0; 65536];
    while remaining > 0 {
        let take = remaining.min(buf.len() as u64) as usize;
        let n = tokio::time::timeout(std::time::Duration::from_secs(30), r.read(&mut buf[..take]))
            .await??;
        ensure!(n != 0, "truncated worker transfer");
        remaining -= n as u64;
        digest.update(&buf[..n]);
        file.write_all(&buf[..n]).await?;
    }
    ensure!(
        hex::encode(digest.finish()) == build.sha256,
        "worker transfer digest mismatch"
    );
    file.sync_all().await?;
    Ok(temp)
}
pub fn install(temp: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o700))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}
pub async fn send(w: &mut (impl AsyncWrite + Unpin), path: &Path, build: &Build) -> Result<()> {
    let file = tokio::fs::File::open(path).await?;
    let n = tokio::io::copy(&mut file.take(build.size), w).await?;
    ensure!(n == build.size, "worker executable changed during transfer");
    w.flush().await?;
    Ok(())
}
/// Keep the authenticated QUIC connection alive while its stream is in use.
pub struct Stream {
    io: tokio::io::Join<iroh::endpoint::RecvStream, iroh::endpoint::SendStream>,
    _connection: Arc<iroh::endpoint::Connection>,
}
impl Stream {
    pub fn new(
        connection: iroh::endpoint::Connection,
        send: iroh::endpoint::SendStream,
        recv: iroh::endpoint::RecvStream,
    ) -> Self {
        Self {
            io: tokio::io::join(recv, send),
            _connection: Arc::new(connection),
        }
    }
}
impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_read(cx, buf)
    }
}
impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.io).poll_write(cx, buf)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self._connection.close(0u32.into(), b"worker stream closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grants_share_data_but_never_share_stop_sockets() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let machine = Machine {
            dir: dir.path().join("machine"),
            socket: dir.path().join("run/sock"),
        };
        machine.init()?;
        let mut worker = Worker {
            epoch: 1,
            machine: "ab".repeat(32),
            scope: format!("{}/{}", "ab".repeat(32), "cd".repeat(16)),
            workspace_key: twr_core::sealed::HubKey::generate()?.public_hex(),
            box_name: "desk".into(),
            build: Build {
                sha256: "ef".repeat(32),
                size: 1,
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                protocol: 7,
            },
        };
        let old = options(&machine, &worker)?;
        worker.epoch = 2;
        let new = options(&machine, &worker)?;
        assert_eq!(old.paths.data_dir, new.paths.data_dir);
        assert_ne!(old.paths.socket, new.paths.socket);
        assert_ne!(
            old.managed.unwrap().revocation,
            new.managed.unwrap().revocation
        );
        Ok(())
    }
    #[tokio::test]
    async fn bulk_is_bounded_digest_checked_and_larger_than_json_limit() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let bytes = vec![47; 16 * 1024 * 1024 + 123];
        let build = Build {
            sha256: hex::encode(ring::digest::digest(&ring::digest::SHA256, &bytes)),
            size: bytes.len() as u64,
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            protocol: twr_core::runner::PROTOCOL_VERSION,
        };
        let path = dir.path().join("twr");
        let (mut send, mut recv) = tokio::io::duplex(65536);
        let sending = tokio::spawn(async move {
            send.write_all(&bytes).await.unwrap();
        });
        let temp = receive(&mut recv, &path, &build).await?;
        sending.await?;
        install(temp, &path)?;
        assert_eq!(Build::of(&path)?, build);
        let mut short = &b"short"[..];
        assert!(receive(&mut short, &path, &build).await.is_err());
        Ok(())
    }
}

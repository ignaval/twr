//! Owner-only local administration for the independent gateway. No device token or hub stop.
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{watch, Semaphore},
};

// Gateway diagnostics are bounded even when the desktop that launched it has exited.
static LOG_FILE: OnceLock<Mutex<PathBuf>> = OnceLock::new();
pub fn set_log_file(path: PathBuf) {
    let _ = LOG_FILE.set(Mutex::new(path));
}
pub fn log(message: &str) {
    if let Some(path) = LOG_FILE.get() {
        let path = path.lock().unwrap();
        let result = (|| -> std::io::Result<()> {
            if std::fs::symlink_metadata(&*path).is_ok_and(|metadata| metadata.len() > 512 * 1024) {
                std::fs::rename(&*path, path.with_extension("log.1"))?;
            }
            let mut file = OpenOptions::new()
                .append(true)
                .create(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&*path)?;
            let mut end = message.len().min(8192);
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            writeln!(file, "{}", &message[..end])
        })();
        if result.is_ok() {
            return;
        }
    }
    eprintln!("{message}");
}
#[macro_export]
macro_rules! gateway_log { ($($argument:tt)*) => { $crate::remote::gateway_lifecycle::log(&format!($($argument)*)) }; }

#[derive(Clone, Serialize, Deserialize)]
pub struct Info {
    pub gateway_id: String,
    pub running: bool,
    pub pid: u32,
    pub instance: String,
    pub version: String,
    pub executable: String,
    pub listen: String,
    pub tls_cert: String,
    pub tls_key: String,
    #[serde(default)]
    pub tls_cert_source: String,
    #[serde(default)]
    pub tls_key_source: String,
    pub certificate_sha256: String,
    #[serde(default)]
    pub renewal_state: String,
    #[serde(default)]
    pub renewal_error: Option<String>,
    pub started_at: u64,
}
impl Info {
    pub fn new(
        gateway_id: String,
        listen: String,
        cert: Option<&Path>,
        key: Option<&Path>,
    ) -> Result<Self> {
        Ok(Self {
            gateway_id,
            running: true,
            pid: std::process::id(),
            instance: uuid::Uuid::new_v4().to_string(),
            version: twr_core::version().into(),
            executable: std::env::current_exe()?
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
            listen,
            tls_cert: canonical(cert)?,
            tls_key: canonical(key)?,
            tls_cert_source: source(cert)?,
            tls_key_source: source(key)?,
            certificate_sha256: cert.map(cert_digest).transpose()?.unwrap_or_default(),
            renewal_state: "current".into(),
            renewal_error: None,
            started_at: super::auth::now(),
        })
    }
}
fn source(path: Option<&Path>) -> Result<String> {
    path.map(|path| std::path::absolute(path).map(|path| path.to_string_lossy().into_owned()))
        .transpose()
        .map(|path| path.unwrap_or_default())
        .map_err(Into::into)
}
fn canonical(path: Option<&Path>) -> Result<String> {
    path.map(|p| p.canonicalize().map(|p| p.to_string_lossy().into_owned()))
        .transpose()
        .map(|p| p.unwrap_or_default())
        .map_err(Into::into)
}
pub fn cert_digest(path: &Path) -> Result<String> {
    Ok(hex::encode(Sha256::digest(std::fs::read(path)?)))
}
fn private_dir(dir: &Path) -> Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let metadata = std::fs::symlink_metadata(dir)?;
    anyhow::ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() },
        "Unsafe gateway administration directory"
    );
    anyhow::ensure!(
        metadata.permissions().mode() & 0o077 == 0,
        "Gateway administration directory must be private (0700)"
    );
    Ok(())
}
/// The control socket's name.
const CONTROL_SOCKET: &str = "gateway-control.sock";

/// Where the control socket of gateway directory `dir` is: in `dir`, when that path fits a
/// Unix socket ([`twr_core::paths::MAX_SOCKET_PATH`]); else, so that a deep twr home still
/// works, in a directory of this user's alone named by a hash of `dir` (made private, 0700,
/// when `create`): in the per-user runtime directory (`$XDG_RUNTIME_DIR`, else
/// `/run/user/<uid>`, on Linux; the per-user temporary directory on macOS) when that is this
/// user's and private, else `/tmp/twr-gateway-<uid>-<hash>`. Every reader finds the same one:
/// the listener, `info`, `stop` and `cancel_invitation`. Whatever directory it is in, a
/// connection is taken only from a peer of this user (and, by a reader, only from the pid
/// the locked metadata names, [`verify_peer`]).
fn control_socket(dir: &Path, create: bool) -> Result<PathBuf> {
    let near = dir.join(CONTROL_SOCKET);
    if near.as_os_str().len() <= twr_core::paths::MAX_SOCKET_PATH {
        return Ok(near);
    }
    let real = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let hash = hex::encode(Sha256::digest(real.as_os_str().as_encoded_bytes()));
    let uid = unsafe { libc::geteuid() };
    let candidates = runtime_dir()
        .map(|r| r.join(format!("twr-gw-{}", &hash[..16])))
        .into_iter()
        .chain([PathBuf::from(format!(
            "/tmp/twr-gateway-{uid}-{}",
            &hash[..24]
        ))]);
    for socket_dir in candidates {
        let socket = socket_dir.join(CONTROL_SOCKET);
        if socket.as_os_str().len() > twr_core::paths::MAX_SOCKET_PATH {
            continue;
        }
        if create {
            private_dir(&socket_dir)?;
        }
        return Ok(socket);
    }
    anyhow::bail!(
        "the gateway's control socket {} is {} bytes, over the {} a Unix socket path can be, and \
         no shorter private directory is available: set TWR_HOME to a shorter directory",
        near.display(),
        near.as_os_str().len(),
        twr_core::paths::MAX_SOCKET_PATH
    )
}

/// A directory only this user can write to, when the system has one: `$XDG_RUNTIME_DIR`
/// (else `/run/user/<uid>`) on Linux, the per-user temporary directory on macOS.
fn runtime_dir() -> Option<PathBuf> {
    let private = |dir: &Path| {
        std::fs::symlink_metadata(dir).is_ok_and(|m| {
            m.is_dir()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.permissions().mode() & 0o077 == 0
        })
    };
    #[cfg(target_os = "macos")]
    {
        let mut buf = vec![0 as libc::c_char; 1024];
        let len =
            unsafe { libc::confstr(libc::_CS_DARWIN_USER_TEMP_DIR, buf.as_mut_ptr(), buf.len()) };
        if len == 0 || len > buf.len() {
            return None;
        }
        let dir = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
        use std::os::unix::ffi::OsStrExt;
        let dir = PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
        (dir.is_absolute() && private(&dir)).then_some(dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([PathBuf::from(format!("/run/user/{}", unsafe {
                libc::geteuid()
            }))])
            .find(|dir| dir.is_absolute() && private(dir))
    }
}

/// `gateway.lock`, opened by [`lock_file`]: unlocked when dropped, not only closed. A process
/// another thread starts meanwhile shares the open file until it execs, and a lock stays on
/// the file while anyone has it open: a gateway that stopped, or a lock merely tried, would
/// still seem held, to `info` (which then reads metadata that is gone) and to the next gateway.
struct LockFile(File);
impl std::ops::Deref for LockFile {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}
impl Drop for LockFile {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
fn lock_file(dir: &Path) -> Result<LockFile> {
    private_dir(dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("gateway.lock"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Unsafe gateway lock"
    );
    Ok(LockFile(file))
}
fn read_info(dir: &Path) -> Result<Info> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir.join("gateway.json"))?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file()
            && metadata.len() <= 32768
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "Unsafe gateway metadata"
    );
    let info: Info = serde_json::from_reader(file)?;
    anyhow::ensure!(
        info.pid > 0 && uuid::Uuid::parse_str(&info.instance).is_ok(),
        "Invalid gateway metadata"
    );
    Ok(info)
}
fn write_info(dir: &Path, info: &Info) -> Result<()> {
    let temporary = dir.join(format!(".gateway-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(info)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, dir.join("gateway.json"))?;
        Ok::<_, anyhow::Error>(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}
fn verify_peer(stream: &UnixStream, pid: Option<u32>) -> Result<()> {
    let peer = stream
        .peer_cred()
        .context("Cannot identify the gateway administration peer")?;
    anyhow::ensure!(
        peer.uid() == unsafe { libc::geteuid() },
        "Gateway administration peer belongs to another user"
    );
    if let Some(expected) = pid {
        anyhow::ensure!(
            peer.pid().and_then(|p| u32::try_from(p).ok()) == Some(expected),
            "Gateway administration PID does not match the locked metadata"
        );
    }
    Ok(())
}
async fn exchange(dir: &Path, info: &Info, operation: &str, code: Option<&str>) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = UnixStream::connect(control_socket(dir, false)?).await.context("Running gateway does not support verified local administration; restart it from the terminal that launched it")?;
        verify_peer(&stream, Some(info.pid))?;
        stream.write_all(format!("{}\n", json!({"operation":operation,"instance":info.instance,"code":code})).as_bytes()).await?;
        let mut response = Vec::new(); let mut reader = BufReader::new(stream).take(32769);
        anyhow::ensure!(reader.read_until(b'\n', &mut response).await? <= 32768, "Gateway administration reply exceeds its limit");
        let value: Value = serde_json::from_slice(&response)?;
        anyhow::ensure!(value["instance"] == info.instance, "Gateway instance changed during administration");
        anyhow::ensure!(value["error"].is_null(), "{}", value["error"].as_str().unwrap_or("Gateway administration failed"));
        Ok(value)
    }).await.context("Gateway administration timed out")?
}
pub async fn info(dir: &Path) -> Result<Value> {
    let lock = lock_file(dir)?;
    match lock.try_lock() {
        Ok(()) => Ok(
            json!({"running":false,"gateway_id":std::fs::read_to_string(dir.join("gateway-id")).unwrap_or_default().trim()}),
        ),
        Err(std::fs::TryLockError::WouldBlock) => {
            let expected = read_info(dir)?;
            let value = exchange(dir, &expected, "info", None).await?;
            Ok(value["info"].clone())
        }
        Err(error) => Err(error.into()),
    }
}
pub async fn stop(dir: &Path, expected_instance: &str) -> Result<()> {
    let lock = lock_file(dir)?;
    anyhow::ensure!(
        matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
        "The gateway is not running"
    );
    let expected = read_info(dir)?;
    anyhow::ensure!(
        expected.instance == expected_instance,
        "Gateway instance changed; check it again before restarting"
    );
    exchange(dir, &expected, "stop", None).await?;
    for _ in 0..150 {
        match lock.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("Gateway acknowledged the restart but has not released its listener yet")
}
pub async fn cancel_invitation(dir: &Path, code: &str) -> Result<bool> {
    anyhow::ensure!(
        !code.is_empty() && code.len() <= 128,
        "Invalid invitation code"
    );
    let expected = read_info(dir)?;
    let reply = exchange(dir, &expected, "cancel-invitation", Some(code)).await?;
    reply["cancelled"]
        .as_bool()
        .context("Invalid cancellation reply")
}
#[derive(Clone)]
pub struct Metadata {
    dir: PathBuf,
    /// The control socket ([`control_socket`]).
    socket: PathBuf,
    info: Arc<Mutex<Info>>,
    active: Arc<AtomicBool>,
}
impl Metadata {
    pub fn certificate(&self, digest: String, cert: &Path, key: &Path) -> Result<()> {
        let mut info = self.info.lock().unwrap();
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        let certificate = canonical(Some(cert))?;
        let private_key = canonical(Some(key))?;
        info.certificate_sha256 = digest;
        info.tls_cert = certificate;
        info.tls_key = private_key;
        info.renewal_state = "current".into();
        info.renewal_error = None;
        write_info(&self.dir, &info)
    }
}
impl Metadata {
    pub fn renewal(&self, state: &str, error: Option<&str>) -> Result<()> {
        let mut info = self.info.lock().unwrap();
        if !self.active.load(Ordering::SeqCst) {
            return Ok(());
        }
        info.renewal_state = state.to_owned();
        info.renewal_error = error.map(str::to_owned);
        write_info(&self.dir, &info)
    }
}
pub struct Lifecycle {
    _lock: LockFile,
    metadata: Metadata,
    administration: tokio::task::JoinHandle<()>,
    stopping: watch::Receiver<bool>,
}
impl Lifecycle {
    pub fn acquire(dir: &Path, info: Info) -> Result<Self> {
        let lock = lock_file(dir)?;
        let until = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < until => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => return Err(error).context("Another gateway is already running; check its configuration before restarting it"),
            }
        }
        let socket = control_socket(dir, true)?;
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket).with_context(|| {
            format!(
                "Cannot listen for gateway administration at {}",
                socket.display()
            )
        })?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        write_info(dir, &info)?;
        let metadata = Metadata {
            dir: dir.to_owned(),
            socket,
            info: Arc::new(Mutex::new(info)),
            active: Arc::new(AtomicBool::new(true)),
        };
        let shared = metadata.clone();
        let (stop, stopping) = watch::channel(false);
        let administration = tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(4));
            while let Ok((stream, _)) = listener.accept().await {
                if verify_peer(&stream, None).is_err() {
                    continue;
                }
                let Ok(slot) = slots.clone().try_acquire_owned() else {
                    continue;
                };
                let metadata = shared.clone();
                let stop = stop.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    let _ = tokio::time::timeout(Duration::from_secs(3), async move {
                        let (reader, mut writer) = stream.into_split(); let mut reader = BufReader::new(reader).take(4097); let mut line = Vec::new();
                        if reader.read_until(b'\n', &mut line).await? > 4096 { return Ok::<_, anyhow::Error>(()); }
                        let request: Value = serde_json::from_slice(&line)?;
                        let info = metadata.info.lock().unwrap().clone();
                        let (reply, shutdown) = if request["instance"] != info.instance { (json!({"instance":info.instance,"error":"Gateway instance changed"}), false) }
                            else { match request["operation"].as_str() { Some("info") => (json!({"instance":info.instance,"info":info}), false), Some("stop") => (json!({"instance":info.instance,"stopping":true}), true), Some("cancel-invitation") => {
                                let code = request["code"].as_str().context("Invitation code required")?;
                                anyhow::ensure!(!code.is_empty() && code.len() <= 128, "Invalid invitation code");
                                let auth = super::auth::Auth::open(&metadata.dir)?;
                                let cancelled = auth.cancel_pairing(code)?;
                                (json!({"instance":info.instance,"cancelled":cancelled}), false)
                            }, _ => (json!({"instance":info.instance,"error":"Unknown gateway administration operation"}), false) } };
                        writer.write_all(format!("{reply}\n").as_bytes()).await?; writer.shutdown().await?;
                        if shutdown { stop.send_replace(true); }
                        Ok(())
                    }).await;
                });
            }
        });
        Ok(Self {
            _lock: lock,
            metadata,
            administration,
            stopping,
        })
    }
    pub fn metadata(&self) -> Metadata {
        self.metadata.clone()
    }
    pub async fn stopping(&mut self) {
        if !*self.stopping.borrow() {
            let _ = self.stopping.changed().await;
        }
    }
}
impl Drop for Lifecycle {
    fn drop(&mut self) {
        self.administration.abort();
        let info = self.metadata.info.lock().unwrap();
        self.metadata.active.store(false, Ordering::SeqCst);
        let expected = info.instance.clone();
        if read_info(&self.metadata.dir).is_ok_and(|info| info.instance == expected) {
            let _ = std::fs::remove_file(&self.metadata.socket);
            // Its own short directory, when it has one (and nothing else is in it).
            if let Some(parent) = self
                .metadata
                .socket
                .parent()
                .filter(|p| *p != self.metadata.dir)
            {
                let _ = std::fs::remove_dir(parent);
            }
            let _ = std::fs::remove_file(self.metadata.dir.join("gateway.json"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn locked_owner_socket_reports_identity_and_fences_stale_stop_requests() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = Info::new("gateway-test".into(), "127.0.0.1:1234".into(), None, None).unwrap();
        let instance = record.instance.clone();
        let mut lifecycle = Lifecycle::acquire(dir.path(), record.clone()).unwrap();
        let watcher = lifecycle.metadata();
        let observed = info(dir.path()).await.unwrap();
        assert_eq!(observed["instance"], instance);
        assert_eq!(observed["pid"], std::process::id());
        assert_eq!(observed["running"], true);
        assert!(Lifecycle::acquire(dir.path(), record).is_err());
        assert!(stop(dir.path(), "stale-instance").await.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), lifecycle.stopping())
                .await
                .is_err()
        );
        let stopped = stop(dir.path(), &instance);
        let shutdown = async {
            lifecycle.stopping().await;
            drop(lifecycle);
        };
        let (result, ()) = tokio::join!(stopped, shutdown);
        result.unwrap();
        watcher
            .certificate("stale".into(), Path::new("/missing"), Path::new("/missing"))
            .unwrap();
        assert!(!dir.path().join("gateway.json").exists());
        assert_eq!(info(dir.path()).await.unwrap()["running"], false);
    }
    #[tokio::test]
    async fn owner_can_cancel_only_the_matching_invitation_without_a_hub() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let auth = super::super::auth::Auth::open(dir.path()).unwrap();
        let old = auth.begin(vec!["read".into()]).unwrap();
        let latest = auth.begin(vec!["read".into()]).unwrap();
        let lifecycle = Lifecycle::acquire(
            dir.path(),
            Info::new("gateway".into(), "127.0.0.1:1".into(), None, None).unwrap(),
        )
        .unwrap();
        assert!(!cancel_invitation(dir.path(), &old).await.unwrap());
        assert!(cancel_invitation(dir.path(), &latest).await.unwrap());
        assert!(auth.pair(&latest, "phone").is_err());
        drop(lifecycle);
    }
    #[tokio::test]
    async fn startup_retries_a_short_status_probe_lock() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let probe = lock_file(dir.path()).unwrap();
        probe.lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(probe);
        });
        let lifecycle = Lifecycle::acquire(
            dir.path(),
            Info::new("gateway".into(), "127.0.0.1:1".into(), None, None).unwrap(),
        )
        .unwrap();
        release.join().unwrap();
        drop(lifecycle);
    }
    /// A gateway directory so deep that its control socket would not fit a Unix socket path
    /// still serves its administration: through a private directory named by a hash of it,
    /// which every reader finds, and which the gateway's exit cleans up.
    #[tokio::test]
    async fn a_deep_gateway_directory_still_serves_its_administration() {
        let base = tempfile::tempdir_in("/tmp").unwrap();
        let dir = base.path().join("d".repeat(60)).join("mobile".repeat(4));
        private_dir(&dir).unwrap();
        assert!(dir.join(CONTROL_SOCKET).as_os_str().len() > twr_core::paths::MAX_SOCKET_PATH);
        let record = Info::new("gateway-test".into(), "127.0.0.1:1234".into(), None, None).unwrap();
        let instance = record.instance.clone();
        let mut lifecycle = Lifecycle::acquire(&dir, record).unwrap();
        let socket = control_socket(&dir, false).unwrap();
        assert!(
            !socket.starts_with(&dir) && socket.exists(),
            "{}",
            socket.display()
        );
        let parent = std::fs::symlink_metadata(socket.parent().unwrap()).unwrap();
        assert_eq!(parent.permissions().mode() & 0o777, 0o700);
        let observed = info(&dir).await.unwrap();
        assert_eq!(observed["instance"], instance);
        assert_eq!(observed["running"], true);
        let auth = super::super::auth::Auth::open(&dir).unwrap();
        let code = auth.begin(vec!["read".into()]).unwrap();
        assert!(cancel_invitation(&dir, &code).await.unwrap());
        let stopped = stop(&dir, &instance);
        let shutdown = async {
            lifecycle.stopping().await;
            drop(lifecycle);
        };
        let (result, ()) = tokio::join!(stopped, shutdown);
        result.unwrap();
        assert!(!socket.exists());
        assert_eq!(info(&dir).await.unwrap()["running"], false);
        assert!(
            !socket.parent().unwrap().exists(),
            "its short directory goes too"
        );
    }
    #[test]
    fn diagnostics_rotate_with_a_bounded_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gateway.log");
        set_log_file(path.clone());
        for _ in 0..150 {
            log(&"x".repeat(9000));
        }
        assert!(std::fs::metadata(path).unwrap().len() <= 512 * 1024 + 8193);
        assert!(
            std::fs::metadata(dir.path().join("gateway.log.1"))
                .unwrap()
                .len()
                <= 512 * 1024 + 8193
        );
    }
    #[tokio::test]
    async fn metadata_cannot_redirect_administration_to_another_pid() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let record = Info::new("gateway-test".into(), "127.0.0.1:1234".into(), None, None).unwrap();
        let lifecycle = Lifecycle::acquire(dir.path(), record.clone()).unwrap();
        let mut forged = record;
        forged.pid = forged.pid.saturating_add(1);
        write_info(dir.path(), &forged).unwrap();
        assert!(info(dir.path())
            .await
            .unwrap_err()
            .to_string()
            .contains("PID"));
        drop(lifecycle);
    }
}

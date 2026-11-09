//! Durable machine trust. The service is the only writer, holding an exclusive lifetime lock.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
};
use twr_core::paths::{ensure_private_dir, Paths};

#[derive(Clone, Debug)]
pub struct Machine {
    pub dir: PathBuf,
    pub socket: PathBuf,
}
/// The longest socket name the service directory holds: a worker daemon's
/// (`lr/<20 hex>.sock`, [`crate::worker::options`]).
const LONGEST_SOCKET: &str = "lr/0123456789abcdef0123.sock";

/// Whether `path` is a directory of this user that no one else can enter (not a link).
fn private_dir(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::symlink_metadata(path).is_ok_and(|m| {
        m.is_dir()
            && m.uid() == twr_core::paths::effective_uid()
            && m.permissions().mode() & 0o077 == 0
    })
}

/// A directory only this user can write to, for the service's sockets, when the system
/// has one: `$XDG_RUNTIME_DIR` (else `/run/user/<uid>`) on Linux, the per-user temporary
/// directory on macOS. Unlike `/tmp`, no other local user can create the service's directory
/// there first.
fn runtime_dir() -> Option<PathBuf> {
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
        (dir.is_absolute() && private_dir(&dir)).then_some(dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .into_iter()
            .chain([PathBuf::from(format!(
                "/run/user/{}",
                twr_core::paths::effective_uid()
            ))])
            .find(|dir| dir.is_absolute() && private_dir(dir))
    }
}

/// The service directory earlier releases used (and still the fallback): under `/tmp`.
fn legacy_dir(hash: &str) -> PathBuf {
    PathBuf::from("/tmp").join(format!(
        "twr-runner-{}-{}",
        twr_core::paths::effective_uid(),
        &hash[..24]
    ))
}

/// The digest of the data directory that names its service's socket directory.
fn data_hash(data: &Path) -> String {
    hex::encode(ring::digest::digest(
        &ring::digest::SHA256,
        data.as_os_str().as_encoded_bytes(),
    ))
}

/// The service's socket directory in `runtime` (for data hash `hash`), when every socket it
/// will hold fits a Unix socket path.
fn runtime_socket_dir(runtime: &Path, hash: &str) -> Option<PathBuf> {
    let dir = runtime.join(format!("twr-{}", &hash[..16]));
    (dir.join(LONGEST_SOCKET).as_os_str().len() <= twr_core::paths::MAX_SOCKET_PATH).then_some(dir)
}

impl Machine {
    pub fn of(paths: &Paths) -> Self {
        let root = paths.root();
        let data = twr_core::paths::resolve_lenient(&root.data_dir).unwrap_or(root.data_dir);
        let hash = data_hash(&data);
        let dir = runtime_dir()
            .and_then(|runtime| runtime_socket_dir(&runtime, &hash))
            .unwrap_or_else(|| legacy_dir(&hash));
        Self {
            dir: data.join("machine"),
            socket: dir.join("runner.sock"),
        }
    }
    /// Where a service an earlier release started listens, while it still does: its socket
    /// directory under `/tmp`, when that is this user's and private and differs from
    /// [`Machine::socket`]'s. A client reaches such a service there until it is replaced.
    pub fn legacy_socket(&self) -> Option<PathBuf> {
        let dir = legacy_dir(&data_hash(self.dir.parent()?));
        let socket = dir.join("runner.sock");
        (socket != self.socket && private_dir(&dir)).then_some(socket)
    }
    /// The directory of worker daemon sockets: next to the service socket, or, for a daemon
    /// an earlier release started (its socket `name` still there), the legacy directory.
    pub fn worker_sockets(&self, name: &str) -> Result<PathBuf> {
        if let Some(legacy) = self.legacy_socket() {
            let old = legacy
                .parent()
                .context("legacy socket has no parent")?
                .join("lr");
            if private_dir(&old) && std::fs::symlink_metadata(old.join(name)).is_ok() {
                return Ok(old);
            }
        }
        Ok(self
            .socket
            .parent()
            .context("socket parent missing")?
            .join("lr"))
    }
    pub fn init(&self) -> Result<()> {
        twr_core::paths::check_socket_path(&self.socket)?;
        ensure_private_dir(&self.dir)?;
        ensure_private_dir(
            self.socket
                .parent()
                .context("runner socket has no parent")?,
        )?;
        Ok(())
    }
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("runners.toml")
    }
    pub fn load(&self) -> Result<State> {
        match read_private(&self.state_path(), 1024 * 1024) {
            Ok(bytes) => Ok(toml::from_str(std::str::from_utf8(&bytes)?)?),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(State::default())
            }
            Err(e) => Err(e),
        }
    }
    pub fn fence(&self, machine: &str, epoch: u64) -> PathBuf {
        self.dir
            .join("runner/retired")
            .join(machine)
            .join(format!("{epoch}.json"))
    }
    pub fn retire(&self, machine: &str, epoch: u64) -> Result<()> {
        ensure!(
            machine.len() == 64
                && machine
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid machine identity"
        );
        atomic_write(&self.fence(machine, epoch), b"revoked")
    }
    pub fn reconcile(&self, state: &mut State) -> Result<()> {
        for (key, owner) in &mut state.owners {
            if !owner.revoked && self.fence(key, owner.epoch).try_exists()? {
                owner.revoked = true;
                owner.epoch = owner
                    .epoch
                    .checked_add(1)
                    .context("trust epoch exhausted")?;
            }
        }
        self.save(state)
    }
    pub fn save(&self, state: &State) -> Result<()> {
        let bytes = toml::to_string(state)?;
        ensure!(bytes.len() <= 1024 * 1024, "runner registry exceeds limit");
        atomic_write(&self.state_path(), bytes.as_bytes())
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct State {
    pub disabled: bool,
    pub allow_sleep: bool,
    pub runners: BTreeMap<String, Runner>,
    pub pending_revocations: BTreeMap<String, Runner>,
    pub owners: BTreeMap<String, Owner>,
    pub workers: BTreeMap<String, Worker>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    #[serde(default)]
    pub addresses: Vec<std::net::SocketAddr>,
    pub endpoint: String,
    pub relay: String,
    pub receipt: String,
    pub committed: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub receipt: String,
    pub committed: bool,
    pub revoked: bool,
    pub epoch: u64,
}

/// Atomic replacement and directory fsync: a successful acknowledgement survives a crash.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().context("file has no parent")?;
    ensure_private_dir(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}
pub fn read_private(path: &Path, max: u64) -> Result<Vec<u8>> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.permissions().mode() & 0o077 == 0
            && meta.uid() == twr_core::paths::effective_uid(),
        "{} must be an owner-only regular file",
        path.display()
    );
    ensure!(meta.len() <= max, "file exceeds limit");
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= max, "file exceeds limit");
    Ok(bytes)
}
pub fn secret(path: &Path) -> Result<[u8; 32]> {
    match read_private(path, 32) {
        Ok(bytes) => bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid identity key")),
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            // Only called by the service while its lifetime lock is held.
            let secret = crate::random()?;
            atomic_write(path, &secret)?;
            Ok(secret)
        }
        Err(e) => Err(e),
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Worker {
    pub epoch: u64,
    pub machine: String,
    pub scope: String,
    pub workspace_key: String,
    pub box_name: String,
    pub build: crate::worker::Build,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn durable_retirement_wins_over_an_interrupted_state_save() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let machine = Machine {
            dir: dir.path().join("machine"),
            socket: dir.path().join("run/sock"),
        };
        machine.init()?;
        let key = "ab".repeat(32);
        let mut state = State::default();
        state.owners.insert(
            key.clone(),
            Owner {
                receipt: "receipt".into(),
                committed: true,
                revoked: false,
                epoch: 7,
            },
        );
        machine.save(&state)?;
        machine.retire(&key, 7)?;
        let mut recovered = machine.load()?;
        assert!(!recovered.owners[&key].revoked);
        machine.reconcile(&mut recovered)?;
        assert!(machine.load()?.owners[&key].revoked);
        assert_eq!(machine.load()?.owners[&key].epoch, 8);
        assert!(machine.fence(&key, 7).exists());
        Ok(())
    }
    #[test]
    fn the_service_socket_prefers_a_private_runtime_directory() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let data = std::fs::canonicalize(dir.path())?;
        let paths = Paths {
            data_dir: data.clone(),
            socket: data.join("hub.sock"),
            config_file: data.join("config.toml"),
            workspace: None,
        };
        let machine = Machine::of(&paths);
        let hash = data_hash(&data);
        match runtime_dir() {
            Some(runtime) => {
                assert!(machine.socket.starts_with(&runtime), "{:?}", machine.socket);
                assert!(!machine.socket.starts_with("/tmp"));
            }
            None => assert_eq!(machine.socket, legacy_dir(&hash).join("runner.sock")),
        }
        let socket_dir = machine.socket.parent().unwrap();
        assert!(
            socket_dir.join(LONGEST_SOCKET).as_os_str().len() <= twr_core::paths::MAX_SOCKET_PATH
        );
        // A base too long for a worker socket falls back to /tmp.
        assert!(
            runtime_socket_dir(&PathBuf::from(format!("/{}", "r".repeat(80))), &hash).is_none()
        );
        // A service an earlier release started stays reachable at its /tmp socket, and so do
        // its worker daemons; new daemons go next to the new socket.
        let legacy = legacy_dir(&hash);
        if machine.socket.starts_with(&legacy) {
            return Ok(());
        }
        assert_eq!(machine.legacy_socket(), None);
        ensure_private_dir(&legacy)?;
        ensure_private_dir(&legacy.join("lr"))?;
        struct Remove(PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _remove = Remove(legacy.clone());
        assert_eq!(machine.legacy_socket(), Some(legacy.join("runner.sock")));
        let _old = std::os::unix::net::UnixListener::bind(legacy.join("lr/old.sock"))?;
        assert_eq!(machine.worker_sockets("old.sock")?, legacy.join("lr"));
        assert_eq!(machine.worker_sockets("new.sock")?, socket_dir.join("lr"));
        // Another user's (or an open) /tmp directory is never trusted.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o755))?;
        assert_eq!(machine.legacy_socket(), None);
        assert_eq!(machine.worker_sockets("old.sock")?, socket_dir.join("lr"));
        Ok(())
    }
    #[test]
    fn private_atomic_state_survives_reload_and_refuses_symlinks() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let file = dir.path().join("state");
        atomic_write(&file, b"one")?;
        atomic_write(&file, b"two")?;
        assert_eq!(read_private(&file, 3)?, b"two");
        assert!(read_private(&file, 2).is_err());
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&file, &alias)?;
        assert!(read_private(&alias, 3).is_err());
        Ok(())
    }
}

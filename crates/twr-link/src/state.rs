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
impl Machine {
    pub fn of(paths: &Paths) -> Self {
        let root = paths.root();
        let data = twr_core::paths::resolve_lenient(&root.data_dir).unwrap_or(root.data_dir);
        let hash = hex::encode(ring::digest::digest(
            &ring::digest::SHA256,
            data.as_os_str().as_encoded_bytes(),
        ));
        Self {
            dir: data.join("machine"),
            socket: PathBuf::from("/tmp")
                .join(format!(
                    "twr-runner-{}-{}",
                    twr_core::paths::effective_uid(),
                    &hash[..24]
                ))
                .join("runner.sock"),
        }
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

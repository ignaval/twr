//! Restrictions for machine-managed linked workers. All paths stay within one full-key scope.
//! This is protocol confinement for trusted owner hubs, not an OS sandbox for same-user agents.
use anyhow::{ensure, Result};
use std::{
    os::fd::AsRawFd,
    path::{Component, Path, PathBuf},
};
use twr_core::runner::Op;
#[derive(Clone, Debug)]
pub struct Managed {
    pub root: PathBuf,
    pub resources: PathBuf,
    pub revocation: PathBuf,
}
impl Managed {
    pub fn slots(resources: &Path) -> Result<u32> {
        let file = resources.join("slots.json");
        match std::fs::read(&file) {
            Ok(bytes) => {
                let slots: u32 = serde_json::from_slice(&bytes)?;
                ensure!(
                    (1..=64).contains(&slots),
                    "runtime slots must be between 1 and 64"
                );
                Ok(slots)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
                let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                let memory = if pages > 0 && page_size > 0 {
                    (pages as u64).saturating_mul(page_size as u64)
                } else {
                    8 * 1024 * 1024 * 1024
                };
                Ok((memory / (2 * 1024 * 1024 * 1024)).clamp(1, 4) as u32)
            }
            Err(e) => Err(e.into()),
        }
    }
    pub fn active(&self) -> Result<()> {
        ensure!(!self.revocation.try_exists()?, "linked hub was revoked");
        Ok(())
    }
    pub fn path(&self, path: &Path) -> Result<()> {
        ensure!(
            path.is_absolute() && path.starts_with(&self.root),
            "path is outside this hub's managed root"
        );
        ensure!(
            !path
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir)),
            "non-canonical managed path"
        );
        let mut current = path.to_path_buf();
        loop {
            match std::fs::symlink_metadata(&current) {
                Ok(_) => {
                    let canonical = std::fs::canonicalize(&current)?;
                    ensure!(
                        canonical.starts_with(&self.root),
                        "managed path escapes through a symlink"
                    );
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    ensure!(current.pop(), "managed path has no ancestor")
                }
                Err(e) => return Err(e.into()),
            }
        }
        // Git worktrees can redirect outside their visible directory through .git or
        // commondir. Validate those indirections before any managed filesystem operation.
        let mut ancestor = path;
        while ancestor.starts_with(&self.root) {
            let dot = ancestor.join(".git");
            if std::fs::symlink_metadata(&dot).is_ok() {
                let dot = std::fs::canonicalize(dot)?;
                ensure!(
                    dot.starts_with(&self.root),
                    "Git metadata escapes the managed root"
                );
                let git = if dot.is_file() {
                    ensure!(
                        dot.metadata()?.len() <= 4096,
                        "Git metadata pointer exceeds limit"
                    );
                    let text = std::fs::read_to_string(&dot)?;
                    let target = text
                        .trim_end()
                        .strip_prefix("gitdir: ")
                        .ok_or_else(|| anyhow::anyhow!("invalid Git metadata pointer"))?;
                    std::fs::canonicalize(ancestor.join(target))?
                } else {
                    dot
                };
                ensure!(
                    git.starts_with(&self.root),
                    "Git metadata escapes the managed root"
                );
                let common = git.join("commondir");
                if common.exists() {
                    ensure!(
                        common.metadata()?.len() <= 4096,
                        "Git common directory pointer exceeds limit"
                    );
                    let common = std::fs::canonicalize(
                        git.join(std::fs::read_to_string(common)?.trim_end()),
                    )?;
                    ensure!(
                        common.starts_with(&self.root),
                        "Git common directory escapes the managed root"
                    );
                }
                break;
            }
            let Some(parent) = ancestor.parent() else {
                break;
            };
            ancestor = parent;
        }
        Ok(())
    }
    pub fn check(&self, op: &Op) -> Result<()> {
        self.active()?;
        match op {
            Op::Spawn { cfg, .. } | Op::Resume { cfg, .. } | Op::AttachInfo { cfg, .. } => {
                self.path(&cfg.cwd)?;
                if let Some(path) = &cfg.mcp_config_file {
                    self.path(path)?;
                }
            }
            Op::RepoRoot { dir } | Op::GitCommonDir { dir } => self.path(dir)?,
            Op::Upload { root, .. } => self.path(root)?,
            Op::WorktreeCreate { repo, path, .. }
            | Op::WorktreeRemove { repo, path, .. }
            | Op::WorktreeInventory { repo, path, .. } => {
                self.path(repo)?;
                self.path(path)?;
            }
            Op::WorktreePrune { repo } => self.path(repo)?,
            Op::WorktreeExists { path } | Op::GitSnapshot { path, .. } => self.path(path)?,
            Op::WorktreeTrash { request, .. } => {
                self.path(&request.repo)?;
                self.path(&request.worktree)?;
            }
            Op::BranchOnlyCommits { targets } => {
                for target in targets {
                    self.path(&target.repo)?;
                    if let Some(path) = &target.worktree {
                        self.path(path)?;
                    }
                }
            }
            Op::TrashRestore { request } => {
                for path in &request.paths {
                    self.path(path)?;
                }
            }
            Op::DiskUsage {
                worktree: Some(path),
                ..
            } => self.path(path)?,
            Op::Send { .. }
            | Op::Respond { .. }
            | Op::Stop { .. }
            | Op::TrashList {}
            | Op::TrashPurge { .. }
            | Op::TrashRestoreUndo { .. }
            | Op::DiskUsage { worktree: None, .. }
            | Op::Agents { .. } => {}
        }
        Ok(())
    }
    /// Cross-process kernel leases survive service restarts and release on worker exit.
    /// Four resident runtimes by default; active work is never preempted for admission.
    #[cfg(test)]
    pub fn lease(&self) -> Result<std::fs::File> {
        self.lease_inner(None)
    }
    fn lease_inner(&self, marker: Option<&LeaseMarker>) -> Result<std::fs::File> {
        twr_core::paths::ensure_private_dir(&self.resources)?;
        let gate = twr_core::paths::open_private_append(&self.resources.join("slots.lock"))?;
        ensure!(
            unsafe { libc::flock(gate.as_raw_fd(), libc::LOCK_EX) } == 0,
            "cannot lock runtime admission"
        );
        let limit = Self::slots(&self.resources)?;
        let mut available = None;
        let mut occupied = 0;
        for slot in 0..64 {
            let file = twr_core::paths::open_private_append(
                &self.resources.join(format!("runtime-{slot}.lock")),
            )?;
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                let path = self.resources.join(format!("runtime-{slot}.json"));
                if let Ok(bytes) = std::fs::read(&path) {
                    ensure!(bytes.len() <= 4096, "runtime lease metadata exceeds limit");
                    let old: LeaseMarker = serde_json::from_slice(&bytes)?;
                    match crate::reap::process_liveness(old.pid, old.start) {
                        crate::reap::Alive::No => {
                            let _ = crate::reap::reap_marked(
                                &old.thread,
                                old.min_start,
                                Some(&old.generation),
                            );
                            ensure!(
                                crate::reap::marked_processes(
                                    &old.thread,
                                    old.min_start,
                                    Some(&old.generation)
                                )
                                .is_some_and(|p| p.is_empty()),
                                "could not retire an orphan runtime; waiting for recovery"
                            );
                        }
                        crate::reap::Alive::Unknown => {
                            occupied += 1;
                            continue;
                        }
                        crate::reap::Alive::Yes => {}
                    }
                    std::fs::remove_file(path)?;
                }
                if available.is_none() {
                    available = Some((file, slot));
                }
            } else {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error.into());
                }
                occupied += 1;
            }
        }
        if occupied < limit {
            if let Some((file, slot)) = available {
                let path = self.resources.join(format!("runtime-{slot}.json"));
                if let Some(marker) = marker {
                    use std::io::Write;
                    let mut tmp = tempfile::NamedTempFile::new_in(&self.resources)?;
                    tmp.write_all(&serde_json::to_vec(marker)?)?;
                    tmp.as_file().sync_all()?;
                    tmp.persist(&path).map_err(|e| e.error)?;
                    std::fs::File::open(&self.resources)?.sync_all()?;
                }
                return Ok(file);
            }
        }
        Err(Capacity.into())
    }
}
pub async fn revoked(policy: Option<Managed>) {
    let Some(policy) = policy else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if policy.active().is_err() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

#[derive(Debug)]
pub struct Capacity;
impl std::fmt::Display for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("runner capacity is full; waiting for a runtime lease")
    }
}
impl std::error::Error for Capacity {}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct Queue {
    #[serde(default)]
    next: u64,
    last_owner: String,
    tickets: Vec<Ticket>,
}
#[derive(serde::Serialize, serde::Deserialize)]
struct Ticket {
    thread: String,
    generation: String,
    owner: String,
    key: String,
    created: u64,
    seen: u64,
    pid: u32,
    start: Option<u64>,
}
impl Managed {
    /// Round-robin across full machine owners, FIFO within each owner. Only metadata is
    /// persisted here; kernel locks remain the authority for occupied runtime slots.
    pub fn admit(&self, thread: &str, generation: &str) -> Result<std::fs::File> {
        use std::io::{Read, Seek, Write};
        self.active()?;
        disk_admission(&self.root)?;
        twr_core::paths::ensure_private_dir(&self.resources)?;
        let mut file =
            twr_core::paths::open_private_append(&self.resources.join("admission.json"))?;
        // open_private_append is write-only: use a separate no-follow read/write fd.
        drop(file);
        use std::os::unix::fs::OpenOptionsExt;
        file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.resources.join("admission.json"))?;
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "cannot lock runner admission"
        );
        let mut bytes = Vec::new();
        (&mut file).take(512 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 512 * 1024,
            "runner admission queue exceeds limit"
        );
        let mut queue: Queue = serde_json::from_slice(&bytes).unwrap_or_default();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        queue.tickets.retain(|t| {
            now >= t.seen
                && now.saturating_sub(t.seen) < 5_000
                && crate::reap::process_liveness(t.pid, t.start) != crate::reap::Alive::No
        });
        let prefix = self
            .resources
            .parent()
            .unwrap_or(std::path::Path::new("/"))
            .join("hubs");
        let owner = self
            .root
            .strip_prefix(prefix)
            .ok()
            .and_then(|p| p.components().next())
            .map(|p| p.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| fingerprint(self.root.as_os_str().as_encoded_bytes()));
        let key = fingerprint(format!("{}:{thread}:{generation}", self.root.display()).as_bytes());
        if let Some(ticket) = queue.tickets.iter_mut().find(|t| t.key == key) {
            ticket.seen = now;
        } else if queue.tickets.len() < 1024 {
            queue.next = queue
                .next
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("admission sequence exhausted"))?;
            queue.tickets.push(Ticket {
                thread: thread.into(),
                generation: generation.into(),
                owner: owner.clone(),
                key: key.clone(),
                created: queue.next,
                seen: now,
                pid: std::process::id(),
                start: crate::reap::own_start_time(),
            });
        }
        let owners: std::collections::BTreeSet<_> =
            queue.tickets.iter().map(|t| t.owner.clone()).collect();
        let next = owners
            .iter()
            .find(|o| *o > &queue.last_owner)
            .or_else(|| owners.first());
        let selected = next
            .and_then(|owner| {
                queue
                    .tickets
                    .iter()
                    .filter(|t| &t.owner == owner)
                    .min_by_key(|t| (t.created, &t.key))
            })
            .is_some_and(|t| t.key == key);
        let lease = if selected {
            let marker = LeaseMarker {
                revocation: self.revocation.clone(),
                pid: std::process::id(),
                start: crate::reap::own_start_time(),
                min_start: crate::reap::own_start_time().unwrap_or(0),
                thread: thread.into(),
                generation: generation.into(),
            };
            match self.lease_inner(Some(&marker)) {
                Ok(file) => Some(file),
                Err(e) if e.is::<Capacity>() => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        if lease.is_some() {
            queue.last_owner = owner;
            queue.tickets.retain(|t| t.key != key);
        }
        let bytes = serde_json::to_vec(&queue)?;
        file.set_len(0)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(&bytes)?;
        lease.ok_or_else(|| Capacity.into())
    }
    pub fn cancel_admission(&self, thread: &str, run: Option<&str>) -> Result<()> {
        use std::io::{Read, Seek, Write};
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.resources.join("admission.json"))
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        ensure!(
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
            "cannot lock runner admission"
        );
        let mut bytes = Vec::new();
        (&mut file).take(512 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 512 * 1024, "admission queue exceeds limit");
        let mut queue: Queue = serde_json::from_slice(&bytes).unwrap_or_default();
        queue
            .tickets
            .retain(|t| t.thread != thread || run.is_some_and(|run| run != t.generation));
        file.set_len(0)?;
        file.rewind()?;
        file.write_all(&serde_json::to_vec(&queue)?)?;
        Ok(())
    }
    /// A daemon may have died with a runtime still alive. Revoke its recorded generation
    /// even when no daemon socket remains; an old grant can never select a newer run.
    pub fn retire_grant(resources: &Path, fence: &Path) -> Result<()> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let mut failed = 0;
        for slot in 0..64 {
            let path = resources.join(format!("runtime-{slot}.json"));
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::warn!("cannot read runtime lease during revocation: {e}");
                    failed += 1;
                    continue;
                }
            };
            let mut bytes = Vec::new();
            file.take(4097).read_to_end(&mut bytes)?;
            if bytes.len() > 4096 {
                failed += 1;
                continue;
            }
            let marker: LeaseMarker = match serde_json::from_slice(&bytes) {
                Ok(value) => value,
                Err(_) => {
                    failed += 1;
                    continue;
                }
            };
            if marker.revocation != fence {
                continue;
            }
            let _ = crate::reap::reap_marked(
                &marker.thread,
                marker.min_start,
                Some(&marker.generation),
            );
            if !crate::reap::marked_processes(
                &marker.thread,
                marker.min_start,
                Some(&marker.generation),
            )
            .is_some_and(|left| left.is_empty())
            {
                failed += 1;
            }
        }
        ensure!(
            failed == 0,
            "{failed} runtime lease(s) could not be retired; inspect runner diagnostics"
        );
        Ok(())
    }
    pub fn retire_fenced(resources: &Path) -> Result<()> {
        let mut fences = std::collections::HashSet::new();
        for slot in 0..64 {
            let path = resources.join(format!("runtime-{slot}.json"));
            match std::fs::read(path) {
                Ok(bytes) => {
                    ensure!(bytes.len() <= 4096, "runtime lease metadata exceeds limit");
                    let marker: LeaseMarker = serde_json::from_slice(&bytes)?;
                    if !marker.revocation.as_os_str().is_empty()
                        && marker.revocation.try_exists()?
                    {
                        fences.insert(marker.revocation);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        let mut failed = false;
        for fence in fences {
            if Self::retire_grant(resources, &fence).is_err() {
                failed = true;
            }
        }
        ensure!(
            !failed,
            "some fenced runtime generations could not be retired"
        );
        Ok(())
    }
    pub fn at_capacity(resources: &Path) -> Result<bool> {
        let Some(_gate) = try_admission_gate(resources)? else {
            return Ok(false);
        };
        let mut occupied = 0;
        for slot in 0..64 {
            let path = resources.join(format!("runtime-{slot}.lock"));
            if let Ok(file) = std::fs::File::open(path) {
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                    occupied += 1;
                }
            }
        }
        Ok(occupied >= Self::slots(resources)?)
    }
    pub fn pressure(resources: &Path) -> bool {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(resources.join("admission.json"))
        else {
            return false;
        };
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            return false;
        }
        let mut bytes = Vec::new();
        if (&mut file)
            .take(512 * 1024 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            return false;
        }
        let Ok(queue) = serde_json::from_slice::<Queue>(&bytes) else {
            return false;
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        queue.tickets.iter().any(|t| {
            now >= t.seen
                && now.saturating_sub(t.seen) < 5_000
                && crate::reap::process_liveness(t.pid, t.start) != crate::reap::Alive::No
        })
    }
}

fn fingerprint(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let mut output = String::with_capacity(64);
    for byte in sha2::Sha256::digest(bytes) {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Keep the same reserve for runtime admission as bulk/build admission. Existing work continues.
#[allow(clippy::unnecessary_cast)]
pub fn disk_admission(root: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(root.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    ensure!(
        unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } == 0,
        "cannot inspect runner disk space"
    );
    let stat = unsafe { stat.assume_init() };
    let free = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    let total = (stat.f_blocks as u64).saturating_mul(stat.f_frsize as u64);
    ensure!(
        free >= (5 * 1024 * 1024 * 1024u64).max(total / 10),
        "runner disk reserve reached; reclaim disk space before admitting work"
    );
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LeaseMarker {
    #[serde(default)]
    revocation: PathBuf,
    pid: u32,
    start: Option<u64>,
    min_start: u64,
    thread: String,
    generation: String,
}

/// Coordinate brief lease probes with allocation, so a probe never occupies a runtime/build slot.
pub fn admission_gate(resources: &Path) -> Result<std::fs::File> {
    twr_core::paths::ensure_private_dir(resources)?;
    let file = twr_core::paths::open_private_append(&resources.join("slots.lock"))?;
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0,
        "cannot lock admission probes"
    );
    Ok(file)
}

/// A busy allocator must never block the service's expiry or control loop.
pub fn try_admission_gate(resources: &Path) -> Result<Option<std::fs::File>> {
    twr_core::paths::ensure_private_dir(resources)?;
    let file = twr_core::paths::open_private_append(&resources.join("slots.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn revocation_reaps_an_orphan_generation_and_preserves_a_replacement() -> Result<()> {
        use std::os::unix::process::CommandExt;
        struct Reaped(std::process::Child);
        impl Drop for Reaped {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temp = tempfile::tempdir()?;
        let resources = temp.path().join("leases");
        twr_core::paths::ensure_private_dir(&resources)?;
        let thread = twr_core::event::new_id();
        let old_run = twr_core::event::new_id();
        let new_run = twr_core::event::new_id();
        let spawn = |run: &str| -> Result<Reaped> {
            Ok(Reaped(
                std::process::Command::new("sleep")
                    .arg("30")
                    .env(crate::reap::THREAD_ENV, &thread)
                    .env(crate::reap::RUNTIME_GEN_ENV, run)
                    .env(crate::reap::AGENT_ENV, format!("{thread}:{run}"))
                    .process_group(0)
                    .spawn()?,
            ))
        };
        let mut old = spawn(&old_run)?;
        let mut new = spawn(&new_run)?;
        let old_fence = temp.path().join("old-fence");
        let new_fence = temp.path().join("new-fence");
        for (slot, run, fence) in [(0, old_run, &old_fence), (1, new_run, &new_fence)] {
            let marker = LeaseMarker {
                revocation: fence.clone(),
                pid: u32::MAX,
                start: None,
                min_start: 0,
                thread: thread.clone(),
                generation: run,
            };
            std::fs::write(
                resources.join(format!("runtime-{slot}.json")),
                serde_json::to_vec(&marker)?,
            )?;
        }
        Managed::retire_grant(&resources, &old_fence)?;
        assert!(
            old.0.try_wait()?.is_some(),
            "the dead daemon's runtime survived revocation"
        );
        assert!(
            new.0.try_wait()?.is_none(),
            "revocation killed a replacement generation"
        );
        Ok(())
    }
    #[test]
    fn pending_owners_take_turns_and_fifo_survives_retries() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let resources = temp.path().join("leases");
        twr_core::paths::ensure_private_dir(&resources)?;
        std::fs::write(resources.join("slots.json"), b"1")?;
        let policy = |owner: &str| {
            let root = temp
                .path()
                .join("hubs")
                .join(owner)
                .join("workspace/boxes/desk");
            std::fs::create_dir_all(&root).unwrap();
            Managed {
                root,
                resources: resources.clone(),
                revocation: temp.path().join("retired"),
            }
        };
        let a = policy("owner-a");
        let b = policy("owner-b");
        let live = a.admit("a1", "first")?;
        assert!(a.admit("a2", "second").is_err());
        assert!(b.admit("b1", "third").is_err());
        drop(live);
        assert!(
            a.admit("a2", "second").is_err(),
            "a retry jumped ahead of another waiting owner"
        );
        let live = b.admit("b1", "third")?;
        drop(live);
        assert!(a.admit("a2", "second").is_ok());
        Ok(())
    }
    #[test]
    fn confinement_and_global_leases_are_shared_across_workers() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("hub");
        std::fs::create_dir(&root)?;
        let managed = Managed {
            root: root.clone(),
            resources: dir.path().join("leases"),
            revocation: dir.path().join("revoked"),
        };
        assert!(managed.path(&root.join("new/child")).is_ok());
        assert!(managed.path(&root.join("../other")).is_err());
        std::os::unix::fs::symlink(dir.path(), root.join("escape"))?;
        assert!(managed.path(&root.join("escape/private")).is_err());
        assert!((1..=4).contains(&Managed::slots(&managed.resources)?));
        twr_core::paths::ensure_private_dir(&managed.resources)?;
        std::fs::write(managed.resources.join("slots.json"), b"4")?;
        let leases: Vec<_> = (0..4).map(|_| managed.lease().unwrap()).collect();
        assert!(managed.lease().is_err());
        drop(leases);
        assert!(managed.lease().is_ok());
        Ok(())
    }
}

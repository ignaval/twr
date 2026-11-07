//! Reclaim disposable build caches only. Repositories, spools, worktrees and trust fences stay intact.
use crate::state::{Machine, State};
use anyhow::Result;
use std::{collections::HashSet, os::fd::AsRawFd, path::Path, time::Duration};

#[derive(serde::Serialize, serde::Deserialize)]
pub struct HubRef {
    pub pid: u32,
    pub start: Option<u64>,
    pub build: String,
}
fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn directories(root: &Path) -> Result<Vec<std::fs::DirEntry>> {
    match std::fs::read_dir(root) {
        Ok(entries) => {
            let mut result = Vec::new();
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    result.push(entry);
                }
            }
            Ok(result)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}
fn prune_slots(root: &Path, keep: &HashSet<String>, aggressive: bool) -> Result<u64> {
    let mut entries = directories(root)?;
    entries.retain(|e| e.file_name().to_str().is_some_and(digest));
    entries.sort_by_key(|e| std::cmp::Reverse(e.metadata().and_then(|m| m.modified()).ok()));
    let mut reclaimed = 0;
    // Retain a rollback slot and a day of grace, including builds being installed/opened.
    let mut spare = false;
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let old = entry
            .metadata()?
            .modified()?
            .elapsed()
            .is_ok_and(|age| age > Duration::from_secs(86400));
        if keep.contains(&name) {
            continue;
        }
        if !spare {
            spare = true;
            continue;
        }
        if old || aggressive {
            let bytes = crate::bundle::directory_bytes(&entry.path(), u64::MAX)?;
            std::fs::remove_dir_all(entry.path())?;
            reclaimed += bytes;
        }
    }
    Ok(reclaimed)
}
pub fn prune(machine: &Machine, state: &State, aggressive: bool) -> Result<u64> {
    let mut reclaimed = 0;
    let hubs = machine.dir.join("hub-builds");
    let mut keep = HashSet::new();
    match std::fs::read_dir(hubs.join(".refs")) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                let record = crate::state::read_private(&entry.path(), 4096)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<HubRef>(&b).ok());
                if let Some(record) = record {
                    if twr_runner::reap::process_liveness(record.pid, record.start)
                        != twr_runner::reap::Alive::No
                    {
                        keep.insert(record.build);
                    } else {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    reclaimed += prune_slots(&hubs, &keep, aggressive)?;
    let mut keep = HashSet::new();
    for path in [
        std::env::current_exe().ok(),
        std::fs::read(machine.dir.join("service/current"))
            .ok()
            .map(|b| {
                use std::os::unix::ffi::OsStringExt;
                std::path::PathBuf::from(std::ffi::OsString::from_vec(b))
            }),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(name) = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
        {
            keep.insert(name.to_string());
        }
    }
    reclaimed += prune_slots(&machine.dir.join("service"), &keep, aggressive)?;
    let resources = machine.dir.join("runner/leases");
    twr_core::paths::ensure_private_dir(&resources)?;
    let build_lock = twr_core::paths::open_private_append(&resources.join("build.lock"))?;
    let can_prune_native =
        unsafe { libc::flock(build_lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
    for owner in directories(&machine.dir.join("runner/hubs"))? {
        if !owner.file_name().to_str().is_some_and(digest) {
            continue;
        }
        for scope in directories(&owner.path())? {
            let relative = scope
                .path()
                .strip_prefix(machine.dir.join("runner/hubs"))?
                .to_string_lossy()
                .into_owned();
            let keep: HashSet<String> = state
                .workers
                .values()
                .filter(|w| w.scope == relative)
                .map(|w| w.build.sha256.clone())
                .collect();
            reclaimed += prune_slots(&scope.path().join("builds"), &keep, aggressive)?;
            if can_prune_native {
                let mut sources = HashSet::new();
                for cache in directories(&scope.path().join("native"))? {
                    if let Ok(bytes) =
                        crate::state::read_private(&cache.path().join("output.json"), 4096)
                    {
                        if let Ok(build) = serde_json::from_slice::<crate::worker::Build>(&bytes) {
                            if keep.contains(&build.sha256) {
                                sources.insert(cache.file_name().to_string_lossy().into_owned());
                            }
                        }
                    }
                }
                reclaimed += prune_slots(&scope.path().join("native"), &sources, aggressive)?;
            }
        }
    }
    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_pruning_keeps_current_rollback_and_young_slots() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let old = std::time::SystemTime::now() - Duration::from_secs(172800);
        for key in ["a", "b", "c"] {
            let path = dir.path().join(key.repeat(64));
            std::fs::create_dir(&path)?;
            std::fs::write(path.join("twr"), b"binary")?;
            std::fs::File::open(&path)?.set_times(std::fs::FileTimes::new().set_modified(old))?;
        }
        let young = dir.path().join("d".repeat(64));
        std::fs::create_dir(&young)?;
        let keep = HashSet::from(["b".repeat(64)]);
        assert_eq!(prune_slots(dir.path(), &keep, false)?, 12);
        assert!(young.exists());
        assert!(dir.path().join("b".repeat(64)).exists());
        assert!(!dir.path().join("a".repeat(64)).exists());
        Ok(())
    }
}

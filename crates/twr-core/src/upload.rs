//! Bounded, resumable thread attachments. All filesystem access is anchored to open directory descriptors.
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
};
pub const MAX_CHUNK: usize = 64 * 1024;
pub const EXPIRY: std::time::Duration = std::time::Duration::from_secs(900);
pub const MIN_FILE_CHARGE: usize = 64 * 1024;
pub const MAX_FILE: usize = 25 * 1024 * 1024;
#[derive(Default)]
pub struct Quota {
    charges: std::collections::HashMap<String, (std::time::Instant, usize, bool)>,
}
impl Quota {
    pub fn reserve(&mut self, id: &str, size: usize, now: std::time::Instant) -> Result<()> {
        self.charges.retain(|_, (at, _, pending)| {
            *pending || now.duration_since(*at) < std::time::Duration::from_secs(600)
        });
        let size = size.max(MIN_FILE_CHARGE);
        let used: usize = self.charges.values().map(|(_, size, _)| size).sum();
        anyhow::ensure!(
            used.saturating_add(size) <= 128 * 1024 * 1024,
            "Upload limit reached (128 MiB per 10 minutes). Try again later."
        );
        self.charges.insert(id.into(), (now, size, true));
        Ok(())
    }
    pub fn commit(&mut self, id: &str, now: std::time::Instant) {
        if let Some(charge) = self.charges.get_mut(id) {
            charge.0 = now;
            charge.2 = false;
        }
    }
    pub fn release(&mut self, id: &str) {
        self.charges.remove(id);
    }
}
pub fn cancel(
    pending: &mut std::collections::HashMap<String, Upload>,
    quota: &mut Quota,
    id: &str,
) {
    if pending.remove(id).is_some() {
        quota.release(id);
    }
}
pub struct Upload {
    base: File,
    directory: String,
    pub created: std::time::Instant,
    root: File,
    root_path: PathBuf,
    file: File,
    dir: File,
    name: String,
    pub path: PathBuf,
    size: usize,
    written: usize,
    hash: Sha256,
    expected: String,
    committed: bool,
}
fn open_root(path: &Path) -> Result<File> {
    use std::path::Component;
    anyhow::ensure!(
        path.is_absolute(),
        "Upload working directory must be absolute"
    );
    let mut directory = File::open("/")?;
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                use std::os::unix::ffi::OsStrExt;
                let name = CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                anyhow::ensure!(
                    fd >= 0,
                    "Unsafe upload working directory: {}",
                    std::io::Error::last_os_error()
                );
                directory = unsafe { File::from_raw_fd(fd) };
            }
            _ => anyhow::bail!("Upload working directory must not contain parent components"),
        }
    }
    Ok(directory)
}
fn dir_at(parent: &File, name: &str) -> Result<File> {
    let name = CString::new(name)?;
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if created < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(e.into());
        }
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    anyhow::ensure!(
        fd >= 0,
        "Unsafe upload directory: {}",
        std::io::Error::last_os_error()
    );
    Ok(unsafe { File::from_raw_fd(fd) })
}
impl Upload {
    pub fn begin(root: &Path, name: &str, size: usize, expected: &str) -> Result<Self> {
        anyhow::ensure!(size <= MAX_FILE, "Files can be at most 25 MiB");
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 120
                && name != "."
                && name != ".."
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_. -".contains(c)),
            "Use a filename without directories or control characters"
        );
        anyhow::ensure!(
            expected.len() == 64 && expected.bytes().all(|b| b.is_ascii_hexdigit()),
            "File checksum required"
        );
        let root_file = open_root(root).context("Opening thread working directory")?;
        let base = dir_at(&root_file, ".twr-uploads")?;
        let ignore = CString::new(".gitignore")?;
        let ignore_fd = unsafe {
            libc::openat(
                base.as_raw_fd(),
                ignore.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if ignore_fd >= 0 {
            let mut ignore_file = unsafe { File::from_raw_fd(ignore_fd) };
            ignore_file.write_all(b"*\n")?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let dir = dir_at(&base, &id)?;
        let part_name = CString::new(format!("{name}.part"))?;
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                part_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            let directory = CString::new(id.as_str())?;
            unsafe { libc::unlinkat(base.as_raw_fd(), directory.as_ptr(), libc::AT_REMOVEDIR) };
            anyhow::bail!(
                "Could not create staged upload: {}",
                std::io::Error::last_os_error()
            );
        }
        let path = root.join(".twr-uploads").join(&id).join(name);
        Ok(Self {
            root: root_file,
            root_path: root.to_path_buf(),
            base,
            directory: id,
            created: std::time::Instant::now(),
            file: unsafe { File::from_raw_fd(fd) },
            dir,
            name: name.to_owned(),
            path,
            size,
            written: 0,
            hash: Sha256::new(),
            expected: expected.to_lowercase(),
            committed: false,
        })
    }
    pub fn chunk(&mut self, data: &[u8]) -> Result<usize> {
        anyhow::ensure!(
            data.len() <= MAX_CHUNK && self.written + data.len() <= self.size,
            "Upload exceeds its declared size"
        );
        self.file.seek(SeekFrom::Start(self.written as u64))?;
        self.file.write_all(data)?;
        self.hash.update(data);
        self.written += data.len();
        Ok(self.written)
    }
    pub fn chunk_at(&mut self, offset: usize, data: &[u8]) -> Result<usize> {
        anyhow::ensure!(data.len() <= MAX_CHUNK, "Upload chunk is too large");
        if offset < self.written {
            anyhow::ensure!(
                offset
                    .checked_add(data.len())
                    .is_some_and(|end| end <= self.written),
                "Upload offset changed; check status"
            );
            let mut previous = vec![0; data.len()];
            self.file.seek(SeekFrom::Start(offset as u64))?;
            self.file.read_exact(&mut previous)?;
            anyhow::ensure!(
                previous == data,
                "Repeated upload chunk differs from received bytes"
            );
            return Ok(self.written);
        }
        anyhow::ensure!(
            offset == self.written,
            "Upload offset changed; check status"
        );
        self.chunk(data)
    }
    pub fn offset(&self) -> usize {
        self.written
    }
    pub fn prefix_sha256(&self) -> String {
        hex::encode(self.hash.clone().finalize())
    }
    fn verify_destination(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let same = |a: &File, b: &File| -> Result<bool> {
            let (a, b) = (a.metadata()?, b.metadata()?);
            Ok(a.dev() == b.dev() && a.ino() == b.ino())
        };
        let root = open_root(&self.root_path)?;
        anyhow::ensure!(
            same(&self.root, &root)?,
            "Thread working directory changed; upload again"
        );
        let open = |parent: &File, name: &str, directory: bool| -> Result<File> {
            let name = CString::new(name)?;
            let fd = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY
                        | libc::O_NONBLOCK
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC
                        | if directory { libc::O_DIRECTORY } else { 0 },
                )
            };
            anyhow::ensure!(fd >= 0, "Upload destination changed");
            Ok(unsafe { File::from_raw_fd(fd) })
        };
        let base = open(&root, ".twr-uploads", true)?;
        let directory = open(&base, &self.directory, true)?;
        anyhow::ensure!(
            same(&self.base, &base)? && same(&self.dir, &directory)?,
            "Upload destination changed"
        );
        Ok(())
    }
    fn verify_file(&self, name: &str) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let name = CString::new(name)?;
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        anyhow::ensure!(fd >= 0, "Staged upload was replaced");
        let named = unsafe { File::from_raw_fd(fd) }.metadata()?;
        let original = self.file.metadata()?;
        anyhow::ensure!(
            named.is_file()
                && named.dev() == original.dev()
                && named.ino() == original.ino()
                && named.len() == self.size as u64,
            "Staged upload was replaced"
        );
        Ok(())
    }
    pub fn commit(mut self) -> Result<PathBuf> {
        self.verify_destination()?;
        anyhow::ensure!(self.written == self.size, "Upload is incomplete");
        anyhow::ensure!(
            hex::encode(self.hash.clone().finalize()) == self.expected,
            "Upload checksum mismatch"
        );
        self.verify_file(&format!("{}.part", self.name))?;
        // Verify the bytes on disk too, rather than trusting only the transport's digest.
        self.file.seek(SeekFrom::Start(0))?;
        let mut disk_hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let count = self.file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            disk_hash.update(&buffer[..count]);
        }
        anyhow::ensure!(
            hex::encode(disk_hash.finalize()) == self.expected,
            "Upload checksum mismatch"
        );
        self.file.sync_all()?;
        let part = CString::new(format!("{}.part", self.name))?;
        let name = CString::new(self.name.as_str())?;
        let result = unsafe {
            libc::renameat(
                self.dir.as_raw_fd(),
                part.as_ptr(),
                self.dir.as_raw_fd(),
                name.as_ptr(),
            )
        };
        anyhow::ensure!(
            result == 0,
            "Could not commit upload: {}",
            std::io::Error::last_os_error()
        );
        if let Err(error) = self
            .verify_destination()
            .and_then(|_| self.verify_file(&self.name))
        {
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) };
            return Err(error);
        }
        let _ = self.dir.sync_all();
        self.committed = true;
        Ok(self.path.clone())
    }
}
impl Drop for Upload {
    fn drop(&mut self) {
        if !self.committed {
            if let Ok(name) = CString::new(format!("{}.part", self.name)) {
                unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) };
            }
            if let Ok(name) = CString::new(self.directory.as_str()) {
                unsafe { libc::unlinkat(self.base.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            }
        }
    }
}
/// Requests travel only over an authenticated hub/runner connection. The hub supplies the
/// thread root and the authenticated caller's scope; a client cannot select a filesystem path.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub id: String,
    pub sha256: String,
    #[serde(flatten)]
    pub action: Action,
}
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Begin { name: String, size: usize },
    Status,
    Chunk { offset: usize, data: String },
    Commit,
    Cancel,
}
#[derive(Debug)]
pub struct Rejected(pub anyhow::Error);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Rejected {}
#[derive(Debug)]
pub struct Failed(pub anyhow::Error);
impl std::fmt::Display for Failed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::error::Error for Failed {}
pub fn reject_begin(request: &Request, error: anyhow::Error) -> anyhow::Error {
    if matches!(request.action, Action::Begin { .. }) {
        anyhow::Error::new(Rejected(error))
    } else {
        error
    }
}
pub fn resolve_root(root: &Path, registered_repository: bool) -> Result<PathBuf> {
    if registered_repository {
        std::fs::canonicalize(root).context("Resolving registered repository directory")
    } else {
        Ok(root.to_path_buf())
    }
}
type Key = (String, String);
struct Entry {
    thread: String,
    root: PathBuf,
    sha256: String,
    name: String,
    size: usize,
    touched: std::time::Instant,
    upload: Option<Upload>,
    path: Option<PathBuf>,
}
enum Prepared {
    Reply(serde_json::Value),
    Discard {
        entry: Box<Entry>,
        reply: serde_json::Value,
    },
    Begin {
        key: Key,
        root: PathBuf,
        name: String,
        size: usize,
        sha256: String,
    },
    Chunk {
        key: Key,
        upload: Box<Upload>,
        offset: usize,
        data: Vec<u8>,
    },
    Commit {
        key: Key,
        upload: Box<Upload>,
    },
}
/// Owns staged files and quotas for one box. Unfinished transfers expire after 15 minutes.
#[derive(Default)]
pub struct Manager {
    entries: std::collections::HashMap<Key, Entry>,
    quotas: std::collections::HashMap<String, Quota>,
    cleanup_started: bool,
}
impl Manager {
    pub fn shared() -> std::sync::Arc<std::sync::Mutex<Self>> {
        std::sync::Arc::default()
    }
    /// Start lazily when the host first uploads, including hosts constructed before Tokio.
    pub fn start_cleanup(manager: &std::sync::Arc<std::sync::Mutex<Self>>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let mut manager = manager.lock().unwrap();
            if manager.cleanup_started {
                return;
            }
            manager.cleanup_started = true;
        }
        let weak = std::sync::Arc::downgrade(manager);
        runtime.spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
            loop {
                tick.tick().await;
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                let expired = manager
                    .lock()
                    .unwrap()
                    .take_expired(std::time::Instant::now());
                if !expired.is_empty() {
                    tokio::task::spawn_blocking(move || drop(expired));
                }
            }
        });
    }
    fn take_expired(&mut self, now: std::time::Instant) -> Vec<Entry> {
        let expired = self
            .entries
            .iter()
            .filter(|(_, e)| {
                !(e.upload.is_none() && e.path.is_none()) && now.duration_since(e.touched) >= EXPIRY
            })
            .map(|(key, _)| key.clone())
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|key| {
                let entry = self.entries.remove(&key)?;
                if entry.path.is_none() {
                    if let Some(quota) = self.quotas.get_mut(&key.0) {
                        quota.release(&key.1);
                    }
                }
                Some(entry)
            })
            .collect()
    }
    pub fn expire(&mut self, now: std::time::Instant) {
        drop(self.take_expired(now));
    }
    fn summary(&self, key: &Key) -> serde_json::Value {
        let e = &self.entries[key];
        let busy = e.upload.is_none() && e.path.is_none();
        serde_json::json!({"id":key.1,"received":e.upload.as_ref().map(Upload::offset).unwrap_or(e.size),"prefix_sha256":e.upload.as_ref().map(Upload::prefix_sha256).unwrap_or_else(||e.sha256.clone()),"sha256":e.sha256,"path":e.path,"busy":busy,"committing":busy,"chunk_size":MAX_CHUNK,"expires_in":EXPIRY.as_secs()})
    }
    fn prepare(
        &mut self,
        scope: &str,
        thread: &str,
        root: &Path,
        req: Request,
    ) -> Result<Prepared> {
        use serde_json::json;
        let now = std::time::Instant::now();
        anyhow::ensure!(
            req.id.len() <= 64
                && !req.id.is_empty()
                && req
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid upload ID"
        );
        let key = (scope.to_owned(), req.id.clone());
        if let Some(e) = self.entries.get(&key) {
            anyhow::ensure!(
                e.thread == thread && e.root == root && e.sha256 == req.sha256,
                "Upload destination or checksum changed"
            );
        } else if let Action::Begin { name, size } = &req.action {
            anyhow::ensure!(
                self.entries.len() < 1024
                    && self
                        .entries
                        .keys()
                        .filter(|(owner, _)| owner == scope)
                        .count()
                        < 128,
                "Upload history capacity reached; try again later"
            );
            anyhow::ensure!(
                self.entries
                    .iter()
                    .filter(|((owner, _), e)| owner == scope && e.path.is_none())
                    .count()
                    < 4,
                "Finish an existing upload first"
            );
            anyhow::ensure!(
                self.entries.values().filter(|e| e.path.is_none()).count() < 32,
                "Upload capacity reached; try again later"
            );
            self.quotas
                .entry(scope.into())
                .or_default()
                .reserve(&req.id, *size, now)?;
            self.entries.insert(
                key.clone(),
                Entry {
                    thread: thread.into(),
                    root: root.into(),
                    sha256: req.sha256.clone(),
                    name: name.clone(),
                    size: *size,
                    touched: now,
                    upload: None,
                    path: None,
                },
            );
            return Ok(Prepared::Begin {
                key,
                root: root.into(),
                name: name.clone(),
                size: *size,
                sha256: req.sha256,
            });
        } else if matches!(req.action, Action::Cancel) {
            return Ok(Prepared::Reply(json!({"cancelled":true,"committed":false})));
        } else {
            anyhow::bail!("Upload expired or was not found; start again");
        }
        let entry = self.entries.get_mut(&key).unwrap();
        entry.touched = now;
        match req.action {
            Action::Begin { name, size } => anyhow::ensure!(
                name == entry.name && size == entry.size,
                "Upload metadata changed"
            ),
            Action::Status => {}
            Action::Chunk { offset, data } => {
                anyhow::ensure!(
                    data.len() <= MAX_CHUNK.div_ceil(3) * 4,
                    "Upload chunk is too large"
                );
                let data = crate::b64::decode(&data)?;
                let upload = entry.upload.take().context(
                    "Upload operation is in progress or already committed; check status",
                )?;
                return Ok(Prepared::Chunk {
                    key,
                    upload: Box::new(upload),
                    offset,
                    data,
                });
            }
            Action::Commit => {
                if let Some(upload) = entry.upload.take() {
                    return Ok(Prepared::Commit {
                        key,
                        upload: Box::new(upload),
                    });
                }
            }
            Action::Cancel => {
                if let Some(path) = &entry.path {
                    return Ok(Prepared::Reply(
                        json!({"cancelled":false,"committed":true,"path":path}),
                    ));
                }
                if entry.upload.is_none() {
                    return Ok(Prepared::Reply(
                        json!({"cancelled":false,"committed":false,"busy":true}),
                    ));
                }
                let entry = self.entries.remove(&key).unwrap();
                self.quotas.get_mut(scope).unwrap().release(&req.id);
                return Ok(Prepared::Discard {
                    entry: Box::new(entry),
                    reply: json!({"cancelled":true,"committed":false}),
                });
            }
        }
        Ok(Prepared::Reply(self.summary(&key)))
    }
    fn finish_commit(&mut self, key: Key, outcome: Result<PathBuf>) -> Result<serde_json::Value> {
        match outcome {
            Ok(path) => {
                let entry = self
                    .entries
                    .get_mut(&key)
                    .context("Upload commit receipt missing")?;
                entry.path = Some(path);
                entry.touched = std::time::Instant::now();
                self.quotas
                    .get_mut(&key.0)
                    .unwrap()
                    .commit(&key.1, entry.touched);
                Ok(self.summary(&key))
            }
            Err(error) => {
                self.entries.remove(&key);
                self.quotas.get_mut(&key.0).unwrap().release(&key.1);
                Err(anyhow::Error::new(Failed(error)))
            }
        }
    }
    fn finish_begin(&mut self, key: Key, outcome: Result<Upload>) -> Result<serde_json::Value> {
        match outcome {
            Ok(upload) => {
                self.entries
                    .get_mut(&key)
                    .context("Upload reservation missing")?
                    .upload = Some(upload);
                Ok(self.summary(&key))
            }
            Err(error) => {
                self.entries.remove(&key);
                self.quotas.get_mut(&key.0).unwrap().release(&key.1);
                Err(anyhow::Error::new(Rejected(error)))
            }
        }
    }
    fn finish_chunk(
        &mut self,
        key: Key,
        upload: Upload,
        outcome: Result<usize>,
    ) -> Result<serde_json::Value> {
        self.entries
            .get_mut(&key)
            .context("Upload reservation missing")?
            .upload = Some(upload);
        outcome?;
        Ok(self.summary(&key))
    }
    /// All filesystem work runs outside the global manager lock, including cleanup drops.
    pub fn request_shared(
        manager: &std::sync::Arc<std::sync::Mutex<Self>>,
        scope: &str,
        thread: &str,
        root: &Path,
        req: Request,
    ) -> Result<serde_json::Value> {
        let expired = manager
            .lock()
            .unwrap()
            .take_expired(std::time::Instant::now());
        drop(expired);
        let begin = matches!(req.action, Action::Begin { .. });
        let prepared = manager
            .lock()
            .unwrap()
            .prepare(scope, thread, root, req)
            .map_err(|error| {
                if begin {
                    anyhow::Error::new(Rejected(error))
                } else {
                    error
                }
            })?;
        match prepared {
            Prepared::Reply(value) => Ok(value),
            Prepared::Discard { entry, reply } => {
                drop(entry);
                Ok(reply)
            }
            Prepared::Begin {
                key,
                root,
                name,
                size,
                sha256,
            } => {
                let outcome = Upload::begin(&root, &name, size, &sha256);
                manager.lock().unwrap().finish_begin(key, outcome)
            }
            Prepared::Chunk {
                key,
                mut upload,
                offset,
                data,
            } => {
                let outcome = upload.chunk_at(offset, &data);
                manager.lock().unwrap().finish_chunk(key, *upload, outcome)
            }
            Prepared::Commit { key, upload } => {
                let outcome = upload.commit();
                manager.lock().unwrap().finish_commit(key, outcome)
            }
        }
    }
    pub fn request(
        &mut self,
        scope: &str,
        thread: &str,
        root: &Path,
        req: Request,
    ) -> Result<serde_json::Value> {
        self.expire(std::time::Instant::now());
        match self.prepare(scope, thread, root, req)? {
            Prepared::Reply(value) => Ok(value),
            Prepared::Discard { entry, reply } => {
                drop(entry);
                Ok(reply)
            }
            Prepared::Begin {
                key,
                root,
                name,
                size,
                sha256,
            } => self.finish_begin(key, Upload::begin(&root, &name, size, &sha256)),
            Prepared::Chunk {
                key,
                mut upload,
                offset,
                data,
            } => {
                let outcome = upload.chunk_at(offset, &data);
                self.finish_chunk(key, *upload, outcome)
            }
            Prepared::Commit { key, upload } => self.finish_commit(key, upload.commit()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
    }
    fn req(action: Action) -> Request {
        Request {
            id: "resume-test".into(),
            sha256: hex::encode(Sha256::digest(b"hello")),
            action,
        }
    }
    #[test]
    fn different_owners_can_use_the_same_id_without_disclosing_or_cancelling_each_other() {
        let root = tempdir();
        let mut manager = Manager::default();
        let begin = req(Action::Begin {
            name: "file.txt".into(),
            size: 5,
        });
        manager
            .request("alice", "thread", root.path(), begin.clone())
            .unwrap();
        manager
            .request("bob", "thread", root.path(), begin)
            .unwrap();
        manager
            .request("bob", "thread", root.path(), req(Action::Cancel))
            .unwrap();
        assert_eq!(
            manager
                .request("alice", "thread", root.path(), req(Action::Status))
                .unwrap()["received"],
            0
        );
        assert_eq!(
            manager
                .request("third", "thread", root.path(), req(Action::Cancel))
                .unwrap(),
            serde_json::json!({"cancelled":true,"committed":false})
        );
    }
    #[test]
    fn even_empty_files_consume_the_minimum_quota() {
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        quota
            .reserve("large", 128 * 1024 * 1024 - MIN_FILE_CHARGE, now)
            .unwrap();
        quota.reserve("empty", 0, now).unwrap();
        assert!(quota.reserve("another-empty", 0, now).is_err());
    }
    #[test]
    fn duplicate_chunks_are_verified_and_failed_commits_remove_the_reservation() {
        let root = tempdir();
        let shared = Manager::shared();
        Manager::request_shared(
            &shared,
            "owner",
            "thread",
            root.path(),
            req(Action::Begin {
                name: "file.txt".into(),
                size: 5,
            }),
        )
        .unwrap();
        let chunk = req(Action::Chunk {
            offset: 0,
            data: crate::b64::encode(b"wrong"),
        });
        let first = Manager::request_shared(&shared, "owner", "thread", root.path(), chunk.clone())
            .unwrap();
        let repeated =
            Manager::request_shared(&shared, "owner", "thread", root.path(), chunk).unwrap();
        assert_eq!(first, repeated);
        let error =
            Manager::request_shared(&shared, "owner", "thread", root.path(), req(Action::Commit))
                .unwrap_err();
        assert!(error.downcast_ref::<Failed>().is_some());
        assert!(shared.lock().unwrap().entries.is_empty());
        assert!(shared.lock().unwrap().quotas["owner"].charges.is_empty());
    }
    #[test]
    fn pending_reservations_do_not_age_out_of_the_quota() {
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        quota.reserve("pending", 128 * 1024 * 1024, now).unwrap();
        assert!(quota
            .reserve("over", 1, now + std::time::Duration::from_secs(601))
            .is_err());
        quota.commit("pending", now + std::time::Duration::from_secs(601));
        quota
            .reserve("later", 1, now + std::time::Duration::from_secs(1202))
            .unwrap();
    }
    #[test]
    fn detached_commit_keeps_busy_receipt_and_charge_while_other_uploads_progress() {
        let root = tempdir();
        let shared = Manager::shared();
        Manager::request_shared(
            &shared,
            "owner",
            "thread",
            root.path(),
            req(Action::Begin {
                name: "file.txt".into(),
                size: 5,
            }),
        )
        .unwrap();
        Manager::request_shared(
            &shared,
            "owner",
            "thread",
            root.path(),
            req(Action::Chunk {
                offset: 0,
                data: crate::b64::encode(b"hello"),
            }),
        )
        .unwrap();
        let Prepared::Commit { key, upload } = shared
            .lock()
            .unwrap()
            .prepare("owner", "thread", root.path(), req(Action::Commit))
            .unwrap()
        else {
            panic!("expected detached commit")
        };
        assert_eq!(
            Manager::request_shared(&shared, "owner", "thread", root.path(), req(Action::Status))
                .unwrap()["busy"],
            true
        );
        assert_eq!(
            Manager::request_shared(&shared, "owner", "thread", root.path(), req(Action::Cancel))
                .unwrap()["cancelled"],
            false
        );
        shared
            .lock()
            .unwrap()
            .expire(std::time::Instant::now() + EXPIRY);
        assert_eq!(shared.lock().unwrap().entries.len(), 1);
        let mut other = req(Action::Begin {
            name: "second.txt".into(),
            size: 5,
        });
        other.id = "other".into();
        Manager::request_shared(&shared, "owner", "thread", root.path(), other).unwrap();
        let outcome = upload.commit();
        let committed = shared.lock().unwrap().finish_commit(key, outcome).unwrap();
        let cancelled =
            Manager::request_shared(&shared, "owner", "thread", root.path(), req(Action::Cancel))
                .unwrap();
        assert_eq!(cancelled["committed"], true);
        assert_eq!(cancelled["cancelled"], false);
        assert_eq!(cancelled["path"], committed["path"]);
    }
    #[tokio::test]
    async fn cleanup_starts_for_a_manager_constructed_outside_tokio() {
        let shared = std::thread::spawn(Manager::shared).join().unwrap();
        assert!(!shared.lock().unwrap().cleanup_started);
        Manager::start_cleanup(&shared);
        assert!(shared.lock().unwrap().cleanup_started);
    }
    #[test]
    fn registered_repository_aliases_resolve_but_worktree_aliases_stay_strict() {
        let root = tempdir();
        let outside = tempdir();
        let alias = root.path().join("repo");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        let canonical = resolve_root(&alias, true).unwrap();
        assert_eq!(canonical, outside.path());
        Upload::begin(&canonical, "x", 0, &hex::encode(Sha256::digest(b""))).unwrap();
        assert!(Upload::begin(
            &resolve_root(&alias, false).unwrap(),
            "x",
            0,
            &hex::encode(Sha256::digest(b""))
        )
        .is_err());
    }
    #[test]
    fn resume_verifies_scope_thread_root_hash_and_offset() {
        let root = tempdir();
        let other = tempdir();
        let mut manager = Manager::default();
        let begin = req(Action::Begin {
            name: "resume.txt".into(),
            size: 5,
        });
        manager
            .request("device-a", "thread-a", root.path(), begin.clone())
            .unwrap();
        assert!(manager
            .request("device-b", "thread-a", root.path(), req(Action::Status))
            .is_err());
        assert!(manager
            .request("device-a", "thread-b", root.path(), req(Action::Status))
            .is_err());
        assert!(manager
            .request("device-a", "thread-a", other.path(), req(Action::Status))
            .is_err());
        let mut wrong = req(Action::Status);
        wrong.sha256 = "a".repeat(64);
        assert!(manager
            .request("device-a", "thread-a", root.path(), wrong)
            .is_err());
        let status = manager
            .request(
                "device-a",
                "thread-a",
                root.path(),
                req(Action::Chunk {
                    offset: 0,
                    data: crate::b64::encode(b"he"),
                }),
            )
            .unwrap();
        assert_eq!(status["received"], 2);
        assert_eq!(status["prefix_sha256"], hex::encode(Sha256::digest(b"he")));
        assert!(manager
            .request(
                "device-a",
                "thread-a",
                root.path(),
                req(Action::Chunk {
                    offset: 0,
                    data: crate::b64::encode(b"xx")
                })
            )
            .is_err());
        manager
            .request(
                "device-a",
                "thread-a",
                root.path(),
                req(Action::Chunk {
                    offset: 2,
                    data: crate::b64::encode(b"llo"),
                }),
            )
            .unwrap();
        let committed = manager
            .request("device-a", "thread-a", root.path(), req(Action::Commit))
            .unwrap();
        assert_eq!(
            std::fs::read(committed["path"].as_str().unwrap()).unwrap(),
            b"hello"
        );
        assert_eq!(
            manager
                .request("device-a", "thread-a", root.path(), req(Action::Commit))
                .unwrap(),
            committed
        );
        manager
            .request("device-a", "thread-a", root.path(), req(Action::Cancel))
            .unwrap();
        assert!(Path::new(committed["path"].as_str().unwrap()).exists());
    }
    #[test]
    fn expiry_and_cancellation_remove_staging_and_refund_only_pending_quota() {
        let root = tempdir();
        let mut manager = Manager::default();
        manager
            .request(
                "device",
                "thread",
                root.path(),
                req(Action::Begin {
                    name: "x".into(),
                    size: 5,
                }),
            )
            .unwrap();
        let staged = manager.entries[&("device".into(), "resume-test".into())]
            .upload
            .as_ref()
            .unwrap()
            .path
            .clone();
        manager.expire(std::time::Instant::now() + EXPIRY);
        assert!(!staged.with_file_name("x.part").exists());
        assert!(manager.quotas["device"].charges.is_empty());
        manager
            .request(
                "device",
                "thread",
                root.path(),
                req(Action::Begin {
                    name: "x".into(),
                    size: 5,
                }),
            )
            .unwrap();
        manager
            .request("device", "thread", root.path(), req(Action::Cancel))
            .unwrap();
        assert!(manager.entries.is_empty());
        assert!(manager.quotas["device"].charges.is_empty());
    }
    #[test]
    fn symlink_worktree_and_parent_and_replaced_root_are_refused() {
        let root = tempdir();
        let outside = tempdir();
        let linked = root.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        assert!(Upload::begin(&linked, "x", 0, &hex::encode(Sha256::digest(b""))).is_err());
        let child = outside.path().join("child");
        std::fs::create_dir(&child).unwrap();
        assert!(Upload::begin(
            &linked.join("child"),
            "x",
            0,
            &hex::encode(Sha256::digest(b""))
        )
        .is_err());
        let working = root.path().join("working");
        std::fs::create_dir(&working).unwrap();
        let upload = Upload::begin(&working, "x", 0, &hex::encode(Sha256::digest(b""))).unwrap();
        std::fs::rename(&working, root.path().join("moved")).unwrap();
        std::fs::create_dir(&working).unwrap();
        assert!(upload.commit().is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 1);
    }
    #[test]
    fn replacing_staging_paths_or_changing_disk_bytes_prevents_commit() {
        let root = tempdir();
        let outside = tempdir();
        let hash = hex::encode(Sha256::digest(b"hello"));
        let mut file = Upload::begin(root.path(), "hello.txt", 5, &hash).unwrap();
        file.chunk(b"hello").unwrap();
        let staged = file.path.with_file_name("hello.txt.part");
        std::fs::remove_file(&staged).unwrap();
        std::os::unix::fs::symlink(outside.path().join("escape"), &staged).unwrap();
        assert!(file.commit().is_err());
        assert!(!outside.path().join("escape").exists());
        let mut file = Upload::begin(root.path(), "hello.txt", 5, &hash).unwrap();
        file.chunk(b"hello").unwrap();
        std::fs::write(file.path.with_file_name("hello.txt.part"), b"wrong").unwrap();
        assert!(file.commit().is_err());
        let mut file = Upload::begin(root.path(), "hello.txt", 5, &hash).unwrap();
        file.chunk(b"hello").unwrap();
        let directory = file.path.parent().unwrap();
        std::fs::rename(directory, directory.with_extension("moved")).unwrap();
        std::os::unix::fs::symlink(outside.path(), directory).unwrap();
        assert!(file.commit().is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
    #[test]
    fn cancelling_a_committed_file_does_not_refund_its_quota() {
        let root = tempdir();
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        let mut file = Upload::begin(
            root.path(),
            "committed.txt",
            5,
            &hex::encode(Sha256::digest(b"hello")),
        )
        .unwrap();
        file.chunk(b"hello").unwrap();
        let mut pending = std::collections::HashMap::from([("committed".into(), file)]);
        quota.reserve("committed", 5, now).unwrap();
        let path = pending.remove("committed").unwrap().commit().unwrap();
        cancel(&mut pending, &mut quota, "committed");
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
        quota
            .reserve("remaining", 128 * 1024 * 1024 - MIN_FILE_CHARGE, now)
            .unwrap();
        assert!(quota.reserve("overflow", 1, now).is_err());
    }
    #[test]
    fn cancelled_and_expired_uploads_do_not_exhaust_the_quota() {
        let mut quota = Quota::default();
        let now = std::time::Instant::now();
        for i in 0..5 {
            quota.reserve(&i.to_string(), MAX_FILE, now).unwrap();
        }
        assert!(quota.reserve("sixth", MAX_FILE, now).is_err());
        quota.release("0");
        quota.reserve("replacement", MAX_FILE, now).unwrap();
        assert!(quota.reserve("still-full", MAX_FILE, now).is_err());
        for i in ["1", "2", "3", "4", "replacement"] {
            quota.commit(i, now);
        }
        quota
            .reserve("later", MAX_FILE, now + std::time::Duration::from_secs(601))
            .unwrap();
    }
    #[test]
    fn complete_file_checks_size_hash_and_traversal() {
        let root = tempdir();
        let hash = hex::encode(Sha256::digest(b"hello"));
        assert!(Upload::begin(root.path(), "../escape", 5, &hash).is_err());
        let mut file = Upload::begin(root.path(), "hello.txt", 5, &hash).unwrap();
        assert!(file.chunk(b"oversize").is_err());
        file.chunk(b"hello").unwrap();
        let path = file.commit().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"hello");
    }
    #[test]
    fn symlink_destination_is_refused() {
        let root = tempdir();
        let outside = tempdir();
        std::os::unix::fs::symlink(outside.path(), root.path().join(".twr-uploads")).unwrap();
        assert!(Upload::begin(root.path(), "x", 0, &hex::encode(Sha256::digest(b""))).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }
}

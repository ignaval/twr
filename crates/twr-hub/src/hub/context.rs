//! Standing project context: `<repo>/.twr/context.md`, read and replaced with
//! `context.read` / `context.set`, and injected into every prompt the hub dispatches to a
//! worker thread of that project.
//!
//! - A project is a configured project name, or the project of a (non-orchestrator) thread
//!   (a thread made from a directory has the directory's basename as its project). Its
//!   context file is `<repo>/.twr/context.md`; a missing file is an empty context. `<repo>`
//!   is the repository its threads record (`thread.created.repo`): for a configured project,
//!   the git top level of its configured `repo` (so a project configured at a subdirectory of
//!   a repository shares the repository's context), or that directory itself outside git,
//!   resolved as thread creation resolves it, before any thread exists too. `context.read`,
//!   `context.set` and every dispatch go through one resolution (`Core::context_target`).
//! - `context.set` stores the new bytes as a blob and appends `context.updated{hash,
//!   ext.twr.{project, path, bytes, origin: "set"}}` on the `hub` thread first (preceded, in
//!   the same batch, by an `origin: "file"` capture of a manual edit not logged yet), then
//!   writes the file atomically and durably (random temp file + fsync + rename + directory
//!   fsync, keeping the file's mode; `.twr/` is created with the default mode), every step
//!   relative to the repository and `.twr/` directories held open, so a `.twr/` swapped for a
//!   symlink after the check is never written through ([`write_atomic`]). If the write
//!   fails after the event, the next dispatch notices the file does not match the logged hash
//!   and logs the file's actual content, so the log converges on the file. An `expect_hash`
//!   that is not the current context's hash is a `conflict`; a non-UTF-8 file, or one over
//!   [`SET_MAX`] bytes (whose hash covers only its beginning), is never written over.
//! - The file: a symlinked `.twr/` or `context.md` (or anything but a regular file) is refused
//!   everywhere: not read, not injected, not written (no following links out of the repo,
//!   into secrets or over other files). At most [`SET_MAX`] + 1 bytes are read; a larger file
//!   is logged and injected as its first [`SET_MAX`] bytes (`ext.twr.truncated`). The RPCs
//!   read it outside the core lock and re-validate it under the lock by inode, size and
//!   timestamps; a dispatch only `lstat`s a file it has cached (a file changed in the last
//!   2 s is never cached, so a same-size rewrite cannot hide).
//! - Injection: before a worker turn starts, the hub reads and hashes the file; when the hash
//!   differs from the last one logged for that file (a manual edit), it appends
//!   `context.updated{origin: "file"}` (with the bytes as a blob) first. A non-empty context is
//!   prepended to the prompt the runtime receives as
//!   `<twr-context project="X">\n...\n</twr-context>\n\n`. The orchestrator never gets it.
//! - What is logged: `turn.started.prompt` is the prompt as given (without the context);
//!   `turn.started.ext.twr.context_hash` names the context that was prepended (its bytes are
//!   the blob of that `context.updated`), so the exact text the runtime got can be rebuilt.
//! - A context over [`INJECT_MAX`] bytes is cut to that size (on a character boundary) with a
//!   note to the agent, a hub warning, and `ext.twr.context_truncated = true`.
//!
//! Hashes are blob pointers (`sha256:<hex>` of the file's bytes); the empty context's hash is
//! the "nothing logged yet" default, so a project that never had a context logs nothing.
//!
//! Remote boxes: the hub cannot read files on a runner's box, so a thread on a
//! remote box gets the project's context from the log: the bytes of the latest
//! `context.updated` for its project (whatever set it: `context.set`, or a manual edit of a
//! local repository's file captured at a local dispatch). No file is read for it and nothing
//! is logged at its dispatch. A project that lives only on remote boxes (it has threads and
//! they are all remote, configured or not, or `[projects.<name>] box` names a remote box)
//! keeps its context in the log alone:
//! `context.read` returns the logged bytes and `context.set` records them without writing a
//! file.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use twr_core::blob::{blob_ref, truncate_utf8, BlobStore};
use twr_core::config::ProjectConfig;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

use super::{
    append_err, internal, rpc, twr_ext, twr_ext_map, Batch, Caller, Core, Hub, Inner, LOCAL_BOX,
};
use crate::store;

/// Directory of the standing context inside a project's repository.
pub const CONTEXT_DIR: &str = ".twr";
/// File name of the standing context.
pub const CONTEXT_FILE: &str = "context.md";
/// Most bytes of standing context injected into a prompt.
pub const INJECT_MAX: usize = 64 * 1024;
/// Largest context `context.set` accepts, in bytes.
pub const SET_MAX: usize = 1024 * 1024;

/// A project's context file.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ContextTarget {
    pub(super) project: String,
    pub(super) path: PathBuf,
    /// The repository is on a remote box: the context lives in the log only.
    pub(super) remote: bool,
}

impl ContextTarget {
    fn new(project: &str, repo: &Path) -> ContextTarget {
        ContextTarget {
            project: project.to_string(),
            path: repo.join(CONTEXT_DIR).join(CONTEXT_FILE),
            remote: false,
        }
    }

    fn remote(mut self, remote: bool) -> ContextTarget {
        self.remote = remote;
        self
    }

    /// Key of the logged hashes: the file path.
    fn key(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }
}

/// What a worker turn gets prepended.
pub(super) struct Injected {
    /// The text to put before the prompt.
    pub(super) prefix: String,
    /// Hash of the whole context file.
    pub(super) hash: String,
    /// The context was cut to [`INJECT_MAX`].
    pub(super) truncated: bool,
}

/// Hash of an empty context (the default when nothing was logged).
fn empty_hash() -> String {
    blob_ref(b"")
}

/// `O_NOFOLLOW | O_NONBLOCK` (no `libc` dependency, so the values are spelled out per OS and
/// architecture: Linux's `O_NOFOLLOW` is `0o400000` in the generic ABI, x86 included, but
/// `0o100000` on ARM, where `0o400000` is `O_LARGEFILE`): a symlink swapped in after the
/// `symlink_metadata` check fails to open, and a FIFO does not block the hub. Where the values
/// are not spelled out, the `symlink_metadata` checks and the post-open `fstat` (the opened
/// file must be the one checked, [`read_file`]) stand alone.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
pub const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
pub const OPEN_FLAGS: i32 = 0o100000 | 0o4000;
#[cfg(target_os = "macos")]
pub const OPEN_FLAGS: i32 = 0x0100 | 0x0004;
#[cfg(not(any(
    target_os = "macos",
    all(
        target_os = "linux",
        any(
            target_arch = "x86",
            target_arch = "x86_64",
            target_arch = "riscv64",
            target_arch = "loongarch64",
            target_arch = "arm",
            target_arch = "aarch64"
        )
    )
)))]
pub const OPEN_FLAGS: i32 = 0;
/// `errno` of an `O_NOFOLLOW` open of a symlink.
#[cfg(target_os = "linux")]
const ELOOP: i32 = 40;
#[cfg(not(target_os = "linux"))]
const ELOOP: i32 = 62;

/// An error for a context file twr refuses to touch (a symlink, not a regular file, not UTF-8):
/// reported as `conflict` by the RPCs, and skipped with a warning at dispatch.
fn refused(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Maps a context file error to an RPC error: a refusal is a `conflict`, the rest `internal`.
fn file_err(path: &Path, e: std::io::Error) -> RpcError {
    if e.kind() == std::io::ErrorKind::InvalidData {
        rpc(codes::CONFLICT, e.to_string())
    } else {
        internal(format!("reading {}: {e}", path.display()))
    }
}

/// What identifies one version of a file without reading it (device, inode, size, mtime and
/// ctime to the nanosecond): a rewrite changes the ctime at least.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileKey {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl FileKey {
    fn of(m: &std::fs::Metadata) -> FileKey {
        use std::os::unix::fs::MetadataExt;
        FileKey {
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    /// Modified within the last [`RACY_SECS`] (or in the future): a same-size rewrite within
    /// the timestamp granularity could keep the key, so such a version is never cached (the
    /// "racy git" rule).
    fn racy(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        let newest = self.mtime.0.max(self.ctime.0);
        newest >= now.saturating_sub(RACY_SECS)
    }
}

/// See [`FileKey::racy`].
const RACY_SECS: i64 = 2;

/// A read of a context file (at most [`SET_MAX`] bytes of it).
#[derive(Debug, Clone, Default)]
pub(super) struct FileSnap {
    /// `None`: the file (or `.twr/`) does not exist (an empty context).
    key: Option<FileKey>,
    /// Its first [`SET_MAX`] bytes.
    bytes: Vec<u8>,
    /// `blob_ref(bytes)`.
    hash: String,
    /// The file is larger than [`SET_MAX`]: `bytes` is its beginning.
    truncated: bool,
    /// The file's size.
    size: u64,
    /// The file's permission bits (kept when `context.set` replaces it).
    mode: Option<u32>,
}

impl FileSnap {
    fn empty() -> FileSnap {
        FileSnap {
            hash: empty_hash(),
            ..FileSnap::default()
        }
    }

    /// `hash` identifies the whole file: it was not cut by the cap and its size is the
    /// number of bytes read (a file that grew or shrank while it was read is not whole).
    fn whole(&self) -> bool {
        !self.truncated && self.size == self.bytes.len() as u64
    }

    /// The bytes are UTF-8 (a character cut by the size cap does not count against it).
    fn utf8(&self) -> bool {
        match std::str::from_utf8(&self.bytes) {
            Ok(_) => true,
            Err(e) => self.truncated && e.error_len().is_none(),
        }
    }
}

/// Parsed file versions per context file, so a dispatch only `lstat`s an unchanged file
/// (`Core::context_file`).
#[derive(Default)]
pub(super) struct FileCache(std::collections::HashMap<PathBuf, FileSnap>);

/// `lstat` of the context file: `Ok(None)` when it (or `.twr/`) does not exist. Refuses a
/// symlinked `.twr/` or `context.md`, and anything but a directory / regular file.
fn context_key(path: &Path) -> std::io::Result<Option<FileKey>> {
    let dir = path.parent().unwrap_or(Path::new("."));
    match std::fs::symlink_metadata(dir) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(refused(format!(
                "{} is a symbolic link; twr does not follow symlinked context directories \
                 (make it a real directory)",
                dir.display()
            )))
        }
        Ok(m) if !m.is_dir() => {
            return Err(refused(format!("{} is not a directory", dir.display())))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    }
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => Err(refused(format!(
            "{} is a symbolic link; twr does not follow symlinked context files \
             (make it a regular file)",
            path.display()
        ))),
        Ok(m) if !m.is_file() => Err(refused(format!("{} is not a regular file", path.display()))),
        Ok(m) => Ok(Some(FileKey::of(&m))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Reads the context file: a missing file is empty; at most [`SET_MAX`] + 1 bytes are read
/// (more marks the read `truncated`). Symlinks and non-regular files are refused
/// ([`context_key`], then `O_NOFOLLOW` and an `fstat` of what was opened, which must be the
/// file checked: the same device and inode, so a `.twr/` or `context.md` swapped between the
/// check and the open is refused too).
fn read_file(path: &Path) -> std::io::Result<FileSnap> {
    use std::io::Read;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let Some(checked) = context_key(path)? else {
        return Ok(FileSnap::empty());
    };
    #[cfg(test)]
    tests::before_open(path);
    let f = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(FileSnap::empty()),
        // ELOOP: replaced by a symlink after the check.
        Err(e) if e.raw_os_error() == Some(ELOOP) => {
            return Err(refused(format!(
                "{} is a symbolic link; twr does not follow symlinked context files",
                path.display()
            )))
        }
        Err(e) => return Err(e),
    };
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(refused(format!("{} is not a regular file", path.display())));
    }
    let opened = FileKey::of(&meta);
    if (opened.dev, opened.ino) != (checked.dev, checked.ino) {
        return Err(refused(format!(
            "{} changed while it was opened (another file, or a symlinked directory, took its \
             place); try again",
            path.display()
        )));
    }
    let cap = SET_MAX as u64 + 1;
    let mut bytes = Vec::with_capacity(meta.len().min(cap) as usize);
    f.take(cap).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > SET_MAX;
    bytes.truncate(SET_MAX);
    Ok(FileSnap {
        key: Some(FileKey::of(&meta)),
        hash: blob_ref(&bytes),
        size: meta.len().max(bytes.len() as u64),
        truncated,
        bytes,
        mode: Some(meta.permissions().mode() & 0o777),
    })
}

/// Writes `bytes` to `path` atomically and durably: a new temp file (random name,
/// `create_new`, `O_NOFOLLOW`) in the same directory, written with `mode` (the replaced file's
/// permissions; default: `0644` less the umask) and fsynced, renamed over `path`, then the
/// directory fsynced. Creates `.twr/` if needed; refuses a symlinked `.twr/` or `context.md`.
///
/// Every step is relative to directories held open (`twr_runner::local::dirfd`): the
/// repository, opened once by its path, and its `.twr/`, opened from it without following a
/// link. So a `.twr/` swapped for a link once checked changes nothing: the file is written,
/// and renamed, in the directory checked, never in the one the link names. Before the rename
/// the repository's `.twr/` must still be that directory; one swapped meanwhile is a refusal
/// and nothing is renamed (the temp file goes).
fn write_atomic(path: &Path, bytes: &[u8], mode: Option<u32>) -> std::io::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use twr_runner::local::dirfd::{not_a_dir, Dir};
    let dir = path.parent().unwrap_or(Path::new("."));
    let (Some(repo), Some(twr), Some(file)) = (dir.parent(), dir.file_name(), path.file_name())
    else {
        return Err(refused(format!(
            "{} is not a context file's path",
            path.display()
        )));
    };
    std::fs::create_dir_all(repo)?;
    // The repository as its path leads now (a link on the way followed once, here).
    let repo_dir = Dir::open(&std::fs::canonicalize(repo)?)?;
    let symlinked = || {
        refused(format!(
            "{} is a symbolic link (or not a directory); twr does not follow symlinked \
             context directories (make it a real directory)",
            dir.display()
        ))
    };
    let held = match repo_dir.dir(twr) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match repo_dir.make_dir(twr, 0o777) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
            repo_dir
                .dir(twr)
                .map_err(|e| if not_a_dir(&e) { symlinked() } else { e })?
        }
        Err(e) if not_a_dir(&e) => return Err(symlinked()),
        Err(e) => return Err(e),
    };
    #[cfg(test)]
    tests::before_write(path);
    // `context.md` must not be a symlink (the rename would replace the link, not follow it,
    // but it is refused like on reads), nor anything but a regular file.
    match held.open_entry(file) {
        Ok(f) if !f.metadata()?.is_file() => {
            return Err(refused(format!("{} is not a regular file", path.display())))
        }
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if not_a_dir(&e) => {
            return Err(refused(format!(
                "{} is a symbolic link; twr does not follow symlinked context files \
                 (make it a regular file)",
                path.display()
            )))
        }
        Err(e) => return Err(e),
    }
    let mut tries = 0;
    let (mut f, tmp) = loop {
        let tmp = format!(".{CONTEXT_FILE}.{}.tmp", ulid::Ulid::generate());
        match held.create_new(OsStr::new(&tmp), 0o644) {
            Ok(f) => break (f, tmp),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    let tmp = OsStr::new(&tmp);
    let written = (|| {
        if let Some(mode) = mode {
            f.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        // Still the repository's `.twr/`: the context lands where it is read from.
        let now = repo_dir.dir(twr).and_then(|d| d.file().metadata());
        let was = held.file().metadata()?;
        if !now.is_ok_and(|m| (m.dev(), m.ino()) == (was.dev(), was.ino())) {
            return Err(refused(format!(
                "{} changed while the context was written (another directory, or a symbolic \
                 link, took its place); try again",
                dir.display()
            )));
        }
        held.rename(tmp, file)?;
        held.file().sync_all()
    })();
    if written.is_err() {
        let _ = held.unlink(tmp);
    }
    written
}

/// The text prepended to a prompt for `content` (already cut to size).
pub fn injection(project: &str, content: &str, note: Option<&str>) -> String {
    let project = project.replace(['"', '<', '>', '\n'], "_");
    let mut body = content.trim_end_matches('\n').to_string();
    if let Some(n) = note {
        body.push_str("\n\n");
        body.push_str(n);
    }
    format!("<twr-context project=\"{project}\">\n{body}\n</twr-context>\n\n")
}

impl Core {
    /// The context file of a worker thread (`None` for the orchestrator, the hub thread, or a
    /// thread without a project or repository): [`Core::context_target`] for the thread.
    fn thread_context(&self, hub: &Inner, view: &ThreadView) -> Option<ContextTarget> {
        if view.is_orchestrator() || view.id == HUB_THREAD {
            return None;
        }
        let project = view.project.as_deref()?;
        view.repo.as_ref()?;
        self.context_target(hub, project, Some(view)).ok()
    }

    /// Where the standing context of `project` is: the one resolution `context.read`,
    /// `context.set` and every dispatch (`thread`: the worker thread a turn starts on) use, so
    /// the context a user sets is the one workers get.
    ///
    /// - A configured project: `<root>/.twr/context.md`, where `root` is the repository its
    ///   threads record (`thread.created.repo`): the git top level of its `repo`, or `repo`
    ///   itself outside git ([`Core::project_root`]). Log-only when its `box` is
    ///   remote, or when it names no remote box but all its threads run on remote boxes anyway
    ///   (`--box`).
    /// - Another project: the repository of its threads (they must agree: `ambiguous`
    ///   otherwise), or, for a dispatch, the thread's own.
    ///
    /// A dispatch to a thread on a remote box always takes the context from the log.
    pub(super) fn context_target(
        &self,
        hub: &Inner,
        project: &str,
        thread: Option<&ThreadView>,
    ) -> Result<ContextTarget, RpcError> {
        if project.trim().is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "project is required"));
        }
        let threads = self.project_threads(project);
        let all_remote = all_remote(&threads);
        let thread_remote = thread.map(|v| v.box_name != LOCAL_BOX);
        if let Some(p) = hub.config.projects.get(project) {
            let remote = thread_remote.unwrap_or_else(|| configured_remote(p, &threads));
            let root = self.project_root(project, &p.repo, &threads);
            return Ok(ContextTarget::new(project, &root).remote(remote));
        }
        if let Some(v) = thread {
            let repo = v.repo.as_deref().ok_or_else(|| {
                rpc(
                    codes::NOT_FOUND,
                    format!("thread {} has no repository", v.id),
                )
            })?;
            return Ok(ContextTarget::new(project, Path::new(repo)).remote(v.box_name != LOCAL_BOX));
        }
        let remote = all_remote;
        let repos: BTreeSet<&str> = threads.iter().filter_map(|v| v.repo.as_deref()).collect();
        match repos.len() {
            0 => Err(rpc(
                codes::NOT_FOUND,
                format!("no configured project and no thread project named {project:?}"),
            )),
            1 => {
                let repo = repos.into_iter().next().unwrap_or_default();
                Ok(ContextTarget::new(project, Path::new(repo)).remote(remote))
            }
            _ => Err(rpc(
                codes::AMBIGUOUS,
                format!(
                    "threads of project {project:?} use different repositories {repos:?}; \
                     configure the project in config.toml"
                ),
            )),
        }
    }

    /// The worker threads of `project` that record a repository.
    fn project_threads(&self, project: &str) -> Vec<&ThreadView> {
        self.registry
            .list()
            .into_iter()
            .filter(|v| !v.is_orchestrator() && v.project.as_deref() == Some(project))
            .filter(|v| v.repo.is_some())
            .collect()
    }

    /// Configured project `project` keeps its context in the log alone: the rule
    /// [`Core::context_target`] applies without a thread, decided before anything resolves a
    /// local repository for it (its repository may exist only on a runner's box).
    fn configured_project_remote(&self, hub: &Inner, project: &str) -> bool {
        hub.config
            .projects
            .get(project)
            .is_some_and(|p| configured_remote(p, &self.project_threads(project)))
    }

    /// The repository root of configured project `project` (configured `repo`, its threads
    /// `threads`): as last resolved ([`Hub`]'s `context.read` / `context.set` resolve it with
    /// the local box's git before taking the core lock, as thread creation does, which records
    /// it too); before that (a hub just started) the repository its local threads record when
    /// they agree; else `repo`.
    fn project_root(&self, project: &str, repo: &Path, threads: &[&ThreadView]) -> PathBuf {
        if let Some(root) = self.context_roots.get(project) {
            return root.clone();
        }
        let local: BTreeSet<&str> = threads
            .iter()
            .filter(|v| v.box_name == LOCAL_BOX)
            .filter_map(|v| v.repo.as_deref())
            .collect();
        match (local.len(), local.first()) {
            (1, Some(root)) => PathBuf::from(root),
            _ => repo.to_path_buf(),
        }
    }

    /// The last hash logged for `target` (the empty context's hash when none was): for a
    /// local file the last logged for that file, for a remote one the last logged for the
    /// project.
    fn logged_context_hash(&self, target: &ContextTarget) -> String {
        let logged = if target.remote {
            self.project_contexts.get(&target.project)
        } else {
            self.context_hashes.get(&target.key())
        };
        logged.cloned().unwrap_or_else(empty_hash)
    }

    /// The bytes of the latest context logged for a remote `target` (empty when none was).
    fn logged_context(&self, target: &ContextTarget) -> anyhow::Result<Vec<u8>> {
        let hash = self.logged_context_hash(target);
        if hash == empty_hash() {
            return Ok(Vec::new());
        }
        store::get_blob(self.store.conn(), &hash)?
            .ok_or_else(|| anyhow::anyhow!("the logged context {hash} has no blob"))
    }

    /// Adds a context change to `batch`: the bytes as a blob and `context.updated` on the hub
    /// thread.
    /// `file_size`: the bytes are the first [`SET_MAX`] of a larger file of that size
    /// (`ext.twr.truncated`, `ext.twr.file_bytes`).
    fn push_context(
        batch: &mut Batch,
        target: &ContextTarget,
        bytes: &[u8],
        source: Source,
        origin: &str,
        file_size: Option<u64>,
    ) -> anyhow::Result<()> {
        let hash = batch.staged.put(bytes)?;
        let mut ext = json!({
            "project": target.project,
            "path": target.key(),
            "bytes": bytes.len(),
            "origin": origin,
        });
        if let Some(size) = file_size {
            ext["truncated"] = json!(true);
            ext["file_bytes"] = json!(size);
        }
        let body = EventBody::ContextUpdated(ContextUpdated {
            hash,
            ext: twr_ext_map(ext),
            ..Default::default()
        });
        batch.push(HUB_THREAD, None, source, body);
        Ok(())
    }

    /// Adds the capture of a manually edited file (`origin: "file"`) to `batch`.
    fn push_file(batch: &mut Batch, target: &ContextTarget, snap: &FileSnap) -> anyhow::Result<()> {
        if snap.truncated {
            tracing::warn!(path = %target.path.display(), bytes = snap.size, "the standing context is over {SET_MAX} bytes; logging and using its first {SET_MAX}");
        }
        let size = snap.truncated.then_some(snap.size);
        Core::push_context(batch, target, &snap.bytes, Source::Hub, "file", size)
    }

    /// The local context file of `target`, read at most once per version: an unchanged file
    /// costs an `lstat` (`FileCache`). A changed file is read here, under the core lock, but
    /// only [`SET_MAX`] + 1 bytes of it; `pre` is a read made before taking the lock, used
    /// when the file has not changed since (so the RPCs read outside the lock) and is not
    /// racy ([`FileKey::racy`]).
    fn context_file(
        &mut self,
        target: &ContextTarget,
        pre: Option<FileSnap>,
    ) -> std::io::Result<FileSnap> {
        let Some(key) = context_key(&target.path)? else {
            self.context_files.0.remove(&target.path);
            return Ok(FileSnap::empty());
        };
        // A racy version is never trusted: it may have been rewritten since with the same key.
        let cached = pre
            .into_iter()
            .chain(self.context_files.0.get(&target.path).cloned())
            .find(|s| s.key == Some(key) && !key.racy());
        let snap = match cached {
            Some(s) => s,
            None => read_file(&target.path)?,
        };
        match snap.key {
            Some(k) if !k.racy() => {
                self.context_files
                    .0
                    .insert(target.path.clone(), snap.clone());
            }
            _ => {
                self.context_files.0.remove(&target.path);
            }
        }
        Ok(snap)
    }

    /// Bookkeeping for `context.updated` (live and on replay).
    pub(super) fn track_context(&mut self, ev: &Event) {
        if let EventBody::ContextUpdated(p) = &ev.body {
            let twr = twr_ext(&p.ext);
            let key = twr
                .and_then(|t| t.get("path").or_else(|| t.get("project")))
                .and_then(Value::as_str);
            if let Some(key) = key {
                self.context_hashes.insert(key.to_string(), p.hash.clone());
            }
            let project = twr.and_then(|t| t.get("project")).and_then(Value::as_str);
            if let Some(project) = project {
                self.project_contexts
                    .insert(project.to_string(), p.hash.clone());
            }
        }
    }

    /// The standing context for a turn about to start on `thread`: adds a changed file's
    /// `context.updated` to `batch` (the turn's own batch, so the context a turn names is
    /// logged with it; see the module docs), then returns what to prepend (`None`: nothing to
    /// inject). A file the hub cannot read is logged and skipped (the turn still runs).
    pub(super) fn context_for_turn(
        &mut self,
        hub: &Inner,
        thread: &str,
        batch: &mut Batch,
    ) -> anyhow::Result<Option<Injected>> {
        let target = self
            .registry
            .get(thread)
            .and_then(|v| self.thread_context(hub, v));
        let Some(target) = target else {
            return Ok(None);
        };
        let (bytes, size) = if target.remote {
            // The log, never a file (the repository is on another box).
            match self.logged_context(&target) {
                Ok(b) => {
                    let n = b.len();
                    (b, n)
                }
                Err(e) => {
                    tracing::warn!(project = %target.project, error = %format!("{e:#}"), "cannot load the logged standing context; not injecting it");
                    return Ok(None);
                }
            }
        } else {
            let snap = match self.context_file(&target, None) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(path = %target.path.display(), error = %e, "cannot read the standing context; not injecting it");
                    return Ok(None);
                }
            };
            if snap.hash != self.logged_context_hash(&target) {
                Core::push_file(batch, &target, &snap)?;
            }
            let size = usize::try_from(snap.size).unwrap_or(usize::MAX);
            (snap.bytes, size)
        };
        let hash = blob_ref(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        if text.trim().is_empty() {
            return Ok(None);
        }
        let truncated = text.len() > INJECT_MAX;
        let prefix = if truncated {
            tracing::warn!(
                path = %target.path.display(),
                bytes = size,
                "the standing context is over {INJECT_MAX} bytes; injecting the first {INJECT_MAX}"
            );
            let note = format!(
                "[twr: this standing context was cut to its first {INJECT_MAX} of {} bytes; \
                 the rest is in {}]",
                size.max(text.len()),
                target.path.display()
            );
            injection(
                &target.project,
                truncate_utf8(&text, INJECT_MAX),
                Some(&note),
            )
        } else {
            injection(&target.project, &text, None)
        };
        Ok(Some(Injected {
            prefix,
            hash,
            truncated,
        }))
    }
}

/// Every thread in `threads` is on a remote box (none: not remote).
fn all_remote(threads: &[&ThreadView]) -> bool {
    !threads.is_empty() && threads.iter().all(|v| v.box_name != LOCAL_BOX)
}

/// A configured project (its worker threads `threads`) is log-only: its `box` is remote,
/// or it names no remote box and all its threads run on remote boxes anyway (`--box`).
fn configured_remote(p: &ProjectConfig, threads: &[&ThreadView]) -> bool {
    match p.box_name.as_deref() {
        Some(b) if b != LOCAL_BOX => true,
        _ => all_remote(threads),
    }
}

/// `ext.twr` fields a turn with injected context records.
pub(super) fn turn_ext(injected: &Injected, twr: &mut Map<String, Value>) {
    twr.insert("context_hash".into(), json!(injected.hash));
    if injected.truncated {
        twr.insert("context_truncated".into(), json!(true));
    }
}

impl Hub {
    /// Resolves where configured local project `project` keeps its repository (see
    /// [`Core::context_target`]) as thread creation does (`hub::git::prepare_workspace`): the
    /// local box's git top level of its `repo`, or `repo` outside git; outside the core lock.
    /// Nothing to do for another project, or for a log-only one (decided first: its
    /// repository may exist only on a runner's box, where local git cannot see it).
    pub(super) async fn resolve_project_root(&self, project: &str) -> Result<(), RpcError> {
        let Some(p) = self.inner.config.projects.get(project) else {
            return Ok(());
        };
        if self
            .inner
            .core()
            .configured_project_remote(&self.inner, project)
        {
            return Ok(());
        }
        let Some(host) = self.inner.hosts.get(LOCAL_BOX) else {
            return Ok(());
        };
        // Git's output stays in the hub's log: it can echo the repository's configuration,
        // credentials included, and the orchestrator reads this error too.
        let root = host
            .git()
            .repo_root(p.repo.clone())
            .await
            .map_err(|e| {
                tracing::warn!(project, error = %format!("{e:#}"), "cannot find a project's repository");
                rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "git could not find the repository of {project} ({}); the hub's log \
                         says why",
                        p.repo.display()
                    ),
                )
            })?
            .unwrap_or_else(|| p.repo.clone());
        self.inner
            .core()
            .context_roots
            .insert(project.to_string(), root);
        Ok(())
    }

    /// `context.read {project}`: `{project, path, content, hash, bytes, remote, utf8,
    /// truncated}` (a missing file is an empty context; non-UTF-8 bytes are replaced and
    /// `utf8` is false; a file over [`SET_MAX`] bytes is returned cut to that with `truncated`
    /// and `file_bytes`). The file is read outside the core lock; a symlinked `.twr/` or
    /// `context.md` is refused (`conflict`).
    pub async fn context_read(&self, project: &str) -> Result<Value, RpcError> {
        self.resolve_project_root(project).await?;
        let (target, logged) = {
            let core = self.inner.core();
            let target = core.context_target(&self.inner, project, None)?;
            let logged = if target.remote {
                Some(core.logged_context(&target).map_err(internal)?)
            } else {
                None
            };
            (target, logged)
        };
        let snap = match logged {
            Some(b) => FileSnap {
                hash: blob_ref(&b),
                size: b.len() as u64,
                bytes: b,
                ..FileSnap::default()
            },
            None => read_file(&target.path).map_err(|e| file_err(&target.path, e))?,
        };
        let mut r = json!({
            "project": target.project,
            "path": target.path,
            "content": String::from_utf8_lossy(&snap.bytes),
            "hash": snap.hash,
            "bytes": snap.bytes.len(),
            "remote": target.remote,
            "utf8": snap.utf8(),
            "truncated": snap.truncated,
        });
        if snap.truncated {
            r["file_bytes"] = json!(snap.size);
        }
        Ok(r)
    }

    /// `context.set {project, content, expect_hash?}`: records the new context (blob +
    /// `context.updated` on the hub thread), then writes the file atomically. Returns
    /// `{project, path, hash, bytes, changed}`; `changed: false` (nothing appended or written)
    /// when the file and the log already hold exactly this content.
    ///
    /// - `expect_hash` (the `hash` a `context.read` returned): the context must still be that
    ///   one, else `conflict` and nothing changes (no lost update over a concurrent edit).
    /// - A file edited by hand since the last logged hash is logged first (`origin: "file"`,
    ///   same batch), so the log keeps what it replaced.
    /// - A file that is not UTF-8, a symlink, or not a regular file is never written over
    ///   (`conflict`); nor is a file over [`SET_MAX`] bytes (its hash covers only its first
    ///   [`SET_MAX`], so it cannot prove an `expect_hash` current; edit it directly).
    ///
    /// The file is read before the core lock is taken and re-validated under it (by its
    /// inode, size and timestamps; re-read only if it changed).
    pub async fn context_set(
        &self,
        project: &str,
        content: &str,
        expect_hash: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if content.len() > SET_MAX {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("content is longer than {SET_MAX} bytes"),
            ));
        }
        self.resolve_project_root(project).await?;
        let pre = {
            let core = self.inner.core();
            let target = core.context_target(&self.inner, project, None)?;
            (!target.remote).then_some(target)
        };
        let pre = match pre {
            Some(t) => Some(read_file(&t.path).map_err(|e| file_err(&t.path, e))?),
            None => None,
        };
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let target = core.context_target(&self.inner, project, None)?;
        let bytes = content.as_bytes();
        let hash = blob_ref(bytes);
        let logged = core.logged_context_hash(&target);
        // A remote project's context is the log: there is no file here to compare.
        let snap = if target.remote {
            None
        } else {
            Some(
                core.context_file(&target, pre)
                    .map_err(|e| file_err(&target.path, e))?,
            )
        };
        // A snapshot whose hash does not cover the whole file (a file over the cap, or one that
        // changed size while it was read) cannot tell a stale `expect_hash` from the current
        // one, and a capture of it would lose the rest: refused before any comparison or event.
        if let Some(s) = &snap {
            if s.truncated {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} is {} bytes, larger than twr manages ({SET_MAX} bytes, 1 MiB); \
                         edit it directly",
                        target.path.display(),
                        s.size
                    ),
                ));
            }
            if !s.whole() {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} changed while it was read; read it again",
                        target.path.display()
                    ),
                ));
            }
        }
        let current = snap.as_ref().map_or(logged.as_str(), |s| s.hash.as_str());
        if let Some(expected) = expect_hash {
            if expected != current {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "the standing context of {project:?} changed since it was read \
                         (expected {expected}, now {current}); read it again"
                    ),
                ));
            }
        }
        if let Some(s) = &snap {
            if !s.utf8() {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} is not UTF-8; twr will not write over it (fix or remove it)",
                        target.path.display()
                    ),
                ));
            }
        }
        let on_disk = current.to_string();
        let changed = on_disk != hash || logged != hash;
        if changed {
            let mut batch = Batch::default();
            if let Some(s) = snap.as_ref().filter(|s| s.hash != logged && s.hash != hash) {
                // An unlogged manual edit: the log records what the set replaces.
                Core::push_file(&mut batch, &target, s).map_err(internal)?;
            }
            Core::push_context(&mut batch, &target, bytes, caller.source(), "set", None)
                .map_err(internal)?;
            core.append_batch(&self.inner, batch).map_err(append_err)?;
            if let Some(s) = snap.filter(|_| on_disk != hash) {
                // The event goes first and the file second: they cannot be one transaction. A
                // failed (or crashed) write leaves the file as it was, and the next dispatch
                // to the project logs the file's actual content (`origin: "file"`), so the log
                // converges on the file; the caller gets the error and can set it again.
                write_atomic(&target.path, bytes, s.mode).map_err(|e| {
                    let msg = format!(
                        "the new context is recorded, but writing {} failed: {e}",
                        target.path.display()
                    );
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        rpc(codes::CONFLICT, msg)
                    } else {
                        internal(msg)
                    }
                })?;
            }
        }
        let mut r = json!({
            "project": target.project,
            "path": target.path,
            "hash": hash,
            "bytes": bytes.len(),
            "changed": changed,
        });
        if target.remote {
            r["remote"] = json!(true);
        }
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Hook = Box<dyn Fn(&Path)>;

    thread_local! {
        /// Run by [`read_file`] between its check and its open (on this thread).
        static BEFORE_OPEN: std::cell::RefCell<Option<Hook>> =
            const { std::cell::RefCell::new(None) };
    }

    thread_local! {
        /// Run by [`write_atomic`] once it holds `.twr/` (on this thread).
        static BEFORE_WRITE: std::cell::RefCell<Option<Hook>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn before_write(path: &Path) {
        BEFORE_WRITE.with(|h| {
            if let Some(h) = h.borrow().as_ref() {
                h(path)
            }
        });
    }

    /// A `.twr/` swapped for a symlink to another directory once checked, before the context
    /// is written: nothing is written through the link (the other directory's `context.md`
    /// stays), the write is refused, and no temp file is left in either.
    #[test]
    fn a_context_directory_swapped_before_a_write_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".twr").join("context.md");
        write_atomic(&path, b"mine", None).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("context.md"), b"theirs").unwrap();
        let (twr, moved) = (dir.path().join(".twr"), dir.path().join(".twr-old"));
        let swap = {
            let (twr, moved, elsewhere) = (twr.clone(), moved.clone(), elsewhere.clone());
            move |_: &Path| {
                std::fs::rename(&twr, &moved).unwrap();
                std::os::unix::fs::symlink(&elsewhere, &twr).unwrap();
            }
        };
        BEFORE_WRITE.with(|h| *h.borrow_mut() = Some(Box::new(swap)));
        let written = write_atomic(&path, b"new", None);
        BEFORE_WRITE.with(|h| *h.borrow_mut() = None);
        let e = written.expect_err("wrote through the swapped-in link");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains("changed while"), "{e}");
        let names = |d: &Path| -> Vec<_> {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            std::fs::read(elsewhere.join("context.md")).unwrap(),
            b"theirs"
        );
        assert_eq!(names(&elsewhere), ["context.md"]);
        assert_eq!(std::fs::read(moved.join("context.md")).unwrap(), b"mine");
        assert_eq!(names(&moved), ["context.md"]);
        // A `.twr/` that is a link when the write starts is refused, as on reads.
        let e = write_atomic(&path, b"new", None).expect_err("followed a symlinked .twr");
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(
            std::fs::read(elsewhere.join("context.md")).unwrap(),
            b"theirs"
        );
        // And a `context.md` that is one.
        std::fs::remove_file(&twr).unwrap();
        std::fs::rename(&moved, &twr).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("context.md"), &path).unwrap();
        let e = write_atomic(&path, b"new", None).expect_err("followed a symlinked file");
        assert!(e.to_string().contains("symbolic link"), "{e}");
        assert_eq!(
            std::fs::read(elsewhere.join("context.md")).unwrap(),
            b"theirs"
        );
    }

    pub(super) fn before_open(path: &Path) {
        BEFORE_OPEN.with(|h| {
            if let Some(h) = h.borrow().as_ref() {
                h(path)
            }
        });
    }

    /// A `.twr/` swapped for a symlink to another directory between the check and the open
    /// (`O_NOFOLLOW` covers only the last component) is refused: what was opened is not the
    /// file checked.
    #[test]
    fn a_file_swapped_between_check_and_open_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".twr").join("context.md");
        write_atomic(&path, b"mine", None).unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("context.md"), b"secret").unwrap();
        let (twr, moved) = (dir.path().join(".twr"), dir.path().join(".twr-old"));
        let swap = {
            let (twr, moved, elsewhere) = (twr.clone(), moved.clone(), elsewhere.clone());
            move |_: &Path| {
                std::fs::rename(&twr, &moved).unwrap();
                std::os::unix::fs::symlink(&elsewhere, &twr).unwrap();
            }
        };
        BEFORE_OPEN.with(|h| *h.borrow_mut() = Some(Box::new(swap)));
        let read = read_file(&path);
        BEFORE_OPEN.with(|h| *h.borrow_mut() = None);
        let e = read.expect_err("read the swapped-in file");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData, "{e}");
        assert!(e.to_string().contains("changed while it was opened"), "{e}");
        // Swapped back: read as usual.
        std::fs::remove_file(&twr).unwrap();
        std::fs::rename(&moved, &twr).unwrap();
        assert_eq!(read_file(&path).unwrap().bytes, b"mine");
    }

    /// `O_NOFOLLOW` is right for this platform: a symlinked `context.md` fails to open.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_open_flags_refuse_a_symlink() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real"), b"x").unwrap();
        std::os::unix::fs::symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
        let e = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_FLAGS)
            .open(dir.path().join("link"))
            .expect_err("followed a symlink");
        assert_eq!(e.raw_os_error(), Some(ELOOP), "{e}");
    }

    #[test]
    fn injection_wraps_the_context() {
        assert_eq!(
            injection("api", "Use pnpm.\n\n", None),
            "<twr-context project=\"api\">\nUse pnpm.\n</twr-context>\n\n"
        );
        assert_eq!(
            injection("a\"b", "x", Some("[cut]")),
            "<twr-context project=\"a_b\">\nx\n\n[cut]\n</twr-context>\n\n"
        );
    }

    #[test]
    fn atomic_writes_create_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".twr").join("context.md");
        write_atomic(&path, b"one", None).unwrap();
        write_atomic(&path, b"two", None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["context.md"]);
        let missing = read_file(&dir.path().join("missing")).unwrap();
        assert_eq!((missing.bytes.len(), missing.hash), (0, empty_hash()));
    }
}

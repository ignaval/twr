//! The box's trash: where a deleted thread's worktree, its administrative record and its
//! branch go ([`GitOps::worktree_trash`]) until they are purged ([`GitOps::trash_purge`]). A
//! restore ([`GitOps::trash_restore`]) makes the thread's worktree and branch again from an
//! item, which it never moves or changes: the item stays until it is purged.
//!
//! An item is `<trash>/<item>/` (the trash directory is the box's `<data dir>/trash`, private
//! to its user), named by a ULID the hub makes ([`valid_trash_id`]), holding:
//!
//! - `manifest.json` ([`TrashManifest`]): the thread, the repository, where the worktree was,
//!   what the delete card said it loses, the refs below, what moved, its size and how to get
//!   it back by hand. Written first (`complete: false`), after each step, and once everything
//!   moved.
//! - `lock`: held (`flock`) by the one operation at work on the item: its move into the
//!   trash from the item's creation on, a restore, the undoing of one, a purge. Each of them
//!   takes it before reading the manifest, so none of them works on an item another one is
//!   changing or reading.
//! - `worktree/`: the worktree's directory, moved (renamed; across file systems copied whole,
//!   checked, and only then removed at the source). Its `.git` file still names the record's
//!   old path. Its build output is not there: ignored entries of the box's `[threads]
//!   park_disposable` (`target/`, `node_modules/`, ...) holding no git repository are deleted
//!   just before the move ([`drop_build_output`], each checked again as it goes; the manifest
//!   names them), as a park deletes them. They are regenerated, and would keep tens of GB on
//!   disk until the purge. That is safe against symlinks and repositories there when it runs
//!   (no symlink it meets is followed), not against another process writing into the worktree
//!   meanwhile (a parent swapped for a symlink, a repository made while it deletes): twr has
//!   stopped the thread's runtime by then. Only on Linux and macOS, where mount points can be
//!   told ([`mounts_inside`]): elsewhere build output goes to the trash with the rest. An
//!   entry it can neither delete nor put back (its place taken) stays in the item apart, the
//!   manifest records it, and a restore puts back what it records ([`put_back_aside`]). Each
//!   entry is written down in the worktree's record before it moves aside ([`JOURNAL`]): a
//!   deletion stopped part way is put right by the next one ([`reconcile_staged`]). On Linux
//!   and macOS a worktree with a file system mounted in it is not deleted at all
//!   ([`mounts_inside`]), so neither a move, a copy across file systems nor a purge meets a
//!   mount twr carried into the trash.
//! - `record/`: the worktree's administrative record (`<common>/worktrees/<name>`, with its
//!   `HEAD`, reflogs, own refs, index and the repositories of its submodules under
//!   `modules/`), moved the same way: git forgets the worktree.
//! - `index-objects/pack-<id>.*`: every object the record's index names (staged content of
//!   every stage, its cache tree and resolve-undo entries, sparse directory trees) that none of
//!   the item's commits has: once the record is out of the repository, `git gc` no longer sees
//!   that index and would prune them.
//! - `reflogs/branch`: the branch's reflog as it was, verbatim. The copy under the item's
//!   branch ref keeps reflog-only commits as long as `git gc` does not expire it; this one no
//!   gc touches, and a restore writes the new branch's reflog from it.
//!
//! In the repository, before anything moves, refs under `refs/twr-trash/<item>/`
//! ([`TRASH_REF_PREFIX`]) keep every commit the deletion would otherwise make unreachable:
//! `branch` (the branch's tip, the branch's reflog copied to the new ref's), `head` (the
//! worktree's `HEAD` commit, its `HEAD` reflog copied), `wt/<ref>` (the worktree's own refs,
//! `refs/worktree/*`, `refs/bisect/*`, with their reflogs), and `reflog/<id>` for each commit
//! only a reflog named (reflog entries expire on their own schedule; a ref does not). Only
//! then is the branch deleted (`git branch -D`). They stay until the item is purged.
//!
//! Nothing else is deleted on the way: a step that fails leaves what did not move where it
//! was, and what did in the item (the error says so). A purge removes only `<trash>/<id>`
//! directories named by a valid id whose manifest reads and names that id, and only refs under
//! `refs/twr-trash/<id>/` of the repository the manifest names; never an item another
//! operation holds, or an incomplete one that holds anything its deletion moved or the only
//! copy of a commit (one its refs or saved reflogs reach and nothing else of the repository
//! does: [`LocalGit::only_in_item`], never whether a branch of the thread's name exists).
//! Purges of one repository's items run one at a time on the box, in any of its processes:
//! each checks and drops its refs under the repository's purge lock ([`lock_repo`]), so two
//! never both count the other's refs as the copy that survives them.
//!
//! Every git command sees the literal object graph (no replacement objects, no legacy
//! grafts): a commit only a graft makes reachable from a kept tip gets a ref of its own.
//!
//! A restore copies; it moves and edits nothing of the item, and it never deletes, moves or
//! overwrites anything it did not make itself. Its worktree goes to the first place offered
//! where nothing is, that no worktree record of the repository names (stale ones included:
//! a restore clears none), and whose directory it then makes itself (`mkdir`, never over
//! anything: its claim of the place; the missing directories above it made and noted too).
//! It creates the branch at the item's `branch` ref (or its `head` ref when it keeps no
//! branch) with `update-ref create` (never over an existing one; its reflog written from
//! `reflogs/branch` when the branch has none), lets git add a worktree on it in the claimed,
//! empty directory (`git worktree add --no-checkout`, which refuses a directory that is not
//! empty or that a record names), finds the record git made by the new `.git` file (under
//! `<common>/worktrees/`, naming the worktree back, on the branch), marks it as the
//! restore's ([`RESTORE_MARK`]: the item, the branch, its commit), checks the branch's files
//! out, then copies the saved files over them: the saved tree's own `.git`
//! skipped, every entry looked at with `symlink_metadata`, symlinks recreated as symlinks,
//! files copied with their permissions (opened without following a symlink), what the checkout
//! has in the way (a symlink too) removed rather than written through, sockets, FIFOs and
//! devices skipped and reported; tracked files the saved tree does not have are deleted from
//! the new worktree (they were deleted there). The repositories of its submodules
//! (`record/modules/`) are copied into the new record when it has none, and each checked-out
//! submodule, nested ones too, is connected to its copy (its `.git` file and the copy's
//! `core.worktree`, which still named the old places: at a fallback path, or under another
//! name, the old record's name may be another worktree's now), through directories held
//! open ([`LocalGit::reconnect_submodules`]). Copying them or connecting any one fails the
//! restore, and so does a `.git` file of the restored worktree that still names a place in
//! the repository's git directory after that (a repository not found to connect it to): none
//! is left naming another worktree's repository. The index is not
//! brought back (staged changes come back as unstaged ones; `index-objects/` keeps the staged
//! versions), nor is a `HEAD` that was detached or on another branch (`head` keeps its commit).
//! A step that fails removes what the restore made, and only that ([`Made`]); when it failed
//! because git was killed past its timeout ([`super::Killed`]), what git was changing is in
//! an unknown state, and nothing is removed: the error says what is left. Otherwise: the worktree
//! git added in the claimed directory (`git worktree remove`), the record it found (while
//! that names the worktree), the claimed directory (only an empty one, `rmdir`, when git did
//! not add the worktree) and the directories above it it made (only empty ones), the branch
//! while it points where the restore put it and is checked out nowhere, the reflog it wrote.
//! When the hub cannot record the restore, [`GitOps::trash_restore_undo`] removes them,
//! found by the mark (also after a crash), only a worktree whose `.git` names the marked
//! record that names it back: the item is untouched either way, so a restore can be tried
//! again.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{ErrorKind, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use twr_core::git::{
    quote_path, unquote_path, valid_trash_id, KeptAside, PurgeOutcome, RestoreOutcome,
    RestoreRequest, RestoredCopy, TrashFailure, TrashItem, TrashManifest, TrashOutcome, TrashPurge,
    TrashRef, TrashRequest, LISTED_MAX, TRASH_REF_PREFIX,
};
use twr_core::paths::resolve_lenient;

use super::dirfd::{self, Dir};
use super::{
    disposable, disposable_in_both, dot_git, files_under, head_tip, is_absent, linked_outside,
    linked_worktree, may_exist, object_id, path_of, presence, read_reflog, record_of, record_roots,
    repositories_in, repository_at, trim_newline, was_killed, worktree_records, DotGit, LocalGit,
    Mode, Presence,
};
use crate::workdirs::{ChangeKind, CHANGE_WAIT};

/// The file a restore leaves in the record of the worktree it made: the item's id, the
/// branch it created and the commit it created it at, one per line. It finds what to remove
/// when the hub could not record the restore ([`GitOps::trash_restore_undo`]).
///
/// [`GitOps::trash_restore_undo`]: twr_core::git::GitOps::trash_restore_undo
const RESTORE_MARK: &str = "twr-restore";

/// The manifest's file name in an item.
const MANIFEST: &str = "manifest.json";

/// The lock file of an item.
const LOCK: &str = "lock";

/// The directory of an item with the pack of its index's objects.
const INDEX_OBJECTS: &str = "index-objects";

/// The directory of an item with its verbatim reflog copies.
const REFLOGS: &str = "reflogs";

/// The message of the reflog entries the trash's refs get.
const TRASHED_MSG: &str = "twr: moved to the trash";

/// The message of the reflog entry a restored branch gets.
const RESTORED_MSG: &str = "twr: restored from the trash";

/// An incomplete item no operation holds and whose manifest has not changed for this long is
/// abandoned: a purge takes it when it holds nothing its deletion moved.
pub(super) const ABANDONED_AFTER: Duration = Duration::from_secs(3600);

/// How to get an item back by hand (the manifest's `recover`).
fn recover_text(m: &TrashManifest, dir: &Path) -> String {
    let id = &m.item;
    let mut out = format!(
        "`twr trash restore {id}` brings the thread back (a new worktree and branch made from \
         this item, which stays until it is purged). By hand: the branch is \
         refs/twr-trash/{id}/branch in {} (`git branch <name> refs/twr-trash/{id}/branch`); the \
         worktree's files are in {}/worktree, whose .git file still names the record's old path \
         (its record is {}/record: move it back to {}/worktrees/<name> and run `git worktree \
         repair <path>` there, or copy the files elsewhere).",
        m.common.display(),
        dir.display(),
        dir.display(),
        m.common.display(),
    );
    if m.refs.iter().any(|r| r.kind == "head") {
        out.push_str(&format!(
            " The worktree's HEAD was {}; refs/twr-trash/{id}/head keeps it.",
            m.head.as_deref().unwrap_or("detached")
        ));
    }
    if m.index_pack.is_some() {
        out.push_str(&format!(
            " What its index had staged is in {}/{INDEX_OBJECTS}: copy those pack files into \
             {}/objects/pack before a record with that index goes back.",
            dir.display(),
            m.common.display()
        ));
    }
    if m.branch_log_saved {
        out.push_str(&format!(
            " The branch's reflog as it was is {}/{REFLOGS}/branch.",
            dir.display()
        ));
    }
    if !m.build_output_aside.is_empty() {
        out.push_str(&format!(
            " Build output its deletion could neither delete nor put back is in {}/worktree: {}.",
            dir.display(),
            m.build_output_aside
                .iter()
                .map(|k| format!("{} (was {})", k.at, k.path))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// Writes `m` as `dir`'s manifest (through a temporary file, renamed over).
fn write_manifest(dir: &Path, m: &TrashManifest) -> anyhow::Result<()> {
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    let body = serde_json::to_vec_pretty(m).context("encoding the trash manifest")?;
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, dir.join(MANIFEST))
        .with_context(|| format!("writing {}", dir.join(MANIFEST).display()))
}

/// The manifest of item `id` in `dir`: it must read, parse and name `id`.
fn read_manifest(dir: &Path, id: &str) -> Result<TrashManifest, String> {
    let file = dir.join(MANIFEST);
    let raw = std::fs::read(&file).map_err(|e| format!("reading {}: {e}", file.display()))?;
    let m: TrashManifest =
        serde_json::from_slice(&raw).map_err(|e| format!("{}: {e}", file.display()))?;
    if m.item != id {
        return Err(format!("{} names item {:?}", file.display(), m.item));
    }
    Ok(m)
}

/// The directory of item `id` in the trash `root`, when it is one: a valid id, a directory
/// (not a symlink).
fn item_dir(root: &Path, id: &str) -> Result<PathBuf, String> {
    if !valid_trash_id(id) {
        return Err(format!("{id:?} is not a trash item id"));
    }
    let dir = root.join(id);
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if m.is_dir() => Ok(dir),
        Ok(_) => Err(format!("{} is not a directory", dir.display())),
        Err(e) if is_absent(&e) => Err(format!("no item {id} in the trash")),
        Err(e) => Err(format!("{}: {e}", dir.display())),
    }
}

/// An item's lock, held until dropped.
pub(super) struct ItemLock(#[allow(dead_code)] std::fs::File);

/// How long taking an item's lock waits for another holder: a process the box starts (git,
/// a runtime) holds a copy of every open file from its fork until it executes, so a lock just
/// released can stay held that long.
#[cfg(not(test))]
const LOCK_WAIT: Duration = Duration::from_secs(5);
#[cfg(test)]
const LOCK_WAIT: Duration = Duration::from_millis(500);

/// Takes the lock of the item `dir` (made when `create`), waiting at most [`LOCK_WAIT`] for
/// another holder, or says it is busy.
pub(super) async fn lock_item(dir: &Path, create: bool) -> anyhow::Result<ItemLock> {
    let deadline = tokio::time::Instant::now() + LOCK_WAIT;
    loop {
        match try_lock_item(dir, create) {
            Err(e) if e.busy && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            r => return r.map_err(|e| e.error),
        }
    }
}

/// Why [`try_lock_item`] did not take the lock.
struct NotLocked {
    error: anyhow::Error,
    /// Another holder has it.
    busy: bool,
}

/// [`lock_item`] without waiting. The lock file must still be the one at its path once
/// locked: a purge removes it last, and an operation that opened it meanwhile holds a file
/// nobody else will.
fn try_lock_item(dir: &Path, create: bool) -> Result<ItemLock, NotLocked> {
    let fail = |error: anyhow::Error| NotLocked { error, busy: false };
    let path = dir.join(LOCK);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .open(&path)
        .map_err(|e| {
            fail(if is_absent(&e) {
                anyhow!(
                    "trash item {} is being removed (it has no lock file)",
                    dir.display()
                )
            } else {
                anyhow!("opening {}: {e}", path.display())
            })
        })?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(NotLocked {
                error: anyhow!(
                    "trash item {} is busy: it is being moved to the trash, restored or purged",
                    dir.display()
                ),
                busy: true,
            })
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(fail(anyhow!("locking {}: {e}", path.display())))
        }
    }
    let held = file.metadata().map(|m| (m.dev(), m.ino()));
    let there = std::fs::symlink_metadata(&path).map(|m| (m.dev(), m.ino()));
    match (held, there) {
        (Ok(a), Ok(b)) if a == b => Ok(ItemLock(file)),
        _ => Err(fail(anyhow!(
            "trash item {} is being removed",
            dir.display()
        ))),
    }
}

/// A repository's purge lock ([`lock_repo`]), held until dropped.
pub(super) struct RepoLock(#[allow(dead_code)] std::fs::File);

/// How long taking a repository's purge lock waits for another purge of it: one holds it
/// through a few reads of the repository's refs and history and the deletion of its refs.
#[cfg(not(test))]
const REPO_LOCK_WAIT: Duration = Duration::from_secs(120);
#[cfg(test)]
const REPO_LOCK_WAIT: Duration = Duration::from_secs(10);

/// Takes the purge lock of the repository whose git common directory is `common`: an
/// exclusive `flock` of the directory itself, so that it is one lock per
/// repository however its path is spelled, held by one purge at a time in any process of the
/// box (the hub's own box and a runner on the same machine, two data directories), and
/// nothing is written into the repository. Waits at most [`REPO_LOCK_WAIT`] for another
/// holder. `None` when the repository is gone (a purge then touches nothing of it).
async fn lock_repo(common: &Path) -> anyhow::Result<Option<RepoLock>> {
    let deadline = tokio::time::Instant::now() + REPO_LOCK_WAIT;
    let dir = match std::fs::File::open(common) {
        Ok(f) => f,
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => bail!("opening {}: {e}", common.display()),
    };
    loop {
        match dir.try_lock() {
            Ok(()) => return Ok(Some(RepoLock(dir))),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "another purge of the trash in {} is still at work",
                common.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                bail!("locking {} for the purge: {e}", common.display())
            }
        }
    }
}

/// Removes the item directory `dir`: everything but its manifest and lock, then the manifest,
/// the lock and the directory, so that one cut short is still listed (and purged again).
fn remove_item(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != MANIFEST && name != LOCK {
            remove_tree(&entry.path())?;
        }
    }
    for last in [MANIFEST, LOCK] {
        match std::fs::remove_file(dir.join(last)) {
            Err(e) if !is_absent(&e) => return Err(e),
            _ => {}
        }
    }
    std::fs::remove_dir(dir)
}

/// Whether `e` says a rename crossed file systems.
fn crosses_devices(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::CrossesDevices || e.raw_os_error() == Some(18)
}

/// What [`move_tree`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Moved {
    /// Renamed: all of it is at the destination.
    Renamed,
    /// Copied to another file system, whole, then the source removed: `left` when some of the
    /// source stayed (an entry that could not be removed, or one not copied: a socket, a FIFO,
    /// a device, which hold no data, stay at the source).
    Copied { left: bool },
}

/// Moves `src` to `dst` (which must not exist): a rename, or across file systems (or with
/// `copy`) a copy (files, directories with their permissions, symlinks as they are; sockets,
/// FIFOs and devices stay at the source) that is checked whole (each file's size as copied)
/// and then `on_copied` (which notes it), and only then the removal of each copied entry at
/// the source; never an entry that was not copied. A copy that fails, or whose `on_copied`
/// fails, removes only the partial copy it made and leaves the source whole.
pub(super) fn move_tree(
    src: &Path,
    dst: &Path,
    copy: bool,
    on_copied: &mut dyn FnMut() -> std::io::Result<()>,
) -> std::io::Result<Moved> {
    if !matches!(presence(dst), Presence::Absent) {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!("{} exists", dst.display()),
        ));
    }
    if !copy {
        match std::fs::rename(src, dst) {
            Ok(()) => return Ok(Moved::Renamed),
            Err(e) if crosses_devices(&e) => {}
            Err(e) => return Err(e),
        }
    }
    let mut copied = Copied::default();
    let result = copy_tree(src, dst, &mut copied, true).and_then(|()| on_copied());
    if let Err(e) = result {
        if copied.made_root {
            if let Err(r) = remove_tree(dst) {
                tracing::warn!(path = %dst.display(), error = %r, "cannot remove a partial copy");
            }
        }
        return Err(e);
    }
    Ok(Moved::Copied {
        left: remove_sources(&copied),
    })
}

/// What [`copy_tree`] copied, at the source.
#[derive(Default)]
struct Copied {
    files: Vec<PathBuf>,
    /// In the order they were made (parents first).
    dirs: Vec<PathBuf>,
    skipped: bool,
    /// The destination itself was made (so a failure removes it).
    made_root: bool,
}

/// Copies `src` to `dst` (see [`move_tree`]), noting each source entry copied.
fn copy_tree(src: &Path, dst: &Path, copied: &mut Copied, root: bool) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    let kind = meta.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        copied.made_root |= root;
        copied.files.push(src.to_path_buf());
    } else if kind.is_file() {
        let n = std::fs::copy(src, dst)?;
        copied.made_root |= root;
        if n != meta.len() {
            return Err(std::io::Error::other(format!(
                "{} changed while it was copied ({n} of {} bytes)",
                src.display(),
                meta.len()
            )));
        }
        copied.files.push(src.to_path_buf());
    } else if kind.is_dir() {
        std::fs::create_dir(dst)?;
        copied.made_root |= root;
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o700))?;
        copied.dirs.push(src.to_path_buf());
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), copied, false)?;
        }
        std::fs::set_permissions(dst, meta.permissions())?;
    } else {
        // A socket, FIFO or device: not copied, so not removed.
        copied.skipped = true;
    }
    Ok(())
}

/// Removes the source entries `copied` copied (each directory made writable by its owner
/// first): whether some of the source stayed.
fn remove_sources(copied: &Copied) -> bool {
    let mut left = copied.skipped;
    for d in &copied.dirs {
        if let Ok(meta) = std::fs::symlink_metadata(d) {
            let mode = meta.permissions().mode();
            if mode & 0o700 != 0o700 {
                let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(mode | 0o700));
            }
        }
    }
    for f in &copied.files {
        if std::fs::remove_file(f).is_err() {
            left = true;
        }
    }
    for d in copied.dirs.iter().rev() {
        if std::fs::remove_dir(d).is_err() {
            left = true;
        }
    }
    left
}

/// Removes `path` and everything under it, symlinks not followed, making each directory
/// readable and writable by its owner first (a read-only directory of a Go module cache would
/// stop `remove_dir_all`).
pub(super) fn remove_tree(path: &Path) -> std::io::Result<()> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if is_absent(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    if !meta.is_dir() {
        return std::fs::remove_file(path);
    }
    let mode = meta.permissions().mode();
    if mode & 0o700 != 0o700 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o700))?;
    }
    for entry in std::fs::read_dir(path)? {
        remove_tree(&entry?.path())?;
    }
    std::fs::remove_dir(path)
}

/// Bytes on disk under `path` (blocks allocated), symlinks not followed; what cannot be read
/// counts nothing.
pub(super) fn disk_usage(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut todo = vec![path.to_path_buf()];
    while let Some(p) = todo.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        total = total.saturating_add(meta.blocks().saturating_mul(512));
        if meta.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&p) {
                todo.extend(entries.flatten().map(|e| e.path()));
            }
        }
    }
    total
}

/// The entries of `ignored` (the work tree at `root`'s own ignored entries, as `git status`
/// lists them) that a deletion deletes instead of moving them to the trash: build output and
/// caches per `list` ([`disposable`]), each a directory (or a file) under real directories
/// only (no symlink on the way), holding no git repository ([`may_hold_repository`]). What the
/// deletion deletes is checked again as it goes ([`drop_build_output`]). (A work tree with a
/// mount point in it is not deleted at all, [`mounts_inside`].)
pub(super) fn build_output(
    root: &Path,
    ignored: &[String],
    list: &[String],
    budget: &mut u64,
) -> Vec<String> {
    ignored
        .iter()
        .filter(|p| disposable(p, list))
        .filter(|p| {
            let rel = unquote_path(p.trim_end_matches('/'));
            let mut at = root.to_path_buf();
            let comps: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
            for (i, c) in comps.iter().enumerate() {
                if c.is_empty() || *c == b"." || *c == b".." {
                    return false;
                }
                at.push(OsStr::from_bytes(c));
                let Ok(meta) = std::fs::symlink_metadata(&at) else {
                    return false;
                };
                let last = i + 1 == comps.len();
                if !last || p.ends_with('/') {
                    if !meta.is_dir() {
                        return false;
                    }
                } else {
                    return !meta.is_dir();
                }
            }
            !may_hold_repository(root, &rel, budget)
        })
        .cloned()
        .collect()
}

/// The mount points inside the work tree at `root` (below its top level, which may be one),
/// as [`quote_path`] writes them, sorted: every one of this process's ([`mount_list`]: Linux's
/// [`mount_points`], macOS's [`mount_names`]) under `root` resolved, by whole components
/// (`target-old` is not under `target`); an error when they cannot be read. `None` where there
/// is no mount list (any system but Linux and macOS): then nothing is refused for a mount, and
/// no build output is deleted or trimmed (it goes to the trash with the rest).
pub(super) fn mounts_inside(root: &Path) -> Option<Result<Vec<String>, String>> {
    let real = match std::fs::canonicalize(root) {
        Ok(r) => r,
        Err(e) => {
            return HAS_MOUNT_LIST.then(|| Err(format!("cannot resolve {}: {e}", root.display())))
        }
    };
    let points = match mount_list(&real) {
        MountList::None => return None,
        MountList::Unreadable => {
            return Some(Err(format!(
                "where file systems are mounted cannot be read ({MOUNT_SOURCE})"
            )))
        }
        MountList::Read(text) => mount_points(&text),
        MountList::Points(points) => points,
    };
    let mut inside: Vec<String> = points
        .iter()
        .filter_map(|m| m.strip_prefix(&real).ok())
        .filter(|rel| !rel.as_os_str().is_empty())
        .map(|rel| quote_path(rel.as_os_str().as_bytes()))
        .collect();
    inside.sort();
    inside.dedup();
    Some(Ok(inside))
}

/// Why the work tree `whose` is not deleted, as [`mounts_inside`] found it
/// ([`twr_core::git::mount_refusal`]).
pub(super) fn mount_refusal(
    mounts: &Option<Result<Vec<String>, String>>,
    whose: &str,
) -> Option<String> {
    match mounts.as_ref()? {
        Ok(m) => twr_core::git::mount_refusal(whose, m, false),
        Err(e) => {
            tracing::warn!(error = %e, "cannot tell where file systems are mounted");
            twr_core::git::mount_refusal(whose, &[], true)
        }
    }
}

/// What this process can tell of where file systems are mounted ([`mount_list`]).
#[derive(Debug, Clone)]
pub(super) enum MountList {
    /// Linux's `/proc/self/mountinfo`, read.
    // Made only on Linux, and by tests.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    Read(Vec<u8>),
    /// The mount points, listed (macOS's `getfsstat(2)`, [`mount_names`]).
    // Made only on macOS, and by tests.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    Points(Vec<PathBuf>),
    /// It cannot be read.
    Unreadable,
    /// No list on this system (any but Linux and macOS).
    // Made only there, and by tests.
    #[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
    None,
}

/// Whether this system has a mount list ([`mount_list`]).
const HAS_MOUNT_LIST: bool = cfg!(any(target_os = "linux", target_os = "macos"));

/// Where [`mount_list`] reads it, for an error.
const MOUNT_SOURCE: &str = if cfg!(target_os = "macos") {
    "getfsstat"
} else {
    "/proc/self/mountinfo"
};

#[cfg(test)]
/// What [`mount_list`] gives instead for a work tree under each path (tests).
pub(super) static MOUNTINFO: std::sync::Mutex<Vec<(PathBuf, MountList)>> =
    std::sync::Mutex::new(Vec::new());

/// The mount list for the work tree at `real`: this process's `/proc/self/mountinfo` on Linux
/// (a regular file of at most 16 MiB), every mounted file system's mount point on macOS
/// ([`getfsstat_points`]), [`MountList::None`] elsewhere. The one place that decides whether
/// mount points can be told, which the inventory, the deletion and a trim all go by.
fn mount_list(real: &Path) -> MountList {
    #[cfg(test)]
    if let Some((_, list)) = MOUNTINFO
        .lock()
        .unwrap()
        .iter()
        .find(|(under, _)| real.starts_with(under))
    {
        return list.clone();
    }
    #[cfg(not(test))]
    let _ = real;
    #[cfg(target_os = "linux")]
    {
        use std::io::Read;
        const MAX: u64 = 16 << 20;
        let read = || -> Option<Vec<u8>> {
            let f = std::fs::File::open("/proc/self/mountinfo").ok()?;
            if !f.metadata().ok()?.is_file() {
                return None;
            }
            let mut text = Vec::new();
            f.take(MAX + 1).read_to_end(&mut text).ok()?;
            (text.len() as u64 <= MAX).then_some(text)
        };
        read().map_or(MountList::Unreadable, MountList::Read)
    }
    #[cfg(target_os = "macos")]
    {
        getfsstat_points().map_or(MountList::Unreadable, MountList::Points)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        MountList::None
    }
}

/// Every mounted file system's mount point, by `getfsstat(2)` (`MNT_NOWAIT`: as the kernel
/// last knew them, no file system asked); `None` when it fails, or when the list keeps
/// growing past the room made for it (a mount point missed could be one in the worktree).
#[cfg(target_os = "macos")]
fn getfsstat_points() -> Option<Vec<PathBuf>> {
    // SAFETY: a null buffer of size 0 only asks how many there are.
    let count = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    let mut room = usize::try_from(count).ok()?.saturating_add(16);
    for _ in 0..4 {
        // SAFETY: `statfs` is plain integers and arrays, for which all zeroes is a value.
        let mut buf: Vec<libc::statfs> = std::iter::repeat_with(|| unsafe { std::mem::zeroed() })
            .take(room)
            .collect();
        let size =
            std::ffi::c_int::try_from(room.checked_mul(std::mem::size_of::<libc::statfs>())?)
                .ok()?;
        // SAFETY: `buf` holds `room` structures, `size` bytes, which is all the call writes.
        let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), size, libc::MNT_NOWAIT) };
        let got = usize::try_from(got).ok()?;
        if got < room {
            buf.truncate(got);
            return Some(mount_names(buf.iter().map(|s| &s.f_mntonname[..])));
        }
        // Full: there may be more than it could hold. Again with more room.
        room = room.saturating_mul(2);
    }
    None
}

/// The mount points named by `names`, each a fixed-size C string buffer (`statfs`'s
/// `f_mntonname`): its bytes up to the first NUL (all of them without one), as a path; one
/// that is not absolute is left out, as [`mount_points`] leaves it out.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(super) fn mount_names<'a>(
    names: impl IntoIterator<Item = &'a [std::ffi::c_char]>,
) -> Vec<PathBuf> {
    names
        .into_iter()
        .map(|name| {
            name.iter()
                .map(|c| *c as u8)
                .take_while(|b| *b != 0)
                .collect::<Vec<u8>>()
        })
        .filter(|p| p.starts_with(b"/"))
        .map(|p| path_of(&p))
        .collect()
}

/// The mount points `mountinfo` (as `/proc/<pid>/mountinfo` reads) lists: the fifth field of
/// each line, its octal escapes (`\040` for a space, `\011`, `\012`, `\134`) decoded.
pub(super) fn mount_points(mountinfo: &[u8]) -> Vec<PathBuf> {
    mountinfo
        .split(|b| *b == b'\n')
        .filter_map(|line| line.split(|b| *b == b' ').nth(4))
        .filter(|p| p.starts_with(b"/"))
        .map(|p| {
            let mut out = Vec::with_capacity(p.len());
            let mut i = 0;
            while i < p.len() {
                let octal = p
                    .get(i + 1..i + 4)
                    .filter(|d| p[i] == b'\\' && d.iter().all(|b| (b'0'..=b'7').contains(b)))
                    .map(|d| d.iter().fold(0u32, |n, b| n * 8 + u32::from(b - b'0')));
                match octal {
                    Some(n) if n <= 0xff => {
                        out.push(n as u8);
                        i += 4;
                    }
                    _ => {
                        out.push(p[i]);
                        i += 1;
                    }
                }
            }
            path_of(&out)
        })
        .collect()
}

/// Whether the directory `rel` (a raw path under `root`) holds a git repository, or may: it is
/// one or what is left of one ([`repository_at`]), it has a `.git`, or a search finds one
/// under it ([`repositories_in`], within `budget`; one not searched to the end may).
fn may_hold_repository(root: &Path, rel: &[u8], budget: &mut u64) -> bool {
    let at = root.join(path_of(rel));
    if may_exist(&at.join(".git")) || !matches!(repository_at(&at), Ok(false)) {
        return true;
    }
    let (found, cut) = repositories_in(root, rel, &|_| false, budget);
    !(found.is_empty() && cut.is_empty())
}

/// Name of the directory of the work tree that build output is moved into to be deleted
/// ([`drop_build_output`]), the first free of `.twr-build-output.<n>`. Its [`OWNER`] names the
/// trash item of the deletion, written before anything moves in. An entry there is `<slot>`,
/// and `<slot>.from` names where it was (as `git status` names it), written before it moves:
/// for a person (a restore goes by the item's manifest, [`put_back_aside`]).
pub(super) const ASIDE: &str = ".twr-build-output";

/// The file of an [`ASIDE`] directory naming the trash item whose deletion made it.
const OWNER: &str = "owner";

/// The file of a worktree's git record where a deletion writes down each entry of its build
/// output before it moves it aside, and which ones are still there ([`Journal`]): what a
/// deletion stopped part way (the hub or runner gone) left aside is found there by the next one
/// ([`reconcile_staged`]), and nowhere else (nothing in the work tree counts). It moves into the
/// trash item with the record.
pub(super) const JOURNAL: &str = "twr-build-output";

/// What [`JOURNAL`] holds.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Journal {
    /// The trash item whose deletion moved them or took them over ([`OWNER`] of their
    /// directories).
    item: String,
    /// The items that owned them before, while a takeover by `item` may not have rewritten
    /// every [`OWNER`] yet ([`reconcile_staged`]): any of them names them too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    previous: Vec<String>,
    /// The entries moved aside and not deleted or put back (yet).
    staged: Vec<KeptAside>,
}

/// Writes `j` to the journal at `path` ([`JOURNAL`]) whole, relative to its directory held
/// open ([`Dir::replace`]: a new file renamed over it; nothing there is followed or opened), or
/// removes it when nothing is staged.
fn write_journal(path: &Path, j: &Journal) -> std::io::Result<()> {
    let (Some(record), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other("no journal path"));
    };
    let record = Dir::open(record)?;
    if j.staged.is_empty() {
        return match record.unlink(name) {
            Err(e) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    record.replace(name, &serde_json::to_vec(j)?)
}

/// What [`drop_build_output`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Dropped {
    /// The entries deleted.
    pub(super) deleted: Vec<String>,
    /// The entries not deleted that could not go back where they were either (their place was
    /// taken meanwhile): they stay in `<ASIDE>.<n>/<slot>` ([`put_back_aside`] puts them back).
    pub(super) aside: Vec<KeptAside>,
    /// The entries not deleted that stayed where they were (or went back), with why: what a
    /// trim says it left.
    pub(super) skipped: Vec<(String, String)>,
}

/// Deletes the build output `build` of the work tree at `root` (ignored entries of `[threads]
/// park_disposable`, as `git status` names them; [`build_output`] for the inventory). Each is
/// checked again as it goes: reached from `root` through real directories only, searched for
/// git repositories where it is (within `budget` entries; one that holds one stays), written
/// down in `journal` ([`JOURNAL`]) and moved into a private directory of `root`, and deleted
/// entry by entry relative to the directories held ([`dirfd`]): no symlink is followed, and it
/// stops at anything that looks like a repository ([`empty`]). One that cannot be deleted whole
/// goes back where it was (what is left of it), never over anything there now: when something
/// is, it stays aside ([`Dropped::aside`], with `kept`, what [`reconcile_staged`] took over),
/// and the move to the trash takes it with the rest. The journal keeps those.
///
/// That is safe against what is there when it runs, not against another process writing into
/// the work tree meanwhile (see [`dirfd`]); twr has stopped the thread's runtime by then.
/// `owner`: the trash item ([`OWNER`]). `hook` runs at the steps of each ([`Step`]; tests): an
/// error it returns stops everything there, as if the process had (what is aside stays, and so
/// does the journal), and is returned.
#[allow(clippy::too_many_arguments)]
pub(super) fn drop_build_output(
    root: &Path,
    build: &[String],
    owner: &str,
    journal: &Path,
    kept: Vec<KeptAside>,
    budget: &mut u64,
    hook: &dyn Fn(Step, &Path) -> std::io::Result<()>,
) -> std::io::Result<Dropped> {
    let warn = |e: &dyn std::fmt::Display| {
        tracing::warn!(path = %root.display(), error = %e, "cannot delete the worktree's build output; it goes to the trash");
    };
    let mut j = Journal {
        item: owner.to_string(),
        previous: Vec::new(),
        staged: kept,
    };
    let mut out = Dropped::default();
    let done = |out: Dropped, j: Journal| Dropped {
        aside: j.staged,
        ..out
    };
    // Nothing deleted, and why (for a trim to say).
    let none = |why: &dyn std::fmt::Display| -> Vec<(String, String)> {
        build.iter().map(|p| (p.clone(), why.to_string())).collect()
    };
    if build.is_empty() {
        return Ok(done(out, j));
    }
    let top = match Dir::open(root) {
        Ok(d) => d,
        Err(e) => {
            warn(&e);
            out.skipped = none(&e);
            return Ok(done(out, j));
        }
    };
    let mut made = None;
    for n in 0..100 {
        let name = OsString::from(format!("{ASIDE}.{n}"));
        match top.make_dir(&name, 0o700) {
            Ok(()) => {
                made = Some(name);
                break;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                warn(&e);
                out.skipped = none(&e);
                return Ok(done(out, j));
            }
        }
    }
    let Some(name) = made else {
        let why = format!("no free {ASIDE}.<n> in the worktree");
        warn(&why);
        out.skipped = none(&why);
        return Ok(done(out, j));
    };
    let aside = match top.dir(&name) {
        Ok(d) => d,
        Err(e) => {
            warn(&e);
            out.skipped = none(&e);
            return Ok(done(out, j));
        }
    };
    if let Err(e) = aside.write_new(OsStr::new(OWNER), owner.as_bytes()) {
        warn(&e);
        out.skipped = none(&e);
        let _ = aside.unlink(OsStr::new(OWNER));
        let _ = top.remove_dir(&name);
        return Ok(done(out, j));
    }
    let mut kept_here = false;
    for (i, p) in build.iter().enumerate() {
        let slot = OsString::from(i.to_string());
        let k = KeptAside {
            path: p.clone(),
            at: format!("{}/{}", name.to_string_lossy(), slot.to_string_lossy()),
        };
        match drop_entry(&top, &aside, &slot, &k, &mut j, journal, budget, hook) {
            Ok(()) => out.deleted.push(p.clone()),
            Err(NotDropped { stop: Some(e), .. }) => return Err(e),
            Err(NotDropped {
                why, aside: false, ..
            }) => {
                tracing::warn!(path = %root.join(path_of(&unquote_path(p.trim_end_matches('/')))).display(), error = %format!("{why:#}"), "build output not deleted; what is left of it goes to the trash");
                out.skipped.push((p.clone(), format!("{why:#}")));
            }
            Err(NotDropped { why, .. }) => {
                tracing::warn!(path = %root.join(&k.at).display(), entry = %p, error = %format!("{why:#}"), "build output neither deleted nor put back; it goes to the trash apart, and a restore puts it back");
                kept_here = true;
            }
        }
    }
    if !kept_here {
        let _ = aside.unlink(OsStr::new(OWNER));
        if let Err(e) = top.remove_dir(&name) {
            tracing::warn!(path = %root.join(&name).display(), error = %e, "cannot remove the directory build output was moved into; it goes to the trash with the worktree");
        }
    }
    Ok(done(out, j))
}

/// Why [`drop_entry`] did not delete an entry: whether what is left of it stayed aside (it
/// could not go back), or everything stops (the test hook's error).
struct NotDropped {
    why: anyhow::Error,
    aside: bool,
    stop: Option<std::io::Error>,
}

impl<E: Into<anyhow::Error>> From<E> for NotDropped {
    fn from(e: E) -> Self {
        NotDropped {
            why: e.into(),
            aside: false,
            stop: None,
        }
    }
}

/// [`NotDropped`] that stops everything with `e`.
fn stop(e: std::io::Error) -> NotDropped {
    NotDropped {
        why: anyhow!("stopped"),
        aside: false,
        stop: Some(e),
    }
}

/// Deletes build output entry `k.path` of the work tree held as `top` ([`drop_build_output`]),
/// moving it into `aside` as `slot` (`k.at`) first, written down in the journal `j` (at
/// `journal`) and as `<slot>.from` before: an error when it is not deleted, what is left of it
/// put back if nothing took its place. The journal keeps it only while it is aside.
#[allow(clippy::too_many_arguments)]
fn drop_entry(
    top: &Dir,
    aside: &Dir,
    slot: &OsStr,
    k: &KeptAside,
    j: &mut Journal,
    journal: &Path,
    budget: &mut u64,
    hook: &dyn Fn(Step, &Path) -> std::io::Result<()>,
) -> Result<(), NotDropped> {
    let entry = k.path.as_str();
    let rel = unquote_path(entry.trim_end_matches('/'));
    let comps: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    let Some((last, parents)) = comps.split_last() else {
        return Err(anyhow!("no path").into());
    };
    let mut held: Option<Dir> = None;
    for c in parents {
        let next = held.as_ref().unwrap_or(top).dir(OsStr::from_bytes(c))?;
        held = Some(next);
    }
    let parent = held.as_ref().unwrap_or(top);
    let name = OsStr::from_bytes(last);
    if !entry.ends_with('/') {
        // A file (or a symlink itself): it holds nothing, and a directory is not unlinked.
        return Ok(parent.unlink(name)?);
    }
    let dir = parent.dir(name)?;
    let meta = dir.file().metadata()?;
    let mode = meta.permissions().mode();
    let at = top.shown().join(path_of(&rel));
    // What holds a repository is never moved.
    if may_hold_repository(&top.path(), &rel, budget) {
        return Err(anyhow!("it holds a git repository").into());
    }
    hook(Step::BuildOutputOpened, &at).map_err(stop)?;
    // Where it was, before it moves: in the journal (what the next deletion goes by), and
    // beside it for a person.
    j.staged.push(k.clone());
    if let Err(e) = write_journal(journal, j) {
        j.staged.pop();
        return Err(e.into());
    }
    let unstage = |j: &mut Journal| {
        j.staged.retain(|s| s != k);
        if let Err(e) = write_journal(journal, j) {
            tracing::warn!(journal = %journal.display(), error = %e, "cannot update the journal of build output moved aside");
        }
    };
    let mut from = slot.to_os_string();
    from.push(".from");
    let written = aside.write_new(&from, entry.as_bytes());
    // A directory moved to another one must be writable (its `..` changes).
    let opened = written.and_then(|()| {
        if mode & 0o700 != 0o700 {
            dir.file()
                .set_permissions(std::fs::Permissions::from_mode(mode | 0o700))?;
        }
        parent.rename_new(name, aside, slot)
    });
    if let Err(e) = opened {
        let _ = dir
            .file()
            .set_permissions(std::fs::Permissions::from_mode(mode));
        let _ = aside.unlink(&from);
        unstage(j);
        return Err(e.into());
    }
    let back = |why: anyhow::Error, j: &mut Journal| -> NotDropped {
        if let Err(e) = hook(Step::BuildOutputPuttingBack, &at) {
            return stop(e);
        }
        match aside.rename_new(slot, parent, name) {
            Ok(()) => {
                let _ = dir
                    .file()
                    .set_permissions(std::fs::Permissions::from_mode(mode));
                let _ = aside.unlink(&from);
                unstage(j);
                NotDropped::from(why)
            }
            Err(e) => NotDropped {
                why: why.context(format!(
                    "and it could not go back to {} ({e})",
                    at.display()
                )),
                aside: true,
                stop: None,
            },
        }
    };
    let moved = match aside.dir(slot) {
        Ok(d) => d,
        Err(e) => return Err(back(e.into(), j)),
    };
    let same = moved
        .file()
        .metadata()
        .is_ok_and(|m| (m.dev(), m.ino()) == (meta.dev(), meta.ino()));
    if !same {
        return Err(back(anyhow!("it changed as it was moved aside"), j));
    }
    hook(Step::BuildOutputSearched, &aside.path().join(slot)).map_err(stop)?;
    if let Err(e) = empty(&moved, meta.dev()).and_then(|()| aside.remove_dir(slot)) {
        return Err(back(e.into(), j));
    }
    let _ = aside.unlink(&from);
    unstage(j);
    Ok(())
}

/// What a deletion of the work tree at `root` stopped part way (the hub or runner gone) left
/// aside, as its `journal` ([`JOURNAL`]) records it, and nothing else: each entry goes back where
/// it was ([`put_back`]), or, when it cannot, is taken over by trash item `item` and returned,
/// to be kept with it. Only an entry proven gone (deleted before the stop) leaves the journal;
/// an error reading or writing what it goes by fails this, the journal as it was or naming
/// `item`, so that the next one starts over.
///
/// The takeover can stop anywhere and be taken up again: the journal names `item` first, with
/// the items that owned the entries before as `previous`, and only then is each of their
/// directories' [`OWNER`] rewritten (once per directory), so an owner naming any of them is
/// taken. One naming another item is left as it is (the entry is kept all the same; a restore
/// will not take it). `hook` runs before and after the rewrites (tests; an error stops there).
pub(super) fn reconcile_staged(
    root: &Path,
    journal: &Path,
    item: &str,
    hook: &dyn Fn(Step, &Path) -> std::io::Result<()>,
) -> anyhow::Result<Vec<KeptAside>> {
    let record = journal.parent().context("no record")?;
    let raw = match Dir::open(record)?.read_small(OsStr::new(JOURNAL), 1 << 20) {
        Ok(raw) => raw,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let was: Journal = serde_json::from_slice(&raw).context("reading the journal")?;
    let mut owners: Vec<&str> = std::iter::once(was.item.as_str())
        .chain(was.previous.iter().map(String::as_str))
        .collect();
    owners.dedup();
    let top = Dir::open(root)?;
    let mut carried = Vec::new();
    let mut dirs = BTreeSet::new();
    for k in was.staged {
        let dir = k.at.split_once('/').map_or(k.at.as_str(), |(d, _)| d);
        dirs.insert(dir.to_string());
        let why = match put_back(&top, &owners, &k) {
            Ok(_) => continue,
            Err(e) => e,
        };
        // Deleted before the stop: nothing to keep. Anything else (or what cannot be told)
        // is kept.
        if matches!(presence(&top.path().join(&k.at)), Presence::Absent) {
            continue;
        }
        tracing::warn!(path = %root.join(&k.at).display(), entry = %k.path, error = %format!("{why:#}"), "build output a stopped deletion left aside cannot go back; it goes to the trash apart, and a restore puts it back");
        carried.push(k);
    }
    let mut j = Journal {
        item: item.to_string(),
        previous: owners
            .iter()
            .filter(|o| **o != item)
            .map(|o| o.to_string())
            .collect(),
        staged: carried.clone(),
    };
    write_journal(journal, &j).context("writing the journal")?;
    hook(Step::BuildOutputTakingOver, root)?;
    let taken: BTreeSet<&str> = carried
        .iter()
        .map(|k| k.at.split_once('/').map_or(k.at.as_str(), |(d, _)| d))
        .collect();
    for dir in taken {
        let aside = top
            .dir(OsStr::new(dir))
            .with_context(|| format!("opening {dir}"))?;
        let owner = aside
            .read_small(OsStr::new(OWNER), 64)
            .with_context(|| format!("reading the owner of {dir}"))?;
        let owner = trim_newline(&owner);
        if owner == item.as_bytes() {
            continue;
        }
        if !owners.iter().any(|o| o.as_bytes() == owner) {
            tracing::warn!(path = %root.join(dir).display(), "build output kept aside by another item; its owner stays");
            continue;
        }
        aside
            .replace(OsStr::new(OWNER), item.as_bytes())
            .with_context(|| format!("writing the owner of {dir}"))?;
    }
    hook(Step::BuildOutputTakenOver, root)?;
    drop_emptied(&top, dirs);
    if !j.previous.is_empty() {
        j.previous.clear();
        write_journal(journal, &j).context("writing the journal")?;
    }
    Ok(carried)
}

/// Removes each of the [`ASIDE`] directories `dirs` of the work tree held as `top` that holds
/// nothing but its [`OWNER`].
fn drop_emptied(top: &Dir, dirs: BTreeSet<String>) {
    for dir in dirs {
        if let Ok(aside) = top.dir(OsStr::new(&dir)) {
            if aside.names().is_ok_and(|n| n.iter().all(|n| n == OWNER)) {
                let _ = aside.unlink(OsStr::new(OWNER));
                let _ = top.remove_dir(OsStr::new(&dir));
            }
        }
    }
}

/// Puts the build output trash item `item` kept aside (`kept`, [`Dropped::aside`] as its
/// manifest has it) back where it was in the work tree at `root` (a restored worktree), and
/// nothing else: only from an [`ASIDE`] directory whose [`OWNER`] names `item`, only where every
/// directory on the way is a real one (made when missing), never over anything there. A note
/// for each that stays where it is.
pub(super) fn put_back_aside(root: &Path, item: &str, kept: &[KeptAside]) -> Vec<String> {
    let mut notes = Vec::new();
    if kept.is_empty() {
        return notes;
    }
    let top = match Dir::open(root) {
        Ok(d) => d,
        Err(e) => {
            notes.push(format!(
                "its build output kept aside ({}) stays where it is: {e}",
                kept.iter()
                    .map(|k| k.at.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
            return notes;
        }
    };
    let mut dirs = BTreeSet::new();
    for k in kept {
        match put_back(&top, &[item], k) {
            Ok(dir) => {
                dirs.insert(dir);
            }
            Err(e) => notes.push(format!(
                "its build output {} could not be deleted when it was deleted, and cannot go \
                 back where it was ({e:#}): it is at {}",
                k.path, k.at
            )),
        }
    }
    // An aside directory emptied goes (a slot left keeps it, with its owner).
    drop_emptied(&top, dirs);
    notes
}

/// Moves `k` back where it was in the work tree held as `top` ([`put_back_aside`]): the name of
/// its aside directory.
fn put_back(top: &Dir, owners: &[&str], k: &KeptAside) -> anyhow::Result<String> {
    let (dir, slot) =
        k.at.split_once('/')
            .filter(|(d, s)| {
                let digits = |x: &str| !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit());
                d.strip_prefix(ASIDE)
                    .and_then(|n| n.strip_prefix('.'))
                    .is_some_and(digits)
                    && digits(s)
            })
            .ok_or_else(|| anyhow!("{:?} is not where build output is kept", k.at))?;
    let rel = unquote_path(k.path.trim_end_matches('/'));
    let comps: Vec<&[u8]> = rel.split(|b| *b == b'/').collect();
    if comps
        .iter()
        .any(|c| c.is_empty() || *c == b"." || *c == b"..")
    {
        bail!("{:?} is no path in it", k.path);
    }
    let aside = top.dir(OsStr::new(dir))?;
    let owner = aside.read_small(OsStr::new(OWNER), 64)?;
    if !owners.iter().any(|o| trim_newline(&owner) == o.as_bytes()) {
        bail!("{dir} was not made by the deletion of this item");
    }
    let (last, parents) = comps.split_last().context("no path")?;
    let mut held: Option<Dir> = None;
    for c in parents {
        let at = held.as_ref().unwrap_or(top);
        let name = OsStr::from_bytes(c);
        let next = match at.dir(name) {
            Err(e) if e.kind() == ErrorKind::NotFound => {
                at.make_dir(name, 0o777)?;
                at.dir(name)?
            }
            Err(e) if dirfd::not_a_dir(&e) => bail!(
                "{} on the way is not a directory (a symlink is not followed)",
                quote_path(c)
            ),
            r => r?,
        };
        held = Some(next);
    }
    let parent = held.as_ref().unwrap_or(top);
    aside
        .rename_new(OsStr::new(slot), parent, OsStr::from_bytes(last))
        .map_err(|e| match e.kind() {
            ErrorKind::AlreadyExists => anyhow!("something is there"),
            _ => e.into(),
        })?;
    let _ = aside.unlink(OsStr::new(&format!("{slot}.from")));
    Ok(dir.to_string())
}

/// Deletes everything in the directory `dir`, relative to the directories held, no symlink
/// followed, each directory made readable, writable and searchable by its owner first (a
/// read-only one of a Go module cache would stop it). Stops (an error) at a directory that
/// looks like a git repository before anything of it goes: one with a `.git`, or with
/// `objects` and `HEAD` or `refs` (what [`repository_at`] takes for one, whatever their kind),
/// and at a directory on another file system than `dev` (a mount point; a work tree with one is
/// not deleted at all where they can be told, [`mounts_inside`], but a bind mount of the same
/// file system is no other file system).
fn empty(dir: &Dir, dev: u64) -> std::io::Result<()> {
    let names = dir.names()?;
    let has = |n: &str| names.iter().any(|x| x == n);
    if has(".git") || (has("objects") && (has("HEAD") || has("refs"))) {
        return Err(std::io::Error::other(format!(
            "{} looks like a git repository",
            dir.shown().display()
        )));
    }
    for name in &names {
        let unlinked = match dir.unlink(name) {
            Ok(()) => continue,
            Err(e) if is_absent(&e) => continue,
            Err(e) => e,
        };
        let sub = match dir.dir(name) {
            Ok(d) => d,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                dir.open_up(name)?;
                dir.dir(name)?
            }
            Err(e) if dirfd::not_a_dir(&e) => return Err(unlinked),
            Err(e) => return Err(e),
        };
        let meta = sub.file().metadata()?;
        if meta.dev() != dev {
            return Err(std::io::Error::other(format!(
                "{} is another file system (a mount point)",
                sub.shown().display()
            )));
        }
        let mode = meta.permissions().mode();
        if mode & 0o700 != 0o700 {
            sub.file()
                .set_permissions(std::fs::Permissions::from_mode(mode | 0o700))?;
        }
        empty(&sub, dev)?;
        dir.remove_dir(name)?;
    }
    Ok(())
}

/// The file at `path`'s raw contents, `None` when it is proven absent.
fn read_optional(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if is_absent(&e) => Ok(None),
        Err(e) => Err(anyhow!("reading {}: {e}", path.display())),
    }
}

/// Copies the file `from` (when it is there) to `to`, which must not exist (its directories
/// made). Returns whether it copied.
fn copy_reflog(from: &Path, to: &Path) -> anyhow::Result<bool> {
    let Some(bytes) = read_optional(from)? else {
        return Ok(false);
    };
    if !matches!(presence(to), Presence::Absent) {
        bail!("the reflog {} exists already", to.display());
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(to, bytes).with_context(|| format!("writing {}", to.display()))?;
    Ok(true)
}

/// Whether every `/`-separated component of `branch` is a valid one.
fn valid_branch(branch: &str) -> bool {
    !branch.starts_with('-') && branch.split('/').all(twr_core::git::valid_branch_component)
}

/// Where the branch `branch` is checked out in the repository whose git common directory is
/// `common` (main checkout `repo`): its main checkout, or a worktree whose record is not
/// `except`. `None` when it is checked out nowhere.
fn checked_out(
    common: &Path,
    repo: &Path,
    branch: &str,
    except: Option<&Path>,
) -> anyhow::Result<Option<PathBuf>> {
    let want = format!("ref: refs/heads/{branch}");
    let mut heads = vec![(common.join("HEAD"), repo.to_path_buf())];
    for r in worktree_records(common)? {
        if Some(r.admin.as_path()) != except {
            heads.push((
                r.admin.join("HEAD"),
                r.worktree.clone().unwrap_or_else(|_| r.admin.clone()),
            ));
        }
    }
    for (head, wt) in heads {
        if read_optional(&head)?.is_some_and(|h| trim_newline(&h) == want.as_bytes()) {
            return Ok(Some(wt));
        }
    }
    Ok(None)
}

/// The age of the item `dir`'s manifest (since it last changed); `None` when it cannot be
/// told.
fn manifest_age(dir: &Path) -> Option<Duration> {
    std::fs::metadata(dir.join(MANIFEST))
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
}

/// `O_NOFOLLOW | O_NONBLOCK` for opening a saved file to copy it (no `libc` but macOS's mount
/// list, so the values are spelled out per OS and architecture, as in `twr-hub`'s context
/// files): a symlink swapped in after the `symlink_metadata` check fails to open, and a FIFO
/// does not block.
/// Where they are not spelled out, the check and the post-open `fstat` (the opened file must
/// be the one checked) stand alone.
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        target_arch = "riscv64",
        target_arch = "loongarch64"
    )
))]
const OPEN_FLAGS: i32 = 0o400000 | 0o4000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const OPEN_FLAGS: i32 = 0o100000 | 0o4000;
#[cfg(target_os = "macos")]
const OPEN_FLAGS: i32 = 0x0100 | 0x0004;
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
const OPEN_FLAGS: i32 = 0;

/// The mark a restore leaves in the record of the worktree it made ([`RESTORE_MARK`]).
pub(super) struct Mark {
    item: String,
    /// The branch the restore created: the thread's recorded branch.
    pub(super) branch: String,
    /// The commit the restore created the branch at.
    target: String,
}

/// The mark of a restore in the worktree record `admin`, if it has a valid one.
pub(super) fn read_mark(admin: &Path) -> Option<Mark> {
    let file = admin.join(RESTORE_MARK);
    if !std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
        return None;
    }
    let text = String::from_utf8(std::fs::read(&file).ok()?).ok()?;
    let mut lines = text.lines();
    let (item, branch, target) = (lines.next()?, lines.next()?, lines.next()?);
    (valid_trash_id(item) && valid_branch(branch) && object_id(target.as_bytes())).then(|| Mark {
        item: item.to_string(),
        branch: branch.to_string(),
        target: target.to_string(),
    })
}

/// The worktree a restore of the item `m` made, by the mark in its record: (record,
/// worktree, mark). Only one whose record names its worktree.
fn restored_copy(m: &TrashManifest) -> Option<(PathBuf, PathBuf, Mark)> {
    let records = worktree_records(&m.common).ok()?;
    records.into_iter().find_map(|r| {
        let mark = read_mark(&r.admin).filter(|k| k.item == m.item)?;
        Some((r.admin, r.worktree.ok()?, mark))
    })
}

/// Whether the directory `wt` is the linked worktree of the record `admin`: `wt` is a
/// directory (no symlink) whose `.git` file (no symlink) names it ([`dot_git`]).
fn linked_to(wt: &Path, admin: &Path) -> bool {
    if !std::fs::symlink_metadata(wt).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let DotGit::Names(named) = dot_git(wt) else {
        return false;
    };
    matches!(
        (resolve_lenient(&named), resolve_lenient(admin)),
        (Some(a), Some(b)) if a == b
    )
}

/// Copies the file `from` to `to` when `to` does not exist yet (its directories made; never
/// through a symlink at `to`): whether it did. A copy cut short is removed.
fn copy_if_absent(from: &Path, to: &Path) -> anyhow::Result<bool> {
    let Some(bytes) = read_optional(from)? else {
        return Ok(false);
    };
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
    {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(anyhow!("creating {}: {e}", to.display())),
    };
    if let Err(e) = file.write_all(&bytes) {
        let _ = std::fs::remove_file(to);
        bail!("writing {}: {e}", to.display());
    }
    Ok(true)
}

/// Whether `rel` is in the tree `root`, every directory on the way a directory (not a
/// symlink to one).
fn tree_has(root: &Path, rel: &Path) -> std::io::Result<bool> {
    let mut at = root.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(part) = parts.next() {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if parts.peek().is_some() && !m.is_dir() => return Ok(false),
            Ok(_) => {}
            Err(e) if is_absent(&e) => return Ok(false),
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Removes what is at `path` (a file or symlink; a directory with everything in it), if
/// anything.
fn clear(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => remove_tree(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if is_absent(&e) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copies the regular file `from` (whose `symlink_metadata` is `meta`) to `to`, which must
/// not exist, with its permissions; the file opened must be the one looked at.
fn copy_file(from: &Path, to: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    let mut src = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(OPEN_FLAGS)
        .open(from)?;
    let opened = src.metadata()?;
    if !opened.is_file() || (opened.dev(), opened.ino()) != (meta.dev(), meta.ino()) {
        return Err(std::io::Error::other(format!(
            "{} changed while it was copied",
            from.display()
        )));
    }
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    let n = std::io::copy(&mut src, &mut dst)?;
    if n != meta.len() {
        return Err(std::io::Error::other(format!(
            "{} changed while it was copied ({n} of {} bytes)",
            from.display(),
            meta.len()
        )));
    }
    dst.set_permissions(meta.permissions())
}

/// Copies the worktree files `saved` (a trash item's `worktree/`) into the new worktree
/// `into` (see [`LocalGit::restore`]), deleting nothing outside it and following no
/// symlink: first the files `tracked` (`git ls-files -z` of `into`) names that the saved tree
/// does not have go from `into` (they were deleted, or replaced by something else there);
/// then the saved tree is copied over `into` ([`overlay`]). Returns what it did not copy
/// (sockets, FIFOs, devices), relative to the worktree.
fn copy_saved(saved: &Path, into: &Path, tracked: &[u8]) -> std::io::Result<Vec<PathBuf>> {
    if !std::fs::symlink_metadata(saved)?.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            saved.display()
        )));
    }
    for raw in tracked.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let rel = Path::new(std::ffi::OsStr::from_bytes(raw));
        if !rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
            || tree_has(saved, rel)?
        {
            continue;
        }
        remove_checked_out(saved, into, rel)?;
    }
    let mut skipped = Vec::new();
    overlay(saved, into, Path::new(""), &mut skipped)?;
    Ok(skipped)
}

/// Removes the checked-out file `rel` from the new worktree `into` (a directory, as a
/// submodule's empty one, only when empty), then each directory above it that is empty and
/// that the saved tree `saved` does not have. Nothing when a directory on the way is not one.
fn remove_checked_out(saved: &Path, into: &Path, rel: &Path) -> std::io::Result<()> {
    let mut at = into.to_path_buf();
    let mut parts = rel.components().peekable();
    while let Some(part) = parts.next() {
        at.push(part);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(m) => m,
            Err(e) if is_absent(&e) => return Ok(()),
            Err(e) => return Err(e),
        };
        if parts.peek().is_some() {
            if !meta.is_dir() {
                return Ok(());
            }
        } else if meta.is_dir() {
            if std::fs::remove_dir(&at).is_err() {
                return Ok(());
            }
        } else {
            std::fs::remove_file(&at)?;
        }
    }
    let mut parent = rel.parent();
    while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
        if tree_has(saved, p)? || std::fs::remove_dir(into.join(p)).is_err() {
            break;
        }
        parent = p.parent();
    }
    Ok(())
}

/// Copies the directory `src` of the saved tree over the directory `dst` of the new worktree
/// (`rel` below its top; the top's `.git` skipped): directories made (what the checkout has in
/// the way removed) and given the saved permissions once filled, files and symlinks put in
/// place of what the checkout has there (symlinks as symlinks), special files skipped (noted
/// in `skipped`). Every entry is looked at with `symlink_metadata`, and a symlink of the
/// checkout in the way is removed, never written through.
fn overlay(src: &Path, dst: &Path, rel: &Path, skipped: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if rel.as_os_str().is_empty() && name == ".git" {
            continue;
        }
        let (from, to, r) = (entry.path(), dst.join(&name), rel.join(&name));
        let meta = std::fs::symlink_metadata(&from)?;
        let kind = meta.file_type();
        if kind.is_dir() {
            match std::fs::symlink_metadata(&to) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => {
                    std::fs::remove_file(&to)?;
                    std::fs::create_dir(&to)?;
                }
                Err(e) if is_absent(&e) => std::fs::create_dir(&to)?,
                Err(e) => return Err(e),
            }
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o700))?;
            overlay(&from, &to, &r, skipped)?;
            std::fs::set_permissions(&to, meta.permissions())?;
        } else if kind.is_symlink() {
            clear(&to)?;
            std::os::unix::fs::symlink(std::fs::read_link(&from)?, &to)?;
        } else if kind.is_file() {
            clear(&to)?;
            copy_file(&from, &to, &meta)?;
        } else {
            skipped.push(r);
        }
    }
    Ok(())
}

/// Copies the directory `from` to `to`, which must not exist (files, directories with their
/// permissions, symlinks as they are; special files skipped). A copy that fails removes what
/// it made.
fn copy_new(from: &Path, to: &Path) -> std::io::Result<()> {
    if !std::fs::symlink_metadata(from)?.is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a directory",
            from.display()
        )));
    }
    if !matches!(presence(to), Presence::Absent) {
        return Err(std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!("{} exists", to.display()),
        ));
    }
    let mut copied = Copied::default();
    let r = copy_tree(from, to, &mut copied, true);
    if r.is_err() && copied.made_root {
        let _ = remove_tree(to);
    }
    r
}

/// The git directories of submodules under `modules` (a restored record's `modules/`),
/// relative to its parent (`modules/<name>`, `modules/<name>/modules/<nested>`): each
/// directory holding a `HEAD` and a `config` file, found below it (a submodule's name can hold
/// slashes) and in its own `modules/` (nested submodules), no symlink followed. Anything that
/// could hide one is an error, as a submodule left out would be left naming the old record:
/// a symlink on the way (a folder of names, a repository's `modules/`), a `HEAD` or `config`
/// that is neither a regular file nor a folder (a symlink) or a regular one without the
/// other, a folder that cannot be listed, more than [`MODULES_MAX`] repositories. (A folder
/// named `HEAD` or `config` is part of a submodule's name, `group/config/child`.)
pub(super) fn module_repositories(modules: &Path) -> std::io::Result<Vec<PathBuf>> {
    let top = modules.parent().unwrap_or(modules);
    let err = |p: &Path, why: &str| std::io::Error::other(format!("{}: {why}", p.display()));
    let mut out = Vec::new();
    let mut todo = vec![modules.to_path_buf()];
    while let Some(dir) = todo.pop() {
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(err(&dir, "a symbolic link or no folder")),
            Err(e) => return Err(err(&dir, &e.to_string())),
        }
        // What `HEAD` and `config` are: a repository has both, regular files.
        // A folder of that name is part of a submodule's name (`group/config/child`).
        let marker = |n: &str| match std::fs::symlink_metadata(dir.join(n)) {
            Ok(m) if m.is_file() => Ok(true),
            Ok(m) if m.is_dir() => Ok(false),
            Ok(_) => Err(err(&dir.join(n), "not a regular file")),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
            Err(e) => Err(err(&dir.join(n), &e.to_string())),
        };
        let (head, config) = if dir == modules {
            (false, false)
        } else {
            (marker("HEAD")?, marker("config")?)
        };
        if head != config {
            return Err(err(&dir, "has a HEAD or a config file, not both"));
        }
        if head {
            if out.len() == MODULES_MAX {
                return Err(std::io::Error::other(format!(
                    "more than {MODULES_MAX} submodule repositories"
                )));
            }
            let nested = dir.join("modules");
            match std::fs::symlink_metadata(&nested) {
                Err(e) if e.kind() == ErrorKind::NotFound => {}
                // Checked when its turn comes (a symlink is an error).
                _ => todo.push(nested),
            }
            out.push(dir.strip_prefix(top).unwrap_or(&dir).to_path_buf());
            continue;
        }
        let listed = std::fs::read_dir(&dir).map_err(|e| err(&dir, &e.to_string()))?;
        for e in listed {
            let e = e?;
            let kind = e.file_type()?;
            if kind.is_dir() {
                todo.push(e.path());
            } else if kind.is_symlink() {
                return Err(err(&e.path(), "a symbolic link"));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The `.git` files of the worktree at `root` below its top (submodules, nested ones too):
/// each one's directory, relative to `root`, and the git directory it names. Walked through
/// directories held open from `root` down (a symlink is never followed, a `.git` directory
/// never entered), and each `.git` file read through its directory held ([`Dir::read_small`]:
/// a regular file of at most 64 KiB, not a symlink or a FIFO swapped in after the listing); a
/// folder that cannot be listed, a `.git` file that cannot be read, is larger or names none,
/// or more than [`twr_core::git::DISK_WALK_MAX`] entries, is an error.
pub(super) fn gitfiles(root: &Path) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
    let err = |p: &Path, why: &str| std::io::Error::other(format!("{}: {why}", p.display()));
    let top = Dir::open(root).map_err(|e| err(root, &e.to_string()))?;
    let mut out = Vec::new();
    let mut seen = 0u64;
    let mut todo = vec![PathBuf::new()];
    while let Some(rel) = todo.pop() {
        let shown = root.join(&rel);
        // The directory, reached from the top held one entry at a time.
        let below;
        let dir = if rel.as_os_str().is_empty() {
            &top
        } else {
            below = descend(&top, &rel)
                .map_err(|e| err(&shown, &format!("{e:#}")))?
                .ok_or_else(|| err(&shown, "went away"))?;
            &below
        };
        let listed = std::fs::read_dir(dir.path()).map_err(|e| err(&shown, &e.to_string()))?;
        for e in listed {
            let e = e?;
            seen += 1;
            if seen > twr_core::git::DISK_WALK_MAX {
                return Err(err(root, "too many entries to look for submodules in"));
            }
            let (name, kind) = (e.file_name(), e.file_type()?);
            if name == ".git" {
                if kind.is_file() && !rel.as_os_str().is_empty() {
                    let file = shown.join(&name);
                    let raw = dir
                        .read_small(&name, GITFILE_MAX)
                        .map_err(|x| err(&file, &x.to_string()))?;
                    let named = trim_newline(&raw)
                        .strip_prefix(b"gitdir: ")
                        .map(super::path_of)
                        .ok_or_else(|| err(&file, "names no git directory"))?;
                    out.push((rel.clone(), named));
                }
            } else if kind.is_dir() {
                todo.push(rel.join(&name));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Largest `.git` file read, in bytes: one names a git directory in a line.
const GITFILE_MAX: u64 = 1 << 16;

/// Most submodule repositories [`module_repositories`] finds in one record.
#[cfg(not(test))]
const MODULES_MAX: usize = 256;
#[cfg(test)]
const MODULES_MAX: usize = 8;

/// The directory `rel` (plain names) below the directory `top` held, opened one entry at a
/// time without following a symlink: `None` when one is missing; an error when one is a
/// symlink or no directory.
fn descend(top: &Dir, rel: &Path) -> anyhow::Result<Option<Dir>> {
    let mut parts = rel.components();
    let Some(first) = parts.next() else {
        bail!("no directory to open below {}", top.shown().display());
    };
    let open = |d: &Dir, part: std::path::Component<'_>| -> anyhow::Result<Option<Dir>> {
        match d.dir(part.as_os_str()) {
            Ok(d) => Ok(Some(d)),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) if dirfd::not_a_dir(&e) => bail!(
                "{} is a symbolic link or no directory",
                d.shown().join(part).display()
            ),
            Err(e) => Err(e.into()),
        }
    };
    let Some(mut at) = open(top, first)? else {
        return Ok(None);
    };
    for part in parts {
        match open(&at, part)? {
            Some(d) => at = d,
            None => return Ok(None),
        }
    }
    Ok(Some(at))
}

/// Whether the directories held `a` and `b` are one (device and inode).
fn same_dir(a: &Dir, b: &Dir) -> bool {
    match (a.file().metadata(), b.file().metadata()) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

/// `p` absolute with `.` and `..` taken away by its spelling alone (a `..` drops the part
/// before it, whatever it is on disk): for paths of places that are no more.
fn lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

/// Makes the `.git` file of the submodule directory held `tree` name the git directory
/// `repo` ([`Dir::replace`]: a new file of its own, renamed over it; a symlink there is
/// replaced, never written through).
fn write_gitfile(tree: &Dir, repo: &Path) -> std::io::Result<()> {
    let mut body = b"gitdir: ".to_vec();
    body.extend_from_slice(repo.as_os_str().as_bytes());
    body.push(b'\n');
    tree.replace(OsStr::new(".git"), &body)
}

/// What a restore made, so far, each noted only once it is there: what a failure (or the
/// hub, when it could not record the restore) removes again, and nothing else
/// ([`LocalGit::unmake`]).
#[derive(Debug, Default)]
struct Made {
    /// The branch's reflog file, written (`create_new`) from the item's copy.
    reflog: Option<PathBuf>,
    /// The branch and the commit it was created at (`update-ref create`).
    branch: Option<(String, String)>,
    /// The worktree's directory.
    claim: Option<Claim>,
    /// `git worktree add` succeeded in the claimed directory.
    added: bool,
    /// The worktree's record, the one git made for it ([`added_record`]) or the marked one
    /// (an undo).
    record: Option<PathBuf>,
}

impl Made {
    /// What a restore whose git was killed leaves, as far as it knows (`branch`: the branch it
    /// creates, which a killed `update-ref` may or may not have made).
    fn left(&self, branch: &str) -> String {
        let mut parts = Vec::new();
        if let Some(c) = &self.claim {
            parts.push(format!("the directory {}", c.path.display()));
        }
        if let Some(r) = &self.record {
            parts.push(format!("the worktree record {}", r.display()));
        }
        parts.push(match &self.branch {
            Some((b, _)) => format!("the branch {b}"),
            None => format!("the branch {branch}, if git made it"),
        });
        format!("{}; check them, and remove them by hand", parts.join(", "))
    }
}

/// The directory of a restore's worktree: made empty by the restore ([`claim_dir`]), or, for
/// an undo, proven the restore's by its `.git` naming the marked record.
#[derive(Debug)]
struct Claim {
    path: PathBuf,
    /// The device and inode of the directory the restore made at `path`; `None` for an undo.
    made: Option<(u64, u64)>,
    /// The missing directories above `path` the restore made, outermost first.
    parents: Vec<PathBuf>,
}

impl Claim {
    /// Whether what is at the path is still the directory the restore made (an undo: still
    /// the worktree whose `.git` names `record`).
    fn holds(&self, record: Option<&Path>) -> bool {
        match self.made {
            Some(id) => std::fs::symlink_metadata(&self.path)
                .is_ok_and(|m| m.is_dir() && (m.dev(), m.ino()) == id),
            None => record.is_some_and(|r| linked_to(&self.path, r)),
        }
    }

    /// Removes the directories above the path the restore made, innermost first, each only
    /// while empty (`rmdir`): what anyone else put in one keeps it.
    fn remove_parents(&self) {
        for p in self.parents.iter().rev() {
            if let Err(e) = std::fs::remove_dir(p) {
                tracing::debug!(dir = %p.display(), error = %e, "leaving a directory a restore made");
            }
        }
    }
}

/// Claims `path` for a restore's worktree: makes the missing directories above it (noting
/// those it made; one another process makes meanwhile is that one's), then `path` itself
/// with a single `mkdir`, which fails when anything is there (`AlreadyExists`). On failure
/// the directories it made above are removed again while empty.
fn claim_dir(path: &Path) -> std::io::Result<Claim> {
    let mut missing = Vec::new();
    let mut at = path.parent();
    while let Some(p) = at {
        match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => break,
            Ok(_) => {
                return Err(std::io::Error::other(format!(
                    "{} is not a directory",
                    p.display()
                )))
            }
            Err(e) if is_absent(&e) => missing.push(p.to_path_buf()),
            Err(e) => return Err(e),
        }
        at = p.parent();
    }
    let mut claim = Claim {
        path: path.to_path_buf(),
        made: None,
        parents: Vec::new(),
    };
    for p in missing.into_iter().rev() {
        match std::fs::create_dir(&p) {
            Ok(()) => claim.parents.push(p),
            Err(e)
                if e.kind() == ErrorKind::AlreadyExists
                    && std::fs::metadata(&p).is_ok_and(|m| m.is_dir()) => {}
            Err(e) => {
                claim.remove_parents();
                return Err(e);
            }
        }
    }
    let made = std::fs::create_dir(path).and_then(|()| std::fs::symlink_metadata(path));
    match made {
        Ok(m) => {
            claim.made = Some((m.dev(), m.ino()));
            Ok(claim)
        }
        Err(e) => {
            claim.remove_parents();
            Err(e)
        }
    }
}

/// Whether the worktree record `admin`'s `gitdir` file names the worktree at `path` (as its
/// `.git`), judged resolved.
fn names_worktree(admin: &Path, path: &Path) -> bool {
    let Ok(raw) = std::fs::read(admin.join("gitdir")) else {
        return false;
    };
    let gitdir = super::path_of(trim_newline(&raw));
    // `worktree.useRelativePaths`: relative to the record.
    let gitdir = if gitdir.is_absolute() {
        gitdir
    } else {
        admin.join(gitdir)
    };
    matches!(
        (resolve_lenient(&gitdir), resolve_lenient(&path.join(".git"))),
        (Some(a), Some(b)) if a == b
    )
}

/// The record git made for the worktree it just added at `path` on `branch`, in the
/// repository whose git common directory is `common`: the one `path`'s `.git` file names,
/// directly under `<common>/worktrees/`, whose `gitdir` names `path` back and whose `HEAD` is
/// on `branch`.
fn added_record(common: &Path, path: &Path, branch: &str) -> anyhow::Result<PathBuf> {
    let file = path.join(".git");
    if !std::fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
        bail!("{} is no file", file.display());
    }
    let raw = std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
    let named = trim_newline(&raw)
        .strip_prefix(b"gitdir: ")
        .map(|g| path.join(super::path_of(g)))
        .ok_or_else(|| anyhow!("{} names no git directory", file.display()))?;
    let admin =
        resolve_lenient(&named).ok_or_else(|| anyhow!("cannot resolve {}", named.display()))?;
    let records = resolve_lenient(&common.join("worktrees"));
    if admin.parent() != records.as_deref() {
        bail!(
            "{} names {}, no worktree record of {}",
            file.display(),
            admin.display(),
            common.display()
        );
    }
    if !names_worktree(&admin, path) {
        bail!(
            "the worktree record {} does not name {}",
            admin.display(),
            path.display()
        );
    }
    let head = read_optional(&admin.join("HEAD"))?;
    if head.as_deref().map(trim_newline) != Some(format!("ref: refs/heads/{branch}").as_bytes()) {
        bail!(
            "the worktree record {} is not on the branch {branch}",
            admin.display()
        );
    }
    Ok(admin)
}

/// The worktree paths `git worktree list --porcelain -z` prints (`worktree <path>` fields,
/// NUL-terminated), prunable ones included.
fn listed_worktrees(out: &[u8]) -> Vec<PathBuf> {
    out.split(|b| *b == 0)
        .filter_map(|f| f.strip_prefix(b"worktree "))
        .map(super::path_of)
        .collect()
}

/// Whether the paths `a` and `b` may be the same place: as written, or resolved; when either
/// cannot be resolved, unless their last components differ.
fn same_place(a: &Path, b: &Path) -> bool {
    a == b
        || match (resolve_lenient(a), resolve_lenient(b)) {
            (Some(a), Some(b)) => a == b,
            _ => match (a.file_name(), b.file_name()) {
                (Some(x), Some(y)) => x == y,
                _ => true,
            },
        }
}

/// The steps of a restore, a trash move or a purge a test can act at (`LocalGit::with_step_hook`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// A place passed every check, before its directory is made.
    Chosen,
    /// Its directory is made and the branch created, before git adds the worktree.
    Claimed,
    /// Git added the worktree and it is marked, before anything is checked out.
    Added,
    /// In the blocking job that copies the saved files into the new worktree, before it
    /// copies (with the new worktree's path).
    Copying,
    /// A submodule of a restored worktree passed every check and is held, before it is
    /// connected to its repository's copy (with the submodule's directory).
    Connecting,
    /// In the blocking job of a trash move that moves the worktree or its record into the
    /// item, before it moves (with what it moves).
    Moving,
    /// A purge checked what its item holds, before it drops the item's refs (with the item's
    /// directory).
    PurgeChecked,
    /// In the blocking job of a trash move that deletes build output ([`drop_build_output`]):
    /// an entry is open, before it is moved aside (with where it is).
    BuildOutputOpened,
    /// An entry moved aside was searched for repositories, before it is deleted (with where it
    /// was moved).
    BuildOutputSearched,
    /// What is left of an entry not deleted is about to go back (with where it goes).
    BuildOutputPuttingBack,
    /// A takeover of build output a stopped deletion left aside ([`reconcile_staged`]): the
    /// journal names the new item, before the owners are rewritten (with the worktree).
    BuildOutputTakingOver,
    /// The owners are rewritten, before the journal forgets the previous items.
    BuildOutputTakenOver,
}

/// What a test runs at each [`Step`] (with the worktree's path); an error fails the
/// operation there.
#[cfg(test)]
#[derive(Clone)]
#[allow(clippy::type_complexity)]
pub(super) struct StepHook(
    pub(super) std::sync::Arc<dyn Fn(Step, &Path) -> std::io::Result<()> + Send + Sync>,
);

#[cfg(test)]
impl std::fmt::Debug for StepHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StepHook")
    }
}

/// Where a restore's branch starts.
#[derive(Debug, Clone, Copy)]
struct Start<'a> {
    /// The commit.
    target: &'a str,
    /// It is the tip of the item's branch (else its worktree's `HEAD`).
    from_branch: bool,
    /// The worktree was on another branch than the thread's own when it was deleted
    /// ([`LocalGit::switched_back`]): `Ok((branch, commit))` to check out in the new worktree
    /// instead of the one the restore makes, where it must still be at `commit`; `Err(why)`
    /// when it cannot be.
    checkout: Option<Result<(&'a str, &'a str), &'a str>>,
}

/// The first 12 characters of the object id `id`.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(12)]
}

impl LocalGit {
    /// The trash directory, or an error when this box has none.
    fn trash_dir(&self) -> anyhow::Result<&Path> {
        self.trash
            .as_deref()
            .ok_or_else(|| anyhow!("this box has no trash directory"))
    }

    /// The object type of each of `ids` the repository at `common` has (`commit`, `tag`, ...);
    /// missing ones are left out.
    async fn object_types(
        &self,
        common: &Path,
        ids: &BTreeSet<String>,
    ) -> anyhow::Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let args: Vec<OsString> = ["cat-file", "--batch-check=%(objectname) %(objecttype)"]
            .iter()
            .map(OsString::from)
            .collect();
        let out = self
            .run(
                common,
                args,
                Mode::Read(self.timeouts.read),
                Some(input.into_bytes()),
            )
            .await?
            .into_stdout("git cat-file")?;
        Ok(out
            .lines()
            .filter_map(|l| l.split_once(' '))
            .filter(|(_, t)| *t != "missing")
            .map(|(id, t)| (id.to_string(), t.to_string()))
            .collect())
    }

    /// The commits of `candidates` that `tips` do not reach (`git rev-list`), in the
    /// repository at `common`. Both must be commits.
    async fn unreached(
        &self,
        common: &Path,
        candidates: &[&String],
        tips: &[&String],
    ) -> anyhow::Result<Vec<String>> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let input: String = candidates
            .iter()
            .map(|id| format!("{id}\n"))
            .chain(tips.iter().map(|id| format!("^{id}\n")))
            .collect();
        let args: Vec<OsString> = ["rev-list", "--stdin"].iter().map(OsString::from).collect();
        let reached = self
            .run(
                common,
                args,
                Mode::Read(self.timeouts.read),
                Some(input.into_bytes()),
            )
            .await?
            .into_stdout("git rev-list")?;
        let reached: BTreeSet<&str> = reached.lines().map(str::trim).collect();
        Ok(candidates
            .iter()
            .filter(|id| reached.contains(id.as_str()))
            .map(|id| id.to_string())
            .collect())
    }

    /// The object `name` (a full ref name) points at in the repository at `common`, if it
    /// exists.
    async fn ref_target(&self, common: &Path, name: &str) -> anyhow::Result<Option<String>> {
        let out = self
            .git(common, ["rev-parse", "--verify", "--quiet", name])
            .await?;
        Ok(out
            .ok
            .then(|| out.text().trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    /// Runs `git update-ref --stdin` in `common` with `lines` (one transaction), logging
    /// `msg`.
    async fn update_refs(&self, common: &Path, msg: &str, lines: String) -> anyhow::Result<()> {
        let args: Vec<OsString> = ["update-ref", "--create-reflog", "-m", msg, "--stdin"]
            .iter()
            .map(OsString::from)
            .collect();
        self.run(
            common,
            args,
            Mode::Change(self.timeouts.worktree),
            Some(lines.into_bytes()),
        )
        .await?
        .into_stdout("git update-ref")?;
        Ok(())
    }

    /// [`GitOps::worktree_trash`] (see the module docs). Reserved like a removal: no runtime
    /// may work in the worktree, and no runtime or other change of its path starts until the
    /// whole move is over; the item's lock is held from its creation until it is complete (or
    /// the move failed). Refused, before anything is made or moved, while the worktree holds
    /// a linked worktree whose record stays behind ([`twr_core::git::LinkedWorktree`]).
    ///
    /// [`GitOps::worktree_trash`]: twr_core::git::GitOps::worktree_trash
    pub(super) async fn trash(self, req: TrashRequest) -> anyhow::Result<TrashOutcome> {
        let TrashRequest {
            item,
            repo,
            worktree: path,
            branch,
            other_branches,
            info,
        } = req;
        if !valid_trash_id(&item) {
            bail!("{item:?} is not a trash item id");
        }
        if let Some(b) = &branch {
            if !valid_branch(b) {
                bail!("invalid branch name {b:?}");
            }
        }
        let root = self.trash_dir()?.to_path_buf();
        let common = self.common_dir(&repo).await?;
        if !matches!(presence(&common.join("reftable")), Presence::Absent) {
            bail!(
                "{} may use the reftable ref storage, whose reflogs twr cannot move to the trash",
                common.display()
            );
        }
        // Held until the whole move is over: also while the record moves and the
        // branch goes, no change of the path (a create clearing what looks like a stale record)
        // runs in between.
        let _change = self
            .workdirs
            .begin_change(&path, ChangeKind::Remove, CHANGE_WAIT)
            .await
            .map_err(|b| anyhow!("cannot move the worktree {}: {b}", path.display()))?;
        let present = match tokio::fs::metadata(&path).await {
            Ok(m) => m.is_dir(),
            Err(e) if is_absent(&e) => false,
            Err(e) => bail!(
                "cannot tell whether the worktree {} exists: {e}",
                path.display()
            ),
        };
        // Never anything but the thread's worktree, or what is left of it.
        let mut linked = present;
        if present {
            if let Err(e) = self.check_linked_worktree(&repo, &path).await {
                if !self.leftover(&repo, &path).await? {
                    return Err(e);
                }
                linked = false;
            }
        }
        let others = self.branches_in(&common, &other_branches).await;
        let records = worktree_records(&common)?;
        // Only the record proven the worktree's both ways; conflicting pointers
        // refuse before anything is made or moved.
        let record = record_of(
            &common,
            &records,
            &path,
            branch.as_deref(),
            others.as_deref(),
        )
        .map_err(|e| anyhow!("not deleting {}: {e}; nothing is touched", path.display()))?
        .map(|k| records[k].admin.clone());
        // Build output is deleted only where mount points can be told and there are none.
        let mut mounts_known = false;
        // A linked worktree checked out in it whose record stays behind would be
        // left naming a missing checkout, and `git worktree prune` + `git gc` in its
        // repository would lose what only it has: refused before anything is made or moved.
        if present {
            let whose = if info.thread_name.is_empty() {
                format!("the worktree {}", path.display())
            } else {
                format!("{}'s worktree", info.thread_name)
            };
            let doomed: Vec<PathBuf> = std::iter::once(path.as_path())
                .chain(record.as_deref())
                .map(|p| resolve_lenient(p).unwrap_or_else(|| p.to_path_buf()))
                .collect();
            let (root, mut budget) = (path.clone(), self.search_max);
            // A search cut short (too many entries, unreadable parts) goes on: the delete card
            // said which directories it could not search, and the user confirmed it.
            let (linked, _cut) =
                tokio::task::spawn_blocking(move || linked_outside(&root, &doomed, &mut budget))
                    .await
                    .context("looking for linked worktrees in the worktree")?;
            if let Some((dir, rec)) = linked.first() {
                let l = linked_worktree(dir, rec, Some(&common));
                bail!("{}; nothing is touched", l.refusal(&whose));
            }
            // Nor one with a file system mounted in it: moving, copying or deleting through a
            // mount point (build output, a move across file systems, a purge) would take that
            // file system's files.
            let at = path.clone();
            let mounts = tokio::task::spawn_blocking(move || mounts_inside(&at))
                .await
                .context("looking for mount points in the worktree")?;
            if let Some(why) = mount_refusal(&mounts, &whose) {
                bail!("{why}; nothing is touched");
            }
            mounts_known = mounts.is_some();
        }
        let tip = match &branch {
            Some(b) => self.ref_target(&common, &format!("refs/heads/{b}")).await?,
            None => None,
        };
        // A branch checked out elsewhere stays: `git branch -D` would refuse it at the end.
        if let (Some(b), Some(_)) = (&branch, &tip) {
            if let Some(at) = checked_out(&common, &repo, b, record.as_deref())? {
                bail!("its branch {b} is checked out at {}", at.display());
            }
        }
        if !present && record.is_none() && tip.is_none() {
            return Ok(TrashOutcome::default());
        }
        twr_core::paths::ensure_private_dir(&root)?;
        let dir = root.join(&item);
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let lock = match lock_item(&dir, true).await {
            Ok(l) => l,
            Err(e) => {
                let _ = std::fs::remove_file(dir.join(LOCK));
                let _ = std::fs::remove_dir(&dir);
                return Err(e);
            }
        };
        let mut m = TrashManifest {
            item: item.clone(),
            info,
            repo: repo.clone(),
            common: common.clone(),
            worktree: path.clone(),
            record_name: record
                .as_ref()
                .and_then(|a| a.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
            branch: branch.clone(),
            ..Default::default()
        };
        m.recover = recover_text(&m, &dir);
        write_manifest(&dir, &m)?;
        // Nothing moved yet: on a failure here, the item goes (only what this made).
        let discard = |dir: &Path| {
            if let Err(r) = remove_item(dir) {
                tracing::warn!(dir = %dir.display(), error = %r, "cannot remove a trash item nothing moved to");
            }
        };
        // 1. Refs that keep every commit the branch and the record reach.
        if let Err(e) = self
            .keep_refs(
                &common,
                &dir,
                branch.as_deref(),
                tip.as_deref(),
                record.as_deref(),
                &mut m,
            )
            .await
        {
            if was_killed(&e) {
                // The transaction may have made the refs: the item stays (incomplete, nothing
                // moved), and a purge drops whatever refs it has once it is abandoned.
                return Err(e.context(format!(
                    "keeping its commits in the trash; nothing moved, and trash item {item} ({}) \
                     stays incomplete with whatever refs git made",
                    dir.display()
                )));
            }
            discard(&dir);
            return Err(e.context("keeping its commits in the trash"));
        }
        // 2. The objects of its index that no kept commit has.
        if let Some(admin) = &record {
            match self.pack_index(&common, admin, &dir, &m).await {
                Ok(pack) => m.index_pack = pack,
                Err(e) => {
                    if let Err(r) = self.drop_refs(&common, &item).await {
                        tracing::warn!(item = %item, error = %format!("{r:#}"), "cannot drop the refs of a trash item nothing moved to");
                    } else {
                        discard(&dir);
                    }
                    return Err(e.context("keeping the objects of its index in the trash"));
                }
            }
        }
        m.recover = recover_text(&m, &dir);
        write_manifest(&dir, &m)?;
        let partial = |what: &str, e: &dyn std::fmt::Display| {
            anyhow!(
                "{what} failed: {e}; trash item {item} ({}) keeps what moved and its commits, the \
                 rest is where it was",
                dir.display()
            )
        };
        // 3. Its build output is deleted, not kept: it is regenerated, and can be tens of GB.
        //    First, what a deletion stopped part way left aside goes back, or comes along.
        if let Some(journal) = record.as_ref().filter(|_| linked).map(|r| r.join(JOURNAL)) {
            let (root, j, owner) = (path.clone(), journal.clone(), m.item.clone());
            let hook = self.steps_in_job();
            m.build_output_aside =
                tokio::task::spawn_blocking(move || reconcile_staged(&root, &j, &owner, &hook))
                    .await
                    .map_err(|e| anyhow!("{e}"))
                    .and_then(|r| r)
                    .map_err(|e| {
                        partial(
                            "putting back build output a deletion left aside",
                            &format!("{e:#}"),
                        )
                    })?;
            // Where mount points cannot be told (no mount list: any system but Linux and macOS),
            // none is deleted: it goes to the trash with the rest.
            if mounts_known && !self.disposable.is_empty() {
                match self.tree_files(&path).await {
                    Ok(tree) => {
                        let (root, list, mut budget) =
                            (path.clone(), self.disposable.clone(), self.search_max);
                        let (hook, owner) = (self.steps_in_job(), m.item.clone());
                        let kept = std::mem::take(&mut m.build_output_aside);
                        let dropped = tokio::task::spawn_blocking(move || {
                            let build: Vec<String> = tree
                                .ignored
                                .into_iter()
                                .filter(|p| disposable(p, &list))
                                .collect();
                            drop_build_output(
                                &root,
                                &build,
                                &owner,
                                &journal,
                                kept,
                                &mut budget,
                                &hook,
                            )
                        })
                        .await
                        .map_err(std::io::Error::other)
                        .and_then(|r| r)
                        .map_err(|e| partial("deleting its build output", &e))?;
                        m.build_output = dropped.deleted;
                        m.build_output_aside = dropped.aside;
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "cannot list the worktree's build output; all of it goes to the trash");
                    }
                }
            }
            if !m.build_output_aside.is_empty() {
                m.recover = recover_text(&m, &dir);
            }
            write_manifest(&dir, &m)?;
        }
        // 4. The directory, then the record (git then forgets the worktree). Each is noted as
        //    in the item once all of it is (a copy across file systems: before its source
        //    goes).
        if present {
            let (src, dst) = (path.clone(), dir.join("worktree"));
            let moved = self
                .move_noted(&mut m, &dir, src, dst, |m| m.worktree_moved = true)
                .await
                .map_err(|e| partial("moving the worktree into the trash", &format!("{e:#}")))?;
            m.worktree_left = matches!(moved, Moved::Copied { left: true });
            write_manifest(&dir, &m)?;
        }
        if let Some(admin) = &record {
            let (src, dst) = (admin.clone(), dir.join("record"));
            let moved = self
                .move_noted(&mut m, &dir, src, dst, |m| m.record_moved = true)
                .await
                .map_err(|e| {
                    partial(
                        "moving the worktree record into the trash",
                        &format!("{e:#}"),
                    )
                })?;
            if matches!(moved, Moved::Copied { left: true }) {
                tracing::warn!(record = %admin.display(), "part of a worktree record stayed after its copy to the trash");
            }
            write_manifest(&dir, &m)?;
        }
        // 5. The branch, kept by the item's refs.
        if let (Some(b), Some(_)) = (&branch, &tip) {
            let args: Vec<OsString> = vec!["branch".into(), "-D".into(), b.into()];
            self.run(&common, args, Mode::Change(self.timeouts.worktree), None)
                .await
                .and_then(|o| o.into_stdout("git branch -D"))
                .map_err(|e| partial("deleting the branch", &format!("{e:#}")))?;
            m.branch_deleted = true;
            write_manifest(&dir, &m)?;
        }
        let d = dir.clone();
        m.bytes = tokio::task::spawn_blocking(move || disk_usage(&d))
            .await
            .unwrap_or(0);
        m.complete = true;
        write_manifest(&dir, &m)?;
        drop(lock);
        Ok(TrashOutcome {
            item: Some(item),
            dir: Some(dir),
            worktree_moved: m.worktree_moved,
            worktree_left: m.worktree_left,
            build_output_aside: m
                .build_output_aside
                .iter()
                .map(|k| k.path.clone())
                .collect(),
            branch_deleted: m.branch_deleted,
            refs: m.refs.iter().map(|r| r.name.clone()).collect(),
            bytes: m.bytes,
        })
    }

    /// Moves `src` into the item `dir` at `dst` ([`move_tree`], what cannot be copied left at
    /// the source), applying `note` to `m` and writing the manifest once all of it is there.
    async fn move_noted(
        &self,
        m: &mut TrashManifest,
        dir: &Path,
        src: PathBuf,
        dst: PathBuf,
        note: fn(&mut TrashManifest),
    ) -> anyhow::Result<Moved> {
        let (copy, dir) = (self.copy_moves, dir.to_path_buf());
        let mut noted = m.clone();
        let hook = self.step_in_job(Step::Moving, &src);
        let (moved, noted) = tokio::task::spawn_blocking(move || {
            let r = hook().and_then(|()| {
                move_tree(&src, &dst, copy, &mut || {
                    note(&mut noted);
                    write_manifest(&dir, &noted).map_err(std::io::Error::other)
                })
            });
            if matches!(r, Ok(Moved::Renamed)) {
                note(&mut noted);
            }
            (r, noted)
        })
        .await
        .map_err(|e| anyhow!("moving into the trash: {e}"))?;
        let moved = moved?;
        *m = noted;
        Ok(moved)
    }

    /// Step 1 of [`LocalGit::trash`]: makes the refs of item `dir` in the repository at
    /// `common` (see the module docs), all in one transaction, and notes them in `m`; copies
    /// the branch's reflog verbatim into the item. The reflogs are copied first; on failure,
    /// the copies go again.
    async fn keep_refs(
        &self,
        common: &Path,
        dir: &Path,
        branch: Option<&str>,
        tip: Option<&str>,
        record: Option<&Path>,
        m: &mut TrashManifest,
    ) -> anyhow::Result<()> {
        let item = m.item.clone();
        let base = format!("{TRASH_REF_PREFIX}{item}/");
        let logs = common.join("logs");
        // (name, target, kind, reflog to copy)
        let mut planned: Vec<(String, String, &'static str, Option<PathBuf>)> = Vec::new();
        let mut roots: BTreeSet<String> = BTreeSet::new();
        if let (Some(b), Some(tip)) = (branch, tip) {
            let log = logs.join(format!("refs/heads/{b}"));
            roots.extend(
                read_reflog(&log)
                    .map_err(|e| anyhow!(e))?
                    .lines()
                    .map(str::to_string),
            );
            m.branch_log_saved = copy_reflog(&log, &dir.join(REFLOGS).join("branch"))?;
            planned.push((
                format!("{base}branch"),
                tip.to_string(),
                "branch",
                Some(log),
            ));
        }
        if let Some(admin) = record {
            roots.extend(
                record_roots(admin)
                    .map_err(|e| anyhow!(e))?
                    .lines()
                    .map(str::to_string),
            );
            let head = super::read_small_file(&admin.join("HEAD"))
                .with_context(|| format!("reading {}", admin.join("HEAD").display()))?;
            let head_text = String::from_utf8_lossy(trim_newline(&head)).into_owned();
            m.head = Some(head_text.clone());
            let commit = match head_tip(&head).map_err(|e| anyhow!(e))? {
                Some(id) => Some(id),
                None => match head_text.strip_prefix("ref: ") {
                    Some(target)
                        if Some(target) == branch.map(|b| format!("refs/heads/{b}")).as_deref() =>
                    {
                        tip.map(str::to_string)
                    }
                    Some(target) => self.ref_target(common, target).await?,
                    None => None,
                },
            };
            if let Some(c) = commit {
                planned.push((
                    format!("{base}head"),
                    c,
                    "head",
                    Some(admin.join("logs/HEAD")),
                ));
            }
            let refs_dir = admin.join("refs");
            for r in files_under(&refs_dir).map_err(|e| anyhow!(e))? {
                let raw = super::read_small_file(&r)
                    .with_context(|| format!("reading {}", r.display()))?;
                let id = String::from_utf8_lossy(trim_newline(&raw)).into_owned();
                if id.starts_with("ref: ") {
                    continue;
                }
                let Ok(rel) = r.strip_prefix(admin) else {
                    continue;
                };
                let rel = rel.to_string_lossy().into_owned();
                if !rel.split('/').all(twr_core::git::valid_branch_component) {
                    bail!("the worktree ref {rel:?} cannot be kept under another name");
                }
                planned.push((
                    format!("{base}wt/{rel}"),
                    id,
                    "worktree-ref",
                    Some(admin.join("logs").join(&rel)),
                ));
            }
        }
        // Commits only a reflog names: a ref each, unless a kept tip reaches them.
        let mut all: BTreeSet<String> = roots.clone();
        all.extend(planned.iter().map(|p| p.1.clone()));
        let types = self.object_types(common, &all).await?;
        planned.retain(|p| {
            let keep = types.contains_key(&p.1);
            if !keep {
                tracing::warn!(target = %p.1, "a commit to keep in the trash is missing from the repository");
            }
            keep
        });
        let commit = |id: &String| types.get(id).is_some_and(|t| t == "commit");
        let only_reflog: Vec<String> = {
            let tips: Vec<&String> = planned
                .iter()
                .map(|p| &p.1)
                .filter(|id| commit(id))
                .collect();
            let candidates: Vec<&String> = roots.iter().filter(|id| commit(id)).collect();
            let mut out: Vec<String> = roots
                .iter()
                .filter(|id| types.contains_key(*id) && !commit(id))
                .cloned()
                .collect();
            out.extend(self.unreached(common, &candidates, &tips).await?);
            out
        };
        for id in only_reflog {
            planned.push((format!("{base}reflog/{id}"), id, "reflog", None));
        }
        if planned.is_empty() {
            return Ok(());
        }
        let mut copied: Vec<PathBuf> = Vec::new();
        let result = async {
            for (name, _, _, log) in &planned {
                if let Some(from) = log {
                    let to = logs.join(name);
                    if copy_reflog(from, &to)? {
                        copied.push(to);
                    }
                }
            }
            let lines: String = planned
                .iter()
                .map(|(name, target, _, _)| format!("create {name} {target}\n"))
                .collect();
            self.update_refs(common, TRASHED_MSG, lines).await
        }
        .await;
        if let Err(e) = result {
            // A killed `update-ref` may have made the refs these reflogs now belong to.
            if !was_killed(&e) {
                for c in &copied {
                    let _ = std::fs::remove_file(c);
                }
                let _ = remove_empty_dirs(&logs.join(base.trim_end_matches('/')));
            }
            return Err(e);
        }
        m.refs = planned
            .into_iter()
            .map(|(name, target, kind, _)| TrashRef {
                name,
                target,
                kind: kind.to_string(),
            })
            .collect();
        Ok(())
    }

    /// Step 2 of [`LocalGit::trash`]: packs every object the index of the record `admin`
    /// names that no tree of the item's commits (`m.refs`) has into `<dir>/index-objects/`
    /// (`git rev-list --indexed-objects` of that worktree alone: its entries of every stage,
    /// cache tree and resolve-undo entries, plus its sparse directory trees). Returns the
    /// pack's id, `None` when there is nothing to pack.
    async fn pack_index(
        &self,
        common: &Path,
        admin: &Path,
        dir: &Path,
        m: &TrashManifest,
    ) -> anyhow::Result<Option<String>> {
        if matches!(presence(&admin.join("index")), Presence::Absent) {
            return Ok(None);
        }
        let git_dir = {
            let mut a = OsString::from("--git-dir=");
            a.push(admin.as_os_str());
            a
        };
        let mut input = String::new();
        // Sparse directory entries (an older git without `--sparse` has no sparse index).
        let mut ls: Vec<OsString> = vec![git_dir.clone()];
        ls.extend(["ls-files", "--sparse", "-s", "-z"].map(OsString::from));
        let listed = self
            .run(common, ls, Mode::Read(self.timeouts.read), None)
            .await?;
        if listed.ok {
            for entry in listed.stdout.split(|b| *b == 0) {
                let mut fields = entry.splitn(3, |b| *b == b' ');
                if let (Some(b"040000"), Some(id)) = (fields.next(), fields.next()) {
                    input.push_str(&String::from_utf8_lossy(id));
                    input.push('\n');
                }
            }
        } else {
            tracing::debug!(error = %listed.stderr.trim(), "git ls-files --sparse");
        }
        input.push_str("--not\n");
        for r in &m.refs {
            input.push_str(&format!("{}^{{tree}}\n", r.target));
        }
        let mut args: Vec<OsString> = vec![git_dir];
        // `--single-worktree` before `--indexed-objects`: git applies the latter as it reads it.
        args.extend(
            [
                "rev-list",
                "--objects",
                // Ids only: a path may hold a line break.
                "--no-object-names",
                "--single-worktree",
                "--indexed-objects",
                "--missing=allow-any",
                "--stdin",
            ]
            .map(OsString::from),
        );
        let objects = self
            .run(
                common,
                args,
                Mode::Read(self.timeouts.read),
                Some(input.into_bytes()),
            )
            .await?
            .into_stdout_bytes("git rev-list --indexed-objects")?;
        if objects.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let into = dir.join(INDEX_OBJECTS);
        std::fs::create_dir_all(&into).with_context(|| format!("creating {}", into.display()))?;
        let args: Vec<OsString> =
            vec!["pack-objects".into(), "-q".into(), into.join("pack").into()];
        let id = self
            .run(
                common,
                args,
                Mode::Change(self.timeouts.worktree),
                Some(objects),
            )
            .await?
            .into_stdout("git pack-objects")?;
        let id = id.trim().to_string();
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("git pack-objects named no pack ({id:?})");
        }
        Ok(Some(id))
    }

    /// [`GitOps::trash_list`]: every item of the trash whose manifest reads, oldest first;
    /// an incomplete one with why a purge never takes it ([`TrashItem::kept`]).
    ///
    /// [`GitOps::trash_list`]: twr_core::git::GitOps::trash_list
    pub(super) async fn trash_items(self) -> anyhow::Result<Vec<TrashItem>> {
        let Some(root) = self.trash.clone() else {
            return Ok(Vec::new());
        };
        let mut items = tokio::task::spawn_blocking(move || list_items(&root))
            .await
            .map_err(|e| anyhow!("listing the trash: {e}"))??;
        for i in items.iter_mut().filter(|i| !i.manifest.complete) {
            i.kept = match self.holds(&i.manifest, &i.dir).await {
                Ok(what) => what.map(|w| format!("holds {w}")),
                Err(e) => Some(format!("cannot tell what it holds: {e:#}")),
            };
        }
        Ok(items)
    }

    /// [`GitOps::trash_purge`] (see the module docs).
    ///
    /// [`GitOps::trash_purge`]: twr_core::git::GitOps::trash_purge
    pub(super) async fn purge(self, purge: TrashPurge) -> anyhow::Result<PurgeOutcome> {
        let mut out = PurgeOutcome::default();
        let Some(root) = self.trash.clone() else {
            out.unknown = purge.items;
            return Ok(out);
        };
        let r = root.clone();
        let items = tokio::task::spawn_blocking(move || list_items(&r))
            .await
            .map_err(|e| anyhow!("listing the trash: {e}"))??;
        let mut chosen: Vec<&TrashItem> = Vec::new();
        for id in &purge.items {
            match items.iter().find(|i| i.manifest.item == *id) {
                Some(i) => chosen.push(i),
                None => out.unknown.push(id.clone()),
            }
        }
        if let Some(before) = purge.before_ms {
            for i in &items {
                if i.manifest.info.deleted_at_ms < before
                    && !chosen.iter().any(|c| c.manifest.item == i.manifest.item)
                {
                    chosen.push(i);
                }
            }
        }
        for i in chosen {
            let id = i.manifest.item.clone();
            match self.purge_item(&root, &id).await {
                Ok(()) => out.purged.push(id),
                Err(e) => {
                    tracing::info!(item = %id, error = %format!("{e:#}"), "not purging a trash item");
                    out.failed.push(TrashFailure {
                        item: id,
                        error: format!("{e:#}"),
                    })
                }
            }
        }
        Ok(out)
    }

    /// Removes item `id` of the trash `root` (its lock held): its refs (in the repository its
    /// manifest names, unless that repository is gone), then its directory. Refused for an
    /// incomplete one that holds what its deletion moved or that is not abandoned yet
    /// ([`ABANDONED_AFTER`]).
    ///
    /// The check of what an incomplete item holds alone counts the refs of the repository's
    /// other items as copies that survive it, so every purge in that repository, complete
    /// items' too, runs that check and drops its refs under the repository's purge lock
    /// ([`lock_repo`]), taken after the item's: the check sees no other purge's
    /// deletion half done, and none deletes what it just counted before it is done. Moves to
    /// the trash and restores do not take it: they remove a branch or a reflog only while
    /// their item's refs (that item locked, so not purged meanwhile) keep what it reaches.
    async fn purge_item(&self, root: &Path, id: &str) -> anyhow::Result<()> {
        let dir = item_dir(root, id).map_err(|e| anyhow!(e))?;
        let _lock = lock_item(&dir, false).await?;
        let m = read_manifest(&dir, id).map_err(|e| anyhow!(e))?;
        let repo_lock = match presence(&m.common) {
            Presence::Absent => None,
            Presence::Unknown(e) => bail!("{}: {e}", m.common.display()),
            Presence::Present => lock_repo(&m.common).await?,
        };
        if !m.complete {
            if let Some(what) = self.holds(&m, &dir).await? {
                bail!(
                    "it is incomplete (its deletion failed part way) and holds {what}, which is \
                     never purged: put it back by hand ({})",
                    m.recover
                );
            }
            if manifest_age(&dir).is_none_or(|a| a < ABANDONED_AFTER) {
                bail!("it is incomplete and recent: its deletion may still be at work");
            }
        }
        self.at_step(Step::PurgeChecked, &dir)?;
        self.drop_refs(&m.common, id).await?;
        // What the directory still holds no other item's check counts.
        drop(repo_lock);
        tokio::task::spawn_blocking(move || remove_item(&dir))
            .await
            .map_err(|e| anyhow!("removing the item: {e}"))?
            .context("removing the item's directory")?;
        Ok(())
    }

    /// What of its thread an item `dir` (manifest `m`) holds that is nowhere else, if anything:
    /// the worktree's files, its record, or commits nothing but the item keeps
    /// ([`LocalGit::only_in_item`]). Never judged by whether a branch of the thread's name
    /// exists: one made again since (a resume from `HEAD`) need not have the item's commits.
    async fn holds(&self, m: &TrashManifest, dir: &Path) -> anyhow::Result<Option<String>> {
        for (sub, what) in [
            ("worktree", "the worktree's files"),
            ("record", "the worktree's record"),
        ] {
            if !matches!(presence(&dir.join(sub)), Presence::Absent) {
                return Ok(Some(what.into()));
            }
        }
        let only = self
            .only_in_item(m, dir)
            .await
            .context("telling which commits only the item keeps")?;
        Ok(match only {
            0 => None,
            1 => Some("the only copy of 1 commit".into()),
            n => Some(format!("the only copy of {n} commits")),
        })
    }

    /// How many commits of the repository of item `dir` (manifest `m`) nothing but the item
    /// keeps: those its roots reach and no other root does (`git rev-list <item> --not
    /// <others>`, counted as [`LocalGit::set_losses`] counts what a deletion loses).
    ///
    /// - the item's: every ref under `refs/twr-trash/<item>/` (whatever the manifest says:
    ///   a deletion that failed part way may have made them without noting them), every id of
    ///   their reflogs, and every id of its verbatim reflog copy (`reflogs/branch`);
    /// - the others: every other ref (other trash items' too), every other reflog under
    ///   `logs/`, the main worktree's detached `HEAD`, and every worktree record's roots.
    ///
    /// A root of the item that cannot be read is an error (never "nothing"); another one that
    /// cannot be read is left out (more counts as only the item's, never less). Objects the
    /// repository does not have keep nothing. 0 when the repository is gone.
    async fn only_in_item(&self, m: &TrashManifest, dir: &Path) -> anyhow::Result<u64> {
        let common = &m.common;
        match presence(common) {
            Presence::Absent => return Ok(0),
            Presence::Unknown(e) => bail!("{}: {e}", common.display()),
            Presence::Present => {}
        }
        let base = format!("{TRASH_REF_PREFIX}{}/", m.item);
        let refs = self
            .git(
                common,
                ["for-each-ref", "--format=%(objectname) %(refname)"],
            )
            .await?
            .into_stdout("git for-each-ref")?;
        let mut roots = String::new();
        let mut others: BTreeSet<String> = BTreeSet::new();
        for (id, name) in refs.lines().filter_map(|l| l.split_once(' ')) {
            if name.starts_with(&base) {
                roots.push_str(id);
                roots.push('\n');
            } else {
                others.insert(id.to_string());
            }
        }
        let logs = common.join("logs");
        let own_logs = logs.join(base.trim_end_matches('/'));
        for log in files_under(&own_logs).map_err(|e| anyhow!(e))? {
            roots.push_str(&read_reflog(&log).map_err(|e| anyhow!(e))?);
        }
        roots.push_str(&read_reflog(&dir.join(REFLOGS).join("branch")).map_err(|e| anyhow!(e))?);
        if roots.trim().is_empty() {
            return Ok(0);
        }
        let mut add = |ids: &str| others.extend(ids.lines().map(str::to_string));
        match files_under(&logs) {
            Ok(files) => {
                for log in files.iter().filter(|l| !l.starts_with(&own_logs)) {
                    match read_reflog(log) {
                        Ok(ids) => add(&ids),
                        Err(e) => {
                            tracing::warn!(error = %e, "a reflog cannot be read: what only it keeps counts as only the trash item's")
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the reflogs cannot be listed: what only they keep counts as only the trash item's")
            }
        }
        let head = common.join("HEAD");
        match std::fs::read(&head)
            .map_err(|e| e.to_string())
            .and_then(|h| head_tip(&h))
        {
            Ok(tip) => tip.iter().for_each(|t| add(t)),
            Err(e) => {
                tracing::warn!(head = %head.display(), error = %e, "the main worktree's HEAD cannot be read: what only it keeps counts as only the trash item's")
            }
        }
        for r in worktree_records(common)? {
            match record_roots(&r.admin) {
                Ok(ids) => add(&ids),
                Err(e) => {
                    tracing::warn!(error = %e, "a worktree record cannot be read: what only it keeps counts as only the trash item's")
                }
            }
        }
        let all: BTreeSet<&str> = roots
            .lines()
            .chain(others.iter().map(String::as_str))
            .collect();
        let missing = self.missing_objects(common, &all).await?;
        let roots: String = roots
            .lines()
            .filter(|id| !missing.contains(*id))
            .map(|id| format!("{id}\n"))
            .collect();
        let not: String = others
            .iter()
            .filter(|id| !missing.contains(*id))
            .map(|id| format!("^{id}\n"))
            .collect();
        Ok(self.lost_commits(common, roots, &not).await?.len() as u64)
    }

    /// Deletes every ref under `refs/twr-trash/<id>/` of the repository whose git common
    /// directory is `common` (nothing when it is gone).
    async fn drop_refs(&self, common: &Path, id: &str) -> anyhow::Result<()> {
        match presence(common) {
            Presence::Absent => return Ok(()),
            Presence::Unknown(e) => bail!("{}: {e}", common.display()),
            Presence::Present => {}
        }
        let base = format!("{TRASH_REF_PREFIX}{id}/");
        let names = self
            .git(common, ["for-each-ref", "--format=%(refname)", &base])
            .await?
            .into_stdout("git for-each-ref")?;
        let lines: String = names
            .lines()
            .filter(|n| n.starts_with(&base))
            .map(|n| format!("delete {n}\n"))
            .collect();
        if !lines.is_empty() {
            self.update_refs(common, "twr: purged from the trash", lines)
                .await?;
        }
        let _ = remove_empty_dirs(&common.join("logs").join(base.trim_end_matches('/')));
        Ok(())
    }

    /// [`GitOps::trash_restore`] (see the module docs): reads the item, never changes it (its
    /// lock is held throughout, so no purge removes it meanwhile). A step that fails removes
    /// what the restore made ([`LocalGit::unmake`]).
    ///
    /// [`GitOps::trash_restore`]: twr_core::git::GitOps::trash_restore
    pub(super) async fn restore(self, req: RestoreRequest) -> anyhow::Result<RestoreOutcome> {
        let root = self.trash_dir()?.to_path_buf();
        let dir = item_dir(&root, &req.item).map_err(|e| anyhow!(e))?;
        let _lock = lock_item(&dir, false).await?;
        let m = read_manifest(&dir, &req.item).map_err(|e| anyhow!(e))?;
        if !m.complete {
            bail!(
                "trash item {} is incomplete (its deletion failed part way): put it back by hand \
                 ({})",
                m.item,
                m.recover
            );
        }
        if !valid_branch(&req.branch) {
            bail!("invalid branch name {:?}", req.branch);
        }
        let common = m.common.clone();
        if !matches!(presence(&common), Presence::Present) {
            bail!("its repository {} is not there", common.display());
        }
        match self.common_dir(&m.repo).await {
            Ok(c) if c == common => {}
            Ok(c) => bail!(
                "{} belongs to {} now, not to {}",
                m.repo.display(),
                c.display(),
                common.display()
            ),
            Err(e) => return Err(e.context(format!("its repository {}", m.repo.display()))),
        }
        if let Some((_, wt, mark)) = restored_copy(&m) {
            bail!(
                "trash item {} was restored already: its worktree {} and branch {} are there",
                m.item,
                wt.display(),
                mark.branch
            );
        }
        // The commit the branch starts at: the branch's tip, else the worktree's HEAD.
        let kept = |kind: &str| m.refs.iter().find(|r| r.kind == kind);
        let (start, from_branch) = match (kept("branch"), kept("head")) {
            (Some(r), _) => (r, true),
            (None, Some(r)) => (r, false),
            (None, None) => bail!(
                "trash item {} keeps no commit to make a branch at (no branch, no HEAD)",
                m.item
            ),
        };
        let target = self
            .ref_target(&common, &format!("{}^{{commit}}", start.name))
            .await?
            .ok_or_else(|| {
                anyhow!(
                    "{} is no commit of {} (any more)",
                    start.name,
                    common.display()
                )
            })?;
        if self
            .ref_target(&common, &format!("refs/heads/{}", req.branch))
            .await?
            .is_some()
        {
            bail!("the branch {} exists already", req.branch);
        }
        // Removing what this restore made deletes the branch, and git its reflog with it: a
        // reflog that is there without the branch is not this restore's to delete.
        let log = common.join("logs/refs/heads").join(&req.branch);
        if !matches!(presence(&log), Presence::Absent) {
            bail!(
                "a reflog of the branch {} is there without the branch ({}); remove it, or \
                 restore under another name",
                req.branch,
                log.display()
            );
        }
        let others = self.branches_in(&common, &req.other_branches).await;
        let switched = self.switched_back(&m, others.as_deref()).await?;
        let (claim, _change) = self.claim_place(&m, &req.paths).await?;
        let path = claim.path.clone();
        let mut made = Made {
            claim: Some(claim),
            ..Made::default()
        };
        let start = Start {
            target: &target,
            from_branch,
            checkout: switched.as_ref().map(|s| {
                s.as_ref()
                    .map(|(on, at)| (on.as_str(), at.as_str()))
                    .map_err(String::as_str)
            }),
        };
        match self
            .make_again(&m, &dir, &req.branch, start, &path, &mut made)
            .await
        {
            Ok(out) => Ok(out),
            // Git was killed while it changed something: what it changed is left as it is.
            Err(e) if was_killed(&e) => Err(e.context(format!(
                "restoring trash item {} failed; what it made is left as it is ({}), and the \
                 item is as it was",
                m.item,
                made.left(&req.branch)
            ))),
            Err(e) => Err(match self.unmake(&common, &m.repo, &made).await {
                Ok(()) => e.context(format!(
                    "restoring trash item {} failed; what it made is removed, and the item is as \
                     it was",
                    m.item
                )),
                Err(u) => e.context(format!(
                    "restoring trash item {} failed, and removing what it made failed too ({u}); \
                     the item is as it was",
                    m.item
                )),
            }),
        }
    }

    /// Where the worktree of item `m` comes back when it was on another branch than the
    /// thread's own (`m.head` names it): that branch, which its deletion left in the
    /// repository, as long as it still points at the commit the worktree was on (the item's
    /// `head` ref), is checked out nowhere and is none of `others` (the recorded branches of
    /// the box's live threads in its repository), so the saved files land on the tree they
    /// were made on (`Ok((branch, commit))`); otherwise the restore's new branch, and `Err`
    /// says why. `None`: the worktree was on the thread's branch, or detached, or the hub sent
    /// no `others` (an older hub, which reads no [`RestoreOutcome::checkout`]).
    async fn switched_back(
        &self,
        m: &TrashManifest,
        others: Option<&[String]>,
    ) -> anyhow::Result<Option<Result<(String, String), String>>> {
        let Some(others) = others else {
            return Ok(None);
        };
        let Some(on) = m
            .head
            .as_deref()
            .and_then(|h| h.strip_prefix("ref: refs/heads/"))
            .filter(|on| m.branch.as_deref() != Some(*on))
        else {
            return Ok(None);
        };
        if !valid_branch(on) {
            return Ok(Some(Err(format!("{on:?} is no branch twr checks out"))));
        }
        if others.iter().any(|o| o == on) {
            return Ok(Some(Err(format!(
                "the branch {on} is another thread's now"
            ))));
        }
        let Some(kept) = m.refs.iter().find(|r| r.kind == "head") else {
            return Ok(Some(Err(format!(
                "the trash item keeps no commit of the branch {on}"
            ))));
        };
        Ok(Some(
            match self
                .ref_target(&m.common, &format!("refs/heads/{on}"))
                .await?
            {
                None => Err(format!("the branch {on} is gone")),
                Some(t) if t != kept.target => Err(format!(
                    "the branch {on} has moved since (to {})",
                    short_id(&t)
                )),
                Some(_) => match checked_out(&m.common, &m.repo, on, None)? {
                    Some(at) => Err(format!(
                        "the branch {on} is checked out at {}",
                        at.display()
                    )),
                    None => Ok((on.to_string(), kept.target.clone())),
                },
            },
        ))
    }

    /// Claims the first of `paths` (absolute ones) a restore of the item `m` may put its
    /// worktree in: nothing is there (`symlink_metadata`), no worktree record of the
    /// repository names it ([`LocalGit::named_by_record`]: stale records too, which a restore
    /// never clears), and making its directory ([`claim_dir`]) succeeds; a place another
    /// process takes between the checks and the `mkdir` makes it fail, and the next one is
    /// tried. Returns the claim and the place's reservation ([`ChangeKind::Create`]).
    async fn claim_place(
        &self,
        m: &TrashManifest,
        paths: &[PathBuf],
    ) -> anyhow::Result<(Claim, crate::workdirs::WorkdirChange)> {
        let mut taken: Vec<String> = Vec::new();
        for path in paths.iter().filter(|p| p.is_absolute()) {
            match presence(path) {
                Presence::Absent => {}
                Presence::Present => {
                    taken.push(format!("{} exists", path.display()));
                    continue;
                }
                Presence::Unknown(e) => {
                    taken.push(format!("{}: {e}", path.display()));
                    continue;
                }
            }
            let change = match self
                .workdirs
                .begin_change(path, ChangeKind::Create, CHANGE_WAIT)
                .await
            {
                Ok(c) => c,
                Err(b) => {
                    taken.push(format!("{}: {b}", path.display()));
                    continue;
                }
            };
            match self.named_by_record(m, path).await {
                Ok(None) => {}
                Ok(Some(by)) => {
                    taken.push(format!(
                        "{}: the worktree record {} names it",
                        path.display(),
                        by
                    ));
                    continue;
                }
                Err(e) => {
                    taken.push(format!("{}: {e:#}", path.display()));
                    continue;
                }
            }
            self.at_step(Step::Chosen, path)?;
            match claim_dir(path) {
                Ok(claim) => return Ok((claim, change)),
                Err(e) => taken.push(format!("{}: {e}", path.display())),
            }
        }
        bail!(
            "every place the worktree could go is taken ({})",
            taken.join("; ")
        );
    }

    /// The worktree record of the repository of the item `m` that names `path`, if any: a
    /// worktree `git worktree list --porcelain -z` prints (prunable ones included), or one
    /// the `gitdir` file of a record under `<common>/worktrees/` names; judged as written and
    /// resolved ([`same_place`]). A record that cannot be read names nothing git would see.
    async fn named_by_record(
        &self,
        m: &TrashManifest,
        path: &Path,
    ) -> anyhow::Result<Option<String>> {
        let list = |z: bool| {
            let mut args: Vec<OsString> =
                vec!["worktree".into(), "list".into(), "--porcelain".into()];
            if z {
                args.push("-z".into());
            }
            self.run(&m.repo, args, Mode::Read(self.timeouts.read), None)
        };
        let out = list(true).await?;
        // Git before 2.36 has no `-z`: then its lines (a path with a line break in it is still
        // found by its record's `gitdir` below).
        let listed = if out.ok {
            out.stdout
        } else {
            let mut lines = list(false).await?.into_stdout_bytes("git worktree list")?;
            for b in lines.iter_mut().filter(|b| **b == b'\n') {
                *b = 0;
            }
            lines
        };
        if let Some(wt) = listed_worktrees(&listed)
            .into_iter()
            .find(|wt| same_place(wt, path))
        {
            return Ok(Some(format!("of {}", wt.display())));
        }
        for r in worktree_records(&m.common)? {
            match &r.worktree {
                Ok(wt) if same_place(wt, path) => return Ok(Some(r.admin.display().to_string())),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!(record = %r.admin.display(), error = %e, "a worktree record names no worktree")
                }
            }
        }
        Ok(None)
    }

    /// Runs the test hook at a restore's step `step` ([`LocalGit::with_step_hook`]).
    #[cfg(test)]
    fn at_step(&self, step: Step, path: &Path) -> anyhow::Result<()> {
        match &self.step_hook {
            Some(h) => (h.0)(step, path).with_context(|| format!("test hook at {step:?}")),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn at_step(&self, _step: Step, _path: &Path) -> anyhow::Result<()> {
        Ok(())
    }

    /// The test hook at step `step` of a blocking job ([`LocalGit::with_step_hook`]), for the
    /// job to run.
    #[cfg(test)]
    fn step_in_job(
        &self,
        step: Step,
        path: &Path,
    ) -> impl FnOnce() -> std::io::Result<()> + Send + 'static {
        let (hook, path) = (self.step_hook.clone(), path.to_path_buf());
        move || match hook {
            Some(h) => (h.0)(step, &path),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn step_in_job(
        &self,
        _step: Step,
        _path: &Path,
    ) -> impl FnOnce() -> std::io::Result<()> + Send + 'static {
        || Ok(())
    }

    /// The test hook ([`LocalGit::with_step_hook`]) for a blocking job that runs it at several
    /// steps (an error it returns fails the operation there).
    #[cfg(test)]
    fn steps_in_job(&self) -> impl Fn(Step, &Path) -> std::io::Result<()> + Send + 'static {
        let hook = self.step_hook.clone();
        move |step, path| match &hook {
            Some(h) => (h.0)(step, path),
            None => Ok(()),
        }
    }

    #[cfg(not(test))]
    fn steps_in_job(&self) -> impl Fn(Step, &Path) -> std::io::Result<()> + Send + 'static {
        |_, _| Ok(())
    }

    /// The steps of [`LocalGit::restore`] that make something, each noted in `made` as soon as
    /// it is there: the branch `branch` at `start` (its reflog written from the item's copy
    /// when it has none), a worktree git adds on it in the claimed directory `path` (its
    /// record found and marked as this restore's before anything is checked out), the
    /// branch's files checked out, the saved files copied over them, the repositories of its
    /// submodules copied into its record (`modules/`, only when it has none) and each
    /// submodule connected to its copy.
    async fn make_again(
        &self,
        m: &TrashManifest,
        dir: &Path,
        branch: &str,
        start: Start<'_>,
        path: &Path,
        made: &mut Made,
    ) -> anyhow::Result<RestoreOutcome> {
        let common = &m.common;
        let new_ref = format!("refs/heads/{branch}");
        let target = start.target;
        // 1. The branch, with its reflog as it was when it has none.
        if start.from_branch && m.branch_log_saved {
            let log = common.join("logs").join(&new_ref);
            if copy_if_absent(&dir.join(REFLOGS).join("branch"), &log)? {
                made.reflog = Some(log);
            }
        }
        self.update_refs(
            common,
            &format!("{RESTORED_MSG} (item {})", m.item),
            format!("create {new_ref} {target}\n"),
        )
        .await?;
        made.branch = Some((branch.to_string(), target.to_string()));
        // 2. A worktree on it (or on the branch the worktree was switched to, as it was), made
        // by git in the claimed (empty) directory, marked as this restore's before anything is
        // in it. Git refuses a directory that is not empty, or one a record names (no
        // `--force`: it clears nothing), and a branch checked out elsewhere.
        let on = match start.checkout {
            Some(Ok((on, _))) => on,
            _ => branch,
        };
        self.at_step(Step::Claimed, path)?;
        let args: Vec<OsString> = vec![
            "worktree".into(),
            "add".into(),
            "--no-checkout".into(),
            "-q".into(),
            path.into(),
            on.into(),
        ];
        self.run(&m.repo, args, Mode::Change(self.timeouts.worktree), None)
            .await?
            .into_stdout("git worktree add")?;
        made.added = true;
        let admin = added_record(common, path, on)
            .context("finding the record git made for the new worktree")?;
        made.record = Some(admin.clone());
        let mark = admin.join(RESTORE_MARK);
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&mark)
            .and_then(|mut f| f.write_all(format!("{}\n{branch}\n{target}\n", m.item).as_bytes()))
            .with_context(|| format!("writing {}", mark.display()))?;
        self.at_step(Step::Added, path)?;
        // 3. The branch's files. On a branch the restore did not make, never through a ref
        //    update: `reset --hard` reads `HEAD` again once the files are written and sets the
        //    branch back to the commit it started from, undoing what another process moved it
        //    to meanwhile; `read-tree` writes the index and the files of the commit alone.
        let args: Vec<OsString> = match start.checkout {
            Some(Ok((_, at))) => vec![
                "read-tree".into(),
                "--reset".into(),
                "-u".into(),
                "--no-recurse-submodules".into(),
                at.into(),
            ],
            _ => ["reset", "--hard", "-q", "--no-recurse-submodules"]
                .iter()
                .map(OsString::from)
                .collect(),
        };
        self.run(path, args, Mode::Change(self.timeouts.worktree), None)
            .await?
            .into_stdout("checking out the branch's files")?;
        // The branch it went back on must still be where the worktree left it (checked before
        // the claim; another process may have moved it since): the saved files are copied over
        // that commit's tree. Otherwise what was made goes, and a restore again puts the
        // worktree on `branch`.
        if let Some(Ok((on, at))) = start.checkout {
            let head = self
                .run(
                    path,
                    vec!["rev-parse".into(), "HEAD".into()],
                    Mode::Read(self.timeouts.read),
                    None,
                )
                .await?
                .into_stdout("git rev-parse HEAD")?;
            if head.trim() != at {
                bail!(
                    "the branch {on} moved (to {}) while the worktree was put back on it, \
                     away from {} where the worktree left it; restoring again puts the \
                     worktree on {branch}",
                    short_id(head.trim()),
                    short_id(at)
                );
            }
        }
        // 4. The saved files over them.
        let mut notes = Vec::new();
        let mut skipped = Vec::new();
        if m.worktree_moved {
            let tracked = self
                .run(
                    path,
                    vec!["ls-files".into(), "-z".into()],
                    Mode::Read(self.timeouts.read),
                    None,
                )
                .await?
                .into_stdout_bytes("git ls-files")?;
            let (saved, into) = (dir.join("worktree"), path.to_path_buf());
            let (item, kept) = (m.item.clone(), m.build_output_aside.clone());
            let hook = self.step_in_job(Step::Copying, path);
            let aside;
            (skipped, aside) = tokio::task::spawn_blocking(move || {
                hook()?;
                let skipped = copy_saved(&saved, &into, &tracked)?;
                // Build output its deletion could neither delete nor put back.
                Ok::<_, std::io::Error>((skipped, put_back_aside(&into, &item, &kept)))
            })
            .await
            .map_err(|e| anyhow!("copying the saved files: {e}"))?
            .context("copying the saved files into the new worktree")?;
            notes.extend(aside);
        } else {
            notes.push(
                "its worktree's directory was gone when it was deleted: the new worktree has the \
                 branch's files only"
                    .to_string(),
            );
        }
        // 5. The repositories of its submodules, into the new record.
        let modules = dir.join("record").join("modules");
        if m.record_moved && !matches!(presence(&modules), Presence::Absent) {
            let (from, to) = (modules.clone(), admin.join("modules"));
            // Without them, the saved submodules' `.git` files would name the old record,
            // which may be another worktree's now: the restore fails instead.
            tokio::task::spawn_blocking(move || copy_new(&from, &to))
                .await
                .map_err(std::io::Error::other)
                .and_then(|r| r)
                .with_context(|| {
                    format!(
                        "copying the repositories of its submodules from {}",
                        modules.display()
                    )
                })?;
        }
        // 6. Each submodule connected to its repository's copy: the saved `.git` files, and
        //    the repositories' `core.worktree`, still name the old places. (With no saved
        //    files, nothing names them.)
        if m.worktree_moved {
            self.reconnect_submodules(m, path, &admin)
                .await
                .context("connecting its submodules to their repositories")?;
        }
        // What does not come back as it was.
        if !start.from_branch {
            notes.push(format!(
                "the thread had no branch of its own in the trash: {branch} starts at its \
                 worktree's HEAD {}",
                short_id(target)
            ));
        }
        if let Some(head) = m.head.as_ref().filter(|_| on == branch) {
            let own = m.branch.as_ref().map(|b| format!("ref: refs/heads/{b}"));
            if own.as_deref() != Some(head.as_str()) {
                let was = match head.strip_prefix("ref: refs/heads/") {
                    Some(b) => format!("on the branch {b}"),
                    None => "detached".to_string(),
                };
                let mut note = format!("its worktree was {was}");
                let why = match start.checkout {
                    Some(Err(why)) => format!(" ({why})"),
                    _ => String::new(),
                };
                if let Some(r) = m.refs.iter().find(|r| r.kind == "head") {
                    note.push_str(&format!(
                        " at {}; it is on {branch} now{why}, and that commit stays in the trash \
                         at {} until the item is purged",
                        short_id(&r.target),
                        r.name
                    ));
                } else {
                    note.push_str(&format!("; it is on {branch} now{why}"));
                }
                notes.push(note);
            }
        }
        if m.index_pack.is_some() {
            notes.push(format!(
                "what its index had staged comes back as unstaged changes (the staged versions \
                 stay in the trash item, {}, until it is purged)",
                dir.join(INDEX_OBJECTS).display()
            ));
        }
        if !skipped.is_empty() {
            let shown: Vec<String> = skipped
                .iter()
                .take(5)
                .map(|p| quote_path(p.as_os_str().as_bytes()))
                .collect();
            notes.push(format!(
                "{} socket(s), FIFO(s) or device(s) were not copied ({}{})",
                skipped.len(),
                shown.join(", "),
                if skipped.len() > shown.len() {
                    ", ..."
                } else {
                    ""
                }
            ));
        }
        Ok(RestoreOutcome {
            repo: m.repo.clone(),
            worktree: path.to_path_buf(),
            branch: branch.to_string(),
            checkout: (on != branch).then(|| on.to_string()),
            notes,
            skipped: skipped
                .iter()
                .take(LISTED_MAX)
                .map(|p| quote_path(p.as_os_str().as_bytes()))
                .collect(),
        })
    }

    /// Connects each submodule of the restored worktree `path` (nested ones too) to its
    /// repository's copy in the new record `admin` (`modules/`, copied from the item): the
    /// submodule's `.git` file, copied with the saved files, and the repository's
    /// `core.worktree` still name where the worktree and its record were (`m.worktree`,
    /// `<m.common>/worktrees/<m.record_name>`), which may now be another worktree's. All or
    /// nothing: an error fails the restore, which then removes what it made, so no submodule
    /// is left naming another worktree's repository.
    ///
    /// Every repository found ([`module_repositories`]) that says where its work tree was
    /// (`core.worktree`, read from the config as held, [`Dir`]; a deinitialized submodule's
    /// says nothing, and has no checkout to connect) must name a directory inside the old
    /// worktree. The same place in the new one is reached from the worktree held, one
    /// directory at a time, no symlink followed; when it is missing, or holds no `.git` file
    /// (a git directory of its own), there is nothing to connect. Otherwise its `.git` file
    /// must name the repository's old place (or its new one). Then the repository's
    /// `core.worktree` names the submodule (`git config --file`, by path: the repository's
    /// directory must be the one held before and after, and the value read back must be the
    /// one written), and the `.git` file the repository (a new file in the submodule's
    /// directory held, renamed over it), both by absolute path; the submodule's directory
    /// must still be where it was found. Last, every `.git` file below the worktree's top
    /// ([`gitfiles`]) that names a place in the repository's git directory or the new record
    /// must be one connected here.
    async fn reconnect_submodules(
        &self,
        m: &TrashManifest,
        path: &Path,
        admin: &Path,
    ) -> anyhow::Result<()> {
        let (Some(new_wt), Some(new_record), Some(common)) = (
            resolve_lenient(path),
            resolve_lenient(admin),
            resolve_lenient(&m.common),
        ) else {
            bail!("the worktree, its record or the repository cannot be resolved");
        };
        // Where its record was (none: no repository to find there, and every `.git` file
        // naming the repository's git directory fails the check below).
        let old_record = m
            .record_name
            .as_deref()
            .map(|n| common.join("worktrees").join(n))
            .unwrap_or_else(|| common.join("worktrees"));
        // Where the worktree was, as written and as resolved now (git wrote the relative
        // paths between real ones).
        let mut old_wts = vec![lexical(&m.worktree)];
        if let Some(r) = resolve_lenient(&m.worktree).filter(|r| *r != old_wts[0]) {
            old_wts.push(r);
        }
        let wt_dir = Dir::open(&new_wt).with_context(|| format!("opening {}", new_wt.display()))?;
        let record_dir =
            Dir::open(&new_record).with_context(|| format!("opening {}", new_record.display()))?;
        let modules = new_record.join("modules");
        let repos = if matches!(presence(&modules), Presence::Absent) || m.record_name.is_none() {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(move || module_repositories(&modules))
                .await
                .map_err(|e| anyhow!("{e}"))?
                .context("listing the repositories of its submodules")?
        };
        // The submodules connected (or found connected), by their place in the worktree.
        let mut connected = std::collections::HashSet::new();
        for rel_repo in repos {
            let shown = rel_repo.display();
            let repo = new_record.join(&rel_repo);
            let held_repo = descend(&record_dir, &rel_repo)
                .with_context(|| format!("opening the submodule repository {shown}"))?
                .ok_or_else(|| anyhow!("the submodule repository {shown} went away"))?;
            // None set (a deinitialized submodule's, kept): no checkout of it to connect. A
            // `.git` file that names it anyway fails the check after this loop.
            let Some(value) = self
                .worktree_of(path, &held_repo)
                .await
                .with_context(|| format!("reading the submodule repository {shown}"))?
            else {
                continue;
            };
            let old_repo = old_record.join(&rel_repo);
            let old_tree = lexical(&old_repo.join(&value));
            let (old_wt, rel) = old_wts
                .iter()
                .find_map(|w| {
                    let rel = old_tree.strip_prefix(w).ok()?;
                    let plain = rel.components().next().is_some()
                        && rel
                            .components()
                            .all(|c| matches!(c, std::path::Component::Normal(_)));
                    plain.then(|| (w, rel.to_path_buf()))
                })
                .ok_or_else(|| {
                    anyhow!(
                        "the submodule repository {shown} names a work tree outside the \
                         worktree ({})",
                        value.display()
                    )
                })?;
            // The submodule's directory in the new worktree, reached from the worktree held.
            let Some(held_tree) = descend(&wt_dir, &rel)
                .with_context(|| format!("opening the submodule at {}", rel.display()))?
            else {
                continue;
            };
            let tree = new_wt.join(&rel);
            let named = match held_tree.open_entry(OsStr::new(".git")) {
                Ok(f) if f.metadata()?.is_file() => {
                    let raw = held_tree.read_small(OsStr::new(".git"), GITFILE_MAX)?;
                    trim_newline(&raw)
                        .strip_prefix(b"gitdir: ")
                        .map(super::path_of)
                        .ok_or_else(|| {
                            anyhow!(
                                "the submodule at {} has a .git file that names no repository",
                                rel.display()
                            )
                        })?
                }
                // A git directory of its own: nothing names the record.
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "reading the .git file of the submodule at {}",
                            rel.display()
                        )
                    })
                }
            };
            if lexical(&tree.join(&named)) == repo && lexical(&repo.join(&value)) == tree {
                connected.insert(rel);
                continue;
            }
            if lexical(&old_wt.join(&rel).join(&named)) != old_repo
                && lexical(&tree.join(&named)) != repo
            {
                bail!(
                    "the submodule at {} names another repository ({})",
                    rel.display(),
                    named.display()
                );
            }
            self.at_step(Step::Connecting, &tree)?;
            // The repository's `core.worktree`, by its path: the directory held must be the
            // one that path reaches before and after, and the value read back the one set.
            let same_repo = || -> anyhow::Result<()> {
                let now = descend(&record_dir, &rel_repo)?;
                if !now.is_some_and(|d| same_dir(&d, &held_repo)) {
                    bail!("the submodule repository {shown} changed while it was connected");
                }
                Ok(())
            };
            same_repo()?;
            let set = vec![
                "config".into(),
                "--file".into(),
                repo.join("config").into(),
                "core.worktree".into(),
                tree.clone().into(),
            ];
            self.run(path, set, Mode::Change(self.timeouts.worktree), None)
                .await?
                .into_stdout("git config core.worktree")?;
            same_repo()?;
            if self.worktree_of(path, &held_repo).await? != Some(tree.clone()) {
                bail!("the submodule repository {shown} changed while it was connected");
            }
            // The `.git` file, in the submodule's directory held.
            write_gitfile(&held_tree, &repo)
                .with_context(|| format!("writing {}", tree.join(".git").display()))?;
            if !descend(&wt_dir, &rel)?.is_some_and(|d| same_dir(&d, &held_tree)) {
                bail!(
                    "the submodule at {} changed while it was connected",
                    rel.display()
                );
            }
            connected.insert(rel);
        }
        // Every `.git` file of the worktree that names a place in the repository's git
        // directory (the old record, another worktree's, the main checkout's submodules) must
        // be one connected above: a submodule whose repository was not found would still
        // name the old record, which may be another worktree's now.
        let files = tokio::task::spawn_blocking(move || gitfiles(&new_wt))
            .await
            .map_err(|e| anyhow!("{e}"))?
            .context("looking for its submodules' .git files")?;
        let resolved = resolve_lenient(path).unwrap_or_else(|| path.to_path_buf());
        for (rel, named) in files {
            if connected.contains(&rel) {
                continue;
            }
            let places = old_wts
                .iter()
                .chain([&resolved])
                .map(|w| lexical(&w.join(&rel).join(&named)));
            if places
                .into_iter()
                .any(|p| p.starts_with(&common) || p.starts_with(&new_record))
            {
                bail!(
                    "the submodule at {} names {}, whose repository the restore did not find \
                     to connect it to",
                    rel.display(),
                    named.display()
                );
            }
        }
        Ok(())
    }

    /// The `core.worktree` of the git directory `repo` holds (its `config`, read through the
    /// directory held and given to git, run in `at`, on its standard input): `None` when it
    /// sets none.
    async fn worktree_of(&self, at: &Path, repo: &Dir) -> anyhow::Result<Option<PathBuf>> {
        let config = repo.read_small(OsStr::new("config"), 1 << 20)?;
        let get = vec![
            "config".into(),
            "--file".into(),
            "-".into(),
            "--get".into(),
            "core.worktree".into(),
        ];
        let out = self
            .run(at, get, Mode::Read(self.timeouts.read), Some(config))
            .await?;
        match out.code {
            Some(0) => Ok(Some(super::path_of(trim_newline(&out.stdout)))),
            Some(1) if out.stderr.trim().is_empty() => Ok(None),
            _ => bail!("git config: {}", out.stderr.trim()),
        }
    }

    /// Removes what a restore made (`made`) in the repository whose git common directory is
    /// `common` (main checkout `repo`), and nothing else:
    ///
    /// - git added the worktree: when the directory is still the claimed one and the record
    ///   still names it, `git worktree remove --force --force` of it; then what is left of the
    ///   record (only while it names the worktree) and of the directory (only while it is the
    ///   claimed one).
    /// - git did not: the claimed directory only while it is empty (`rmdir`, never
    ///   recursive).
    /// - the directories above it the restore made, only while empty;
    /// - the branch only while it points where the restore put it and is checked out nowhere
    ///   (its reflog goes with it), else the reflog the restore wrote, while the branch is
    ///   not there.
    ///
    /// When `git worktree remove` is killed past its timeout, nothing more goes: the worktree,
    /// its record and the branch are left as they are (an error says so). The trash item is
    /// not touched.
    async fn unmake(&self, common: &Path, repo: &Path, made: &Made) -> Result<(), String> {
        let mut failed: Vec<String> = Vec::new();
        if let Some(c) = &made.claim {
            let record = made
                .record
                .as_deref()
                .filter(|r| names_worktree(r, &c.path));
            if made.added {
                // Judged before anything goes: an undo's proof is the `.git` naming the record.
                let ours = c.holds(record);
                if ours && record.is_some() {
                    let args: Vec<OsString> = vec![
                        "worktree".into(),
                        "remove".into(),
                        "--force".into(),
                        "--force".into(),
                        c.path.clone().into(),
                    ];
                    match self
                        .run(repo, args, Mode::Change(self.timeouts.worktree), None)
                        .await
                    {
                        Ok(o) if o.ok => {}
                        Ok(o) => tracing::debug!(error = %o.stderr.trim(), "git worktree remove"),
                        // Killed part way: the worktree, its record and the branch checked out
                        // in it are left as they are.
                        Err(e) if was_killed(&e) => {
                            return Err(format!(
                                "{e:#}; {}, its record and the branch stay as they are: check \
                                 them, and remove them by hand",
                                c.path.display()
                            ));
                        }
                        Err(e) => tracing::debug!(error = %format!("{e:#}"), "git worktree remove"),
                    }
                }
                let mut dirs = Vec::new();
                match &made.record {
                    Some(r) if names_worktree(r, &c.path) => dirs.push(r.clone()),
                    Some(r) if may_exist(r) => failed.push(format!(
                        "the worktree record {} does not name {} (any more): it stays",
                        r.display(),
                        c.path.display()
                    )),
                    Some(_) => {}
                    None => failed.push(format!(
                        "the record git made for {} is not known: it stays",
                        c.path.display()
                    )),
                }
                // Still the claimed directory (an undo's proof was taken above).
                let still = match c.made {
                    Some(_) => c.holds(None),
                    None => ours,
                };
                if still {
                    dirs.push(c.path.clone());
                } else if may_exist(&c.path) {
                    failed.push(format!(
                        "{} is not the directory the restore made (any more): it stays",
                        c.path.display()
                    ));
                }
                let removed = tokio::task::spawn_blocking(move || {
                    dirs.iter()
                        .filter_map(|d| {
                            remove_tree(d)
                                .err()
                                .map(|e| format!("{}: {e}", d.display()))
                        })
                        .collect::<Vec<_>>()
                })
                .await
                .unwrap_or_else(|e| vec![format!("removing the new worktree: {e}")]);
                failed.extend(removed);
            } else if c.holds(None) {
                if let Err(e) = std::fs::remove_dir(&c.path) {
                    if !is_absent(&e) {
                        failed.push(format!("{}: {e}; it stays", c.path.display()));
                    }
                }
            }
            c.remove_parents();
        }
        if let Some((b, target)) = &made.branch {
            let name = format!("refs/heads/{b}");
            match self.ref_target(common, &name).await {
                Ok(Some(t)) if t == *target => match checked_out(common, repo, b, None) {
                    Ok(None) => {
                        if let Err(e) = self
                            .update_refs(
                                common,
                                "twr: restore undone",
                                format!("delete {name} {target}\n"),
                            )
                            .await
                        {
                            failed.push(format!("{name}: {e:#}"));
                        }
                    }
                    Ok(Some(at)) => failed.push(format!(
                        "the branch {b} is checked out at {}: it stays",
                        at.display()
                    )),
                    Err(e) => failed.push(format!("{e:#}")),
                },
                Ok(Some(t)) => failed.push(format!(
                    "the branch {b} points at {t} now, not at {target} as restored: it stays"
                )),
                Ok(None) => {}
                Err(e) => failed.push(format!("{name}: {e:#}")),
            }
        }
        if let Some(log) = &made.reflog {
            let name = log
                .strip_prefix(common.join("logs"))
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if matches!(self.ref_target(common, &name).await, Ok(None)) {
                if let Err(e) = std::fs::remove_file(log) {
                    if !is_absent(&e) {
                        failed.push(format!("{}: {e}", log.display()));
                    }
                }
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// [`GitOps::trash_restore_undo`]: removes the worktree and branch a restore of item
    /// `item` made (found by the mark in the worktree's record), when the hub could not record
    /// that restore. Reserved like a removal: no runtime of this box works in the worktree,
    /// and none starts in it meanwhile. The item is not touched.
    ///
    /// [`GitOps::trash_restore_undo`]: twr_core::git::GitOps::trash_restore_undo
    pub(super) async fn restore_undo(self, item: String) -> anyhow::Result<bool> {
        let root = self.trash_dir()?.to_path_buf();
        let dir = item_dir(&root, &item).map_err(|e| anyhow!(e))?;
        let _lock = lock_item(&dir, false).await?;
        let m = read_manifest(&dir, &item).map_err(|e| anyhow!(e))?;
        let Some((admin, wt, mark)) = restored_copy(&m) else {
            return Ok(false);
        };
        let present = !matches!(presence(&wt), Presence::Absent);
        if present && !linked_to(&wt, &admin) {
            bail!(
                "{} is not the worktree restoring trash item {item} made (its .git names another \
                 record): nothing is removed",
                wt.display()
            );
        }
        let _change = self
            .workdirs
            .begin_change(&wt, ChangeKind::Remove, CHANGE_WAIT)
            .await
            .map_err(|b| anyhow!("cannot remove the restored worktree {}: {b}", wt.display()))?;
        // Ours by the mark: the marked record names `wt` (as `restored_copy` found it), and
        // `wt`'s `.git` names that record (checked above, and again by `unmake`).
        let made = Made {
            reflog: None,
            branch: Some((mark.branch, mark.target)),
            claim: Some(Claim {
                path: wt,
                made: None,
                parents: Vec::new(),
            }),
            added: true,
            record: Some(admin),
        };
        self.unmake(&m.common, &m.repo, &made)
            .await
            .map_err(|e| anyhow!("removing what restoring trash item {item} made: {e}"))?;
        Ok(true)
    }
}

/// Removes `dir` and the empty directories under it (never a file).
fn remove_empty_dirs(dir: &Path) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if is_absent(&e) => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            remove_empty_dirs(&entry.path())?;
        }
    }
    std::fs::remove_dir(dir)
}

/// The items of the trash `root` whose manifests read, oldest first; the rest are skipped
/// (with a warning).
fn list_items(root: &Path) -> anyhow::Result<Vec<TrashItem>> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if is_absent(&e) => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("reading {}: {e}", root.display())),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let found = item_dir(root, &name).and_then(|dir| {
            read_manifest(&dir, &name).map(|manifest| TrashItem {
                restored: restored_copy(&manifest).map(|(_, worktree, mark)| RestoredCopy {
                    worktree,
                    branch: mark.branch,
                }),
                dir,
                manifest,
                kept: None,
            })
        });
        match found {
            Ok(i) => out.push(i),
            Err(e) => {
                tracing::warn!(entry = %entry.path().display(), error = %e, "skipping an entry of the trash")
            }
        }
    }
    out.sort_by(|a, b| {
        a.manifest
            .info
            .deleted_at_ms
            .cmp(&b.manifest.info.deleted_at_ms)
            .then(a.manifest.item.cmp(&b.manifest.item))
    });
    Ok(out)
}

impl LocalGit {
    /// [`GitOps::worktree_trim`](twr_core::git::GitOps::worktree_trim): the worktree's build
    /// output (of it only what [`TrimRequest::only`](twr_core::git::TrimRequest::only) names
    /// too, through the same directory or file name, when it names any: an idle trim's,
    /// [`disposable_in_both`]) deleted the way a deletion deletes it
    /// ([`drop_build_output`]), under a
    /// [`ChangeKind::Trim`] of its path. What a stopped deletion or trim left aside is put back
    /// first ([`reconcile_staged`]); what this one cannot delete nor put back stays aside,
    /// written down in the worktree's record for the next one, and is said.
    pub(super) async fn trim(
        self,
        req: twr_core::git::TrimRequest,
    ) -> anyhow::Result<twr_core::git::TrimOutcome> {
        let twr_core::git::TrimRequest {
            repo,
            worktree: path,
            branch,
            other_branches,
            only,
        } = req;
        let _change = self
            .workdirs
            .begin_change(&path, ChangeKind::Trim, CHANGE_WAIT)
            .await
            .map_err(|b| anyhow!("cannot trim the worktree {}: {b}", path.display()))?;
        match tokio::fs::symlink_metadata(&path).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => bail!("{} is not a directory", path.display()),
            Err(e) if is_absent(&e) => bail!("the worktree {} is not there", path.display()),
            Err(e) => bail!("cannot read the worktree {}: {e}", path.display()),
        }
        // Never anything but a linked worktree of the repository, proven by its record.
        self.check_linked_worktree(&repo, &path)
            .await
            .map_err(|e| {
                anyhow!(
                    "{}",
                    format!("{e:#}").replacen("not deleting", "not trimming", 1)
                )
            })?;
        let common = self.common_dir(&repo).await?;
        let others = self.branches_in(&common, &other_branches).await;
        let records = worktree_records(&common)?;
        let record = record_of(
            &common,
            &records,
            &path,
            branch.as_deref(),
            others.as_deref(),
        )
        .map_err(|e| anyhow!("not trimming {}: {e}", path.display()))?
        .map(|k| records[k].admin.clone())
        .ok_or_else(|| {
            anyhow!(
                "not trimming {}: git has no record of it as a worktree",
                path.display()
            )
        })?;
        // Deleting through a mount point would take that file system's files: only where they
        // can be told (Linux and macOS), and none there.
        let at = path.clone();
        match tokio::task::spawn_blocking(move || mounts_inside(&at))
            .await
            .context("looking for mount points in the worktree")?
        {
            None => bail!(
                "twr trims build output only where it can list the mount points (Linux and macOS)"
            ),
            Some(Err(e)) => bail!("{e}"),
            Some(Ok(m)) if !m.is_empty() => bail!(
                "a file system is mounted in the worktree at {}: unmount it first",
                m.join(", ")
            ),
            Some(Ok(_)) => {}
        }
        let mut out = twr_core::git::TrimOutcome::default();
        if self.disposable.is_empty() {
            out.notes
                .push("[threads] park_disposable is empty: nothing counts as build output".into());
            return Ok(out);
        }
        if only.as_ref().is_some_and(|o| o.is_empty()) {
            out.notes
                .push("idle_trim_disposable is empty: an idle trim deletes no build output".into());
            return Ok(out);
        }
        let owner = twr_core::event::new_id();
        let journal = record.join(JOURNAL);
        let (root, j, o, hook) = (
            path.clone(),
            journal.clone(),
            owner.clone(),
            self.steps_in_job(),
        );
        let kept = tokio::task::spawn_blocking(move || reconcile_staged(&root, &j, &o, &hook))
            .await
            .map_err(|e| anyhow!("{e}"))
            .and_then(|r| r)
            .context("putting back build output a deletion left aside")?;
        let tree = self.tree_files(&path).await?;
        let (root, list, mut budget, hook) = (
            path.clone(),
            self.disposable.clone(),
            self.search_max,
            self.steps_in_job(),
        );
        tokio::task::spawn_blocking(move || {
            let build: Vec<String> = tree
                .ignored
                .into_iter()
                .filter(|p| match &only {
                    None => disposable(p, &list),
                    Some(o) => disposable_in_both(p, &list, o),
                })
                .collect();
            let sizes: HashMap<String, u64> = build
                .iter()
                .map(|p| {
                    let at = root.join(path_of(&unquote_path(p.trim_end_matches('/'))));
                    // A directory walked, a file (an ignored `.DS_Store`) by its own blocks.
                    let bytes = match std::fs::symlink_metadata(&at) {
                        Ok(m) if m.is_dir() => worktree_disk(&at, &[]).bytes,
                        Ok(m) => m.blocks().saturating_mul(512),
                        Err(_) => 0,
                    };
                    (p.clone(), bytes)
                })
                .collect();
            let dropped =
                drop_build_output(&root, &build, &owner, &journal, kept, &mut budget, &hook)?;
            out.freed = dropped
                .deleted
                .iter()
                .map(|p| sizes.get(p).copied().unwrap_or(0))
                .sum();
            out.deleted = dropped.deleted;
            out.notes = dropped
                .skipped
                .iter()
                .map(|(p, why)| format!("its build output {p} stays: {why}"))
                .collect();
            // What could not be deleted goes back where it was; what cannot stays aside, still
            // written down for the next deletion or trim.
            out.notes
                .extend(put_back_aside(&root, &owner, &dropped.aside));
            let staged: Vec<KeptAside> = dropped
                .aside
                .into_iter()
                .filter(|k| std::fs::symlink_metadata(root.join(&k.at)).is_ok())
                .collect();
            // What went is said whatever happens to the journal now.
            if let Err(e) = write_journal(
                &journal,
                &Journal {
                    item: owner,
                    previous: Vec::new(),
                    staged,
                },
            ) {
                out.notes.push(format!(
                    "could not update the worktree record's journal of build output kept aside: {e}"
                ));
            }
            Ok::<_, std::io::Error>(out)
        })
        .await
        .map_err(|e| anyhow!("{e}"))?
        .context("deleting the worktree's build output")
    }
}

/// [`GitOps::disk_usage`](twr_core::git::GitOps::disk_usage)'s walk of a worktree at `root`:
/// blocks allocated under it, a file with several links counted once, and how much of it is
/// build output by name (`list`, `[threads] park_disposable`: an entry `name/` names
/// directories, `name` any entry), at most [`twr_core::git::DISK_WALK_MAX`] entries (`partial`
/// past them). Each directory is held open and its entries reached from it ([`Dir`]), no
/// symlink followed: one that took a directory's place meanwhile, and what cannot be read, is
/// not counted, and makes the reading `partial`.
pub(super) fn worktree_disk(root: &Path, list: &[String]) -> twr_core::git::WorktreeDisk {
    disk_walk(root, list, twr_core::git::DISK_WALK_MAX)
}

/// [`worktree_disk`] with at most `max` entries.
pub(super) fn disk_walk(root: &Path, list: &[String], max: u64) -> twr_core::git::WorktreeDisk {
    use std::rc::Rc;
    let build_name = |name: &OsStr, dir: bool| {
        list.iter().any(|e| match e.strip_suffix('/') {
            Some(d) => dir && d.as_bytes() == name.as_bytes(),
            None => e.as_bytes() == name.as_bytes(),
        })
    };
    let mut out = twr_core::git::WorktreeDisk::default();
    // Files with several links count once (a build's hard-linked copies of one binary).
    let mut linked = std::collections::HashSet::new();
    let size = |m: &std::fs::Metadata| m.blocks().saturating_mul(512);
    let Ok(top) = Dir::open(root) else {
        out.partial = true;
        return out;
    };
    // Entries found so far (counted as they are listed, so a huge directory stops the walk
    // while it is read, and what waits to be read stays bounded too).
    let mut found = 1u64;
    // The walk reached its limit: nothing more is listed.
    let mut cut = false;
    // Directories to read, each named in its parent held open: (parent, name, inside build
    // output). A directory is opened when its turn comes, and stays open only while entries of
    // it wait, so at most about the tree's depth is open at once.
    // A directory to read: named in its parent held open (`None`: the top).
    type At = Option<(Rc<Dir>, OsString)>;
    let mut todo: Vec<(At, bool)> = vec![(None, false)];
    let mut top = Some(top);
    while let Some((at, build)) = todo.pop() {
        let d = match at {
            None => match top.take() {
                Some(t) => t,
                None => continue,
            },
            // Reached from the directory held, never through a symlink.
            Some((parent, name)) => match parent.dir(&name) {
                Ok(d) => d,
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(_) => {
                    out.partial = true;
                    continue;
                }
            },
        };
        match d.file().metadata() {
            Ok(m) => {
                out.bytes = out.bytes.saturating_add(size(&m));
                if build {
                    out.build_bytes = out.build_bytes.saturating_add(size(&m));
                }
            }
            Err(_) => out.partial = true,
        }
        if cut {
            continue;
        }
        let d = Rc::new(d);
        // Read as it is walked: past the limit, nothing more of it is read.
        let names = match d.entries() {
            Ok(n) => n,
            Err(_) => {
                out.partial = true;
                continue;
            }
        };
        for name in names {
            if found >= max {
                out.partial = true;
                cut = true;
                break;
            }
            let Ok(name) = name else {
                out.partial = true;
                break;
            };
            found += 1;
            let meta = match std::fs::symlink_metadata(d.path().join(&name)) {
                Ok(m) => m,
                // Gone meanwhile (a build at work): nothing to count.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(_) => {
                    out.partial = true;
                    continue;
                }
            };
            let inside = build || build_name(&name, meta.is_dir());
            if meta.is_dir() {
                todo.push((Some((Rc::clone(&d), name)), inside));
                continue;
            }
            if meta.nlink() > 1 && !linked.insert((meta.dev(), meta.ino())) {
                continue;
            }
            out.bytes = out.bytes.saturating_add(size(&meta));
            if inside {
                out.build_bytes = out.build_bytes.saturating_add(size(&meta));
            }
        }
    }
    out
}

/// Serializes the walks of [`worktree_disk`] on a box: one at a time, whoever asks and
/// whether or not they still wait (a remote request that timed out), so measuring never
/// loads the machine with several walks at once.
pub(super) static DISK_WALK: std::sync::Mutex<()> = std::sync::Mutex::new(());

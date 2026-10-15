//! Git on a box: the [`GitOps`] trait the hub drives worktrees and snapshots through, the
//! pure `git status --porcelain=v2 --branch` parser behind `git.snapshot`, and the thread-name
//! rules that keep `twr/<name>` a valid branch.
//!
//! The trait lives here (not in the hub) because the box that owns a repository does the git
//! work: the hub's local box (`twr_runner::local::LocalGit`) and a runner on a
//! remote box implement the same calls, and the hub forwards to whichever box a thread
//! runs on. Arguments are owned so a remote implementation can ship them over the wire.

use std::path::PathBuf;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::event::GitSnapshot;

/// Prefix of the branch of every hub-managed worktree: thread `api-1` works on `twr/api-1`.
pub const BRANCH_PREFIX: &str = "twr/";

/// How long a git command that changes a repository or a work tree may run on a box before
/// it is killed with its process group (and reaped): `LocalGit`'s limit for `worktree add` /
/// `remove`, `update-ref`, `branch -D`, `reset --hard`, `pack-objects`.
pub const GIT_CHANGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// How long the hub waits for a box's answer to an op that changes a repository
/// ([`crate::runner::Op::changes_repository`]): twice [`GIT_CHANGE_TIMEOUT`], room for the
/// changing command and the reads around it, so that normally an answer (sent only once every
/// git the op started is reaped) comes first. When it does not, or the connection drops, the
/// op's outcome is unknown and the hub runs no other git op on that box until the runner
/// answers it or connects again (a runner finishes such ops before it reconnects).
pub const CHANGE_REQUEST_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(GIT_CHANGE_TIMEOUT.as_secs() * 2);

// The hub's wait outlasts the box's kill-and-reap bound of a changing command.
const _: () = assert!(CHANGE_REQUEST_TIMEOUT.as_secs() > GIT_CHANGE_TIMEOUT.as_secs());

/// The branch of thread `name`'s worktree.
pub fn thread_branch(name: &str) -> String {
    format!("{BRANCH_PREFIX}{name}")
}

/// Why a name cannot be a git branch component.
///
/// Implements the `git check-ref-format` rules for one path component, which is all a thread
/// name becomes (`twr/<name>`): not empty, no control characters, space, `~ ^ : ? * [ \` or
/// `/`, no `..` or `@{`, not `@`, not starting with `.` or `-`, not ending with `.` or
/// `.lock`.
pub fn branch_component_error(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        return Some("is empty");
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Some("has a character git does not allow in branch names");
    }
    if name.contains('/') {
        return Some("contains '/'");
    }
    if name.starts_with('.') {
        return Some("starts with '.'");
    }
    if name.starts_with('-') {
        return Some("starts with '-'");
    }
    if name.ends_with('.') {
        return Some("ends with '.'");
    }
    if name.ends_with(".lock") {
        return Some("ends with \".lock\"");
    }
    if name.contains("..") {
        return Some("contains \"..\"");
    }
    if name.contains("@{") {
        return Some("contains \"@{\"");
    }
    if name == "@" {
        return Some("is \"@\"");
    }
    None
}

/// Whether `name` can be a git branch component (see [`branch_component_error`]).
pub fn valid_branch_component(name: &str) -> bool {
    branch_component_error(name).is_none()
}

/// A worktree to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSpec {
    /// The repository (any directory of its main work tree).
    pub repo: PathBuf,
    /// Where the worktree goes (must not exist, or be an empty directory).
    pub path: PathBuf,
    /// Branch to check out there. Created from `base` when it does not exist; an existing
    /// branch is checked out as it is (then `base` must be `None`).
    pub branch: String,
    /// Start point of a new branch (default: the repository's `HEAD`).
    pub base: Option<String>,
}

/// What [`GitOps::worktree_create`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeCreated {
    /// Whether the branch was created (else an existing one was checked out).
    pub new_branch: bool,
    /// The commit the worktree starts at.
    pub head: String,
    /// What the thread's work merges back into, as far as twr can tell, as a name that means
    /// the same from any checkout, fully qualified: the branch the repository was on (its
    /// `HEAD` the base, `refs/heads/dev`), the branch, remote branch or tag given
    /// (`refs/remotes/origin/main`, `refs/tags/v1`), else the commit the base named. `None` for
    /// an existing branch checked out.
    pub base_ref: Option<String>,
}

/// What [`GitOps::worktree_remove`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoveOutcome {
    /// Removed (its branch is kept).
    Removed,
    /// Kept because it has uncommitted changes or untracked files (never forced).
    KeptDirty {
        /// Dirty files counted like [`GitSnapshot::dirty_files`].
        dirty_files: u64,
    },
    /// There was nothing at that path.
    Missing,
    /// Kept because its checkout is not the thread's branch (a detached `HEAD`, or another
    /// branch): removing it would drop the only reflog that remembers commits made there.
    KeptCheckout {
        /// What is checked out: `branch <name>` or `detached at <commit>`.
        head: String,
    },
    /// Kept because removing it would lose something `git status` does not show: tracked files
    /// hidden from it (skip-worktree / assume-unchanged), ignored files that are not known to
    /// be regenerable (`.env`, ...), a git repository in a tracked directory, or commits only
    /// the worktree's own `HEAD`, reflogs or refs remember.
    Kept {
        /// Why, for the user (e.g. `ignored files not known to be disposable: .env`).
        reason: String,
    },
}

/// Prefix of the refs a trash item keeps in its repository: `refs/twr-trash/<item>/...`
/// ([`GitOps::worktree_trash`]). Every other ref twr reads as one that stays counts these out:
/// what only the trash keeps is lost once the item is purged.
pub const TRASH_REF_PREFIX: &str = "refs/twr-trash/";

/// Whether `id` can name a trash item ([`TrashRequest::item`]): a ULID as the hub makes them,
/// 26 of `[0-9A-Z]`. Every path and ref of an item is built from it, and a purge touches only
/// what such an id names.
pub fn valid_trash_id(id: &str) -> bool {
    id.len() == 26
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

/// What the hub knows of a thread it moves to the trash ([`TrashRequest::info`]), kept in the
/// item's manifest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashInfo {
    /// The thread's id.
    pub thread: String,
    /// The thread's name.
    pub thread_name: String,
    /// Its project.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// The box it ran on.
    pub box_name: String,
    /// Its runtime.
    pub runtime: String,
    /// Its branch existed before the thread.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub existing_branch: bool,
    /// When the user's deletion moved it to the trash (ms since the epoch, the hub's clock).
    pub deleted_at_ms: u64,
    /// What the delete card said it loses, in short (`3 uncommitted files, 2 unpushed
    /// commits`).
    pub losses: String,
}

/// A thread's worktree and branch to move to the trash ([`GitOps::worktree_trash`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashRequest {
    /// The new item's id ([`valid_trash_id`]).
    pub item: String,
    /// The repository (any checkout of it).
    pub repo: PathBuf,
    /// The thread's linked worktree (it may be gone already: its record still moves).
    pub worktree: PathBuf,
    /// The thread's branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The recorded branches of the box's other threads ([`OtherBranches`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_branches: OtherBranches,
    /// For the manifest.
    pub info: TrashInfo,
}

/// The recorded branches of the other threads of a box, which tell a worktree a thread
/// switched to another branch of its own from another thread's checkout
/// ([`TrashRequest::other_branches`], [`DeleteRefs::other_branches`],
/// [`RestoreRequest::other_branches`]). A thread's own worktree (its path, a record git made
/// for that path, and git's own record of it leaving the thread's branch) whose `HEAD` is on
/// another branch is still its own unless that branch is one of these, in the same repository
/// (git common directory); `None` (a hub from before these were sent): unless it is any other
/// `twr/` branch, and a restore puts the worktree on the thread's branch.
pub type OtherBranches = Option<Vec<OtherBranch>>;

/// A branch another thread records ([`OtherBranches`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OtherBranch {
    /// That thread's repository, as it names it (any checkout of it).
    pub repo: PathBuf,
    /// Its recorded branch.
    pub branch: String,
}

/// What [`GitOps::worktree_trash`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashOutcome {
    /// The item made (`None`: there was nothing to keep: no worktree, record or branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    /// Its directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<PathBuf>,
    /// The worktree's directory moved into the item.
    pub worktree_moved: bool,
    /// Part of it stayed at its path ([`TrashManifest::worktree_left`]).
    pub worktree_left: bool,
    /// Build output that could not be deleted nor put back ([`TrashManifest::build_output_aside`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build_output_aside: Vec<String>,
    /// The branch was deleted (the item keeps it).
    pub branch_deleted: bool,
    /// The refs the item keeps.
    pub refs: Vec<String>,
    /// Bytes on disk the item holds.
    pub bytes: u64,
}

/// A ref a trash item keeps ([`TrashManifest::refs`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashRef {
    /// Its full name, under `refs/twr-trash/<item>/`.
    pub name: String,
    /// The object it points at.
    pub target: String,
    /// What it keeps: `branch` (the branch's tip, its reflog copied), `head` (the worktree's
    /// `HEAD`, its reflog copied), `worktree-ref` (one of the worktree's own refs), or
    /// `reflog` (a commit only a reflog named).
    pub kind: String,
}

/// The manifest of a trash item, `<trash>/<item>/manifest.json`: what the item holds and how
/// to get it back. Written when the item is made (`complete: false`) and again once
/// everything moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashManifest {
    /// The item's id ([`valid_trash_id`]); its directory's name.
    pub item: String,
    /// What the hub knew of the thread.
    #[serde(flatten)]
    pub info: TrashInfo,
    /// The repository, as the thread named it.
    pub repo: PathBuf,
    /// Its git common directory, where the item's refs are.
    pub common: PathBuf,
    /// Where the worktree was.
    pub worktree: PathBuf,
    /// The name of the worktree's administrative record (`<common>/worktrees/<name>`), when
    /// it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_name: Option<String>,
    /// The thread's branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The worktree's `HEAD` as it was: `ref: refs/heads/<branch>` or a commit id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The refs the item keeps in the repository.
    pub refs: Vec<TrashRef>,
    /// The worktree's directory moved into the item (`worktree/`).
    pub worktree_moved: bool,
    /// Its administrative record moved into the item (`record/`).
    pub record_moved: bool,
    /// Part of the worktree's directory stayed where it was (it could not all be moved: files
    /// that may not be removed after a copy to another file system, or a special file).
    pub worktree_left: bool,
    /// The branch was deleted (its tip and reflog are in the item's refs).
    pub branch_deleted: bool,
    /// Everything moved (false while it is being made, or when that failed part way: what
    /// did move is here, the rest is where it was).
    pub complete: bool,
    /// Bytes on disk the item holds.
    pub bytes: u64,
    /// How to get it back by hand.
    pub recover: String,
    /// The pack in the item (`index-objects/pack-<this>.pack`) holding every object the
    /// worktree's index named (staged content of every stage, its cache tree, resolve-undo
    /// and sparse directory trees) that none of the item's refs keeps: with the record out of
    /// the repository, `git gc` no longer sees that index. A restore does not bring the
    /// index back (staged changes come back as unstaged ones): these objects stay in the item
    /// until it is purged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_pack: Option<String>,
    /// The branch's reflog was copied verbatim into the item (`reflogs/branch`), where no
    /// `git gc` expires it: a restore writes the new branch's reflog from that copy (when the
    /// new branch has none yet).
    pub branch_log_saved: bool,
    /// The worktree's build output deleted instead of moved into the item (ignored entries of
    /// `[threads] park_disposable`, as `git status` names them): regenerable, so a restore
    /// rebuilds it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build_output: Vec<String>,
    /// Build output that was to be deleted but could not be, nor put back where it was (its
    /// place taken meanwhile): it is in the item's worktree where [`KeptAside::at`] says, and a
    /// restore puts it back there (only these, from a directory whose `owner` names this item).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub build_output_aside: Vec<KeptAside>,
}

/// Build output a deletion kept aside ([`TrashManifest::build_output_aside`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct KeptAside {
    /// Where it was, as `git status` named it (`target/`).
    pub path: String,
    /// Where it is in the worktree: `.twr-build-output.<n>/<slot>`.
    pub at: String,
}

/// A trash item as its box lists it ([`GitOps::trash_list`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashItem {
    /// Its directory.
    pub dir: PathBuf,
    /// Its manifest.
    pub manifest: TrashManifest,
    /// A worktree a restore of the item made in its repository, found by the restore's mark
    /// in that worktree's record: what [`GitOps::trash_restore_undo`] removes when the hub did
    /// not record the restore. It stays marked once the hub has recorded it, until the
    /// restored thread is deleted again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restored: Option<RestoredCopy>,
    /// Why an incomplete item is never purged, e.g. "holds the only copy of 3 commits"
    /// (what its deletion moved, or commits nothing else keeps); `None` for a complete item,
    /// or an incomplete one a purge may take once it is abandoned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept: Option<String>,
}

/// A worktree and branch a restore of a trash item made ([`TrashItem::restored`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoredCopy {
    /// The worktree's path.
    pub worktree: PathBuf,
    /// The branch the restore created.
    pub branch: String,
}

/// Which trash items to purge ([`GitOps::trash_purge`]): those named, and those moved to the
/// trash before `before_ms`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashPurge {
    /// Items by id.
    pub items: Vec<String>,
    /// Every item whose [`TrashInfo::deleted_at_ms`] is before this (ms since the epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_ms: Option<u64>,
}

/// An item a purge could not remove.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrashFailure {
    /// The item.
    pub item: String,
    /// Why.
    pub error: String,
}

/// What [`GitOps::trash_purge`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurgeOutcome {
    /// The items removed (refs and directory).
    pub purged: Vec<String>,
    /// Named items that are not in the trash.
    pub unknown: Vec<String>,
    /// Items that could not be removed (they stay whole or with their refs).
    pub failed: Vec<TrashFailure>,
}

/// A trash item to make a thread's worktree and branch again from ([`GitOps::trash_restore`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreRequest {
    /// The item.
    pub item: String,
    /// The branch to create from the item's branch ref, or its `HEAD` ref when it keeps no
    /// branch (`twr/<name>`; it must not exist).
    pub branch: String,
    /// Where the new worktree may go, in order: the first that does not exist is taken.
    pub paths: Vec<PathBuf>,
    /// The recorded branches of the box's live threads ([`OtherBranches`]): the worktree goes
    /// back on the branch it was switched to only when that is none of theirs. `None` (an
    /// older hub, which does not read [`RestoreOutcome::checkout`]): always on `branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_branches: OtherBranches,
}

/// What [`GitOps::trash_restore`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RestoreOutcome {
    /// The repository, as the thread named it.
    pub repo: PathBuf,
    /// Where the new worktree is.
    pub worktree: PathBuf,
    /// The branch created (it is checked out in the new worktree, unless `checkout` says
    /// otherwise).
    pub branch: String,
    /// The branch checked out in the new worktree when it is not `branch`: the one the
    /// thread's worktree was on when it was deleted ([`GitOps::trash_restore`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout: Option<String>,
    /// What did not come back as it was, for people: a `HEAD` that was detached or on
    /// another branch, staged changes, special files, submodule repositories.
    pub notes: Vec<String>,
    /// Sockets, FIFOs and devices of the saved files, not copied (quoted paths relative to
    /// the worktree, at most [`LISTED_MAX`]).
    pub skipped: Vec<String>,
}

/// Most items a [`Listed`] keeps.
pub const LISTED_MAX: usize = 200;

/// Most groups [`WorktreeInventory::ignored_groups`] keeps.
pub const IGNORED_GROUPS_MAX: usize = 20;

/// A file name git printed (raw bytes, not necessarily UTF-8) as a string that names it and no
/// other: UTF-8 text is kept, while a backslash (`\\`), a control character (`\t`, `\n`, `\r`,
/// `\xNN`, or `\u{NN}` past ASCII) and each byte that is not UTF-8 (`\xNN`) are escaped, so two
/// names never read the same ([`unquote_path`] gives the bytes back). What an inventory
/// ([`WorktreeInventory`]) lists, compares and shows.
pub fn quote_path(raw: &[u8]) -> String {
    let mut out = String::with_capacity(raw.len());
    for chunk in raw.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if c.is_ascii_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
                c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

/// The raw bytes of a name [`quote_path`] gave; an escape it never writes is kept as it is.
pub fn unquote_path(quoted: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(quoted.len());
    let mut rest = quoted;
    while let Some(i) = rest.find('\\') {
        out.extend_from_slice(&rest.as_bytes()[..i]);
        let esc = &rest[i + 1..];
        let (bytes, len): (Option<Vec<u8>>, usize) = match esc.as_bytes().first() {
            Some(b'\\') => (Some(vec![b'\\']), 1),
            Some(b't') => (Some(vec![b'\t']), 1),
            Some(b'n') => (Some(vec![b'\n']), 1),
            Some(b'r') => (Some(vec![b'\r']), 1),
            Some(b'x') => (
                esc.get(1..3)
                    .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                    .map(|b| vec![b]),
                3,
            ),
            Some(b'u') => match esc.find('}') {
                Some(end) => (
                    esc.get(1..end)
                        .and_then(|h| h.strip_prefix('{'))
                        .filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
                        .and_then(|h| u32::from_str_radix(h, 16).ok())
                        .and_then(char::from_u32)
                        .map(|c| c.to_string().into_bytes()),
                    end + 1,
                ),
                None => (None, 0),
            },
            _ => (None, 0),
        };
        match bytes {
            Some(b) => {
                out.extend_from_slice(&b);
                rest = &esc[len..];
            }
            None => {
                out.push(b'\\');
                rest = esc;
            }
        }
    }
    out.extend_from_slice(rest.as_bytes());
    out
}

/// A list of names (paths, commit ids) kept to its first [`LISTED_MAX`] in sorted order, with
/// how many there are in all: what a delete card shows and remembers of a thread's losses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Listed {
    /// The first [`LISTED_MAX`] items, sorted, each once.
    pub items: Vec<String>,
    /// How many items there are in all.
    pub total: u64,
}

impl Listed {
    /// `items` sorted, without repeats, cut to [`LISTED_MAX`].
    pub fn new(items: impl IntoIterator<Item = String>) -> Listed {
        let mut items: Vec<String> = items.into_iter().collect();
        items.sort();
        items.dedup();
        let total = items.len() as u64;
        items.truncate(LISTED_MAX);
        Listed { items, total }
    }

    /// Whether there is nothing.
    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    /// Whether `self` has something `old` did not: more items in all, or an item `old` would
    /// have listed (it sorts before `old`'s last one, or `old` was complete) but does not.
    /// Past what a cut `old` lists, a new item that takes the place of one gone is not seen.
    pub fn added_since(&self, old: &Listed) -> bool {
        if self.total > old.total {
            return true;
        }
        let complete = old.items.len() as u64 == old.total;
        let last = old.items.last();
        self.items.iter().any(|i| {
            (complete || last.is_some_and(|l| i <= l)) && old.items.binary_search(i).is_err()
        })
    }
}

/// Ignored files under one top-level entry of a work tree ([`WorktreeInventory`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct IgnoredGroup {
    /// The entry, relative to the work tree's top level; a directory ends with `/`.
    pub path: String,
    /// Files in it that are ignored (the entry itself when it is a file), counted up to
    /// [`IgnoredGroup::more`].
    pub files: u64,
    /// Counting stopped at `files`: there are more.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

/// What deleting a linked worktree by force deletes besides the commits of its repository
/// ([`GitOps::worktree_inventory`]): its directory, when it is there, and its administrative
/// record. Paths are relative to the work tree's top level, each as [`quote_path`] writes it; a
/// repository inside the work tree (a submodule checked out in it, or another repository
/// nested in it, in a tracked, untracked or ignored directory, bare or not) counts with it, its
/// files under its path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeInventory {
    /// Uncommitted files `git status` shows: changed, unmerged and untracked (each file of an
    /// untracked directory), whatever the user's `status.showUntrackedFiles`.
    pub dirty: Listed,
    /// Tracked files whose content on disk differs from the index although `git status` does
    /// not show it: marked assume-unchanged, or skip-worktree and present on disk.
    pub hidden: Listed,
    /// Ignored files and directories, as `git status --ignored=matching` lists them (a
    /// directory an ignore pattern matches ends with `/`, its contents not listed).
    pub ignored: Listed,
    /// The same by top-level entry, with file counts: single files first, then directories,
    /// each by path; at most [`IGNORED_GROUPS_MAX`].
    pub ignored_groups: Vec<IgnoredGroup>,
    /// How many top-level entries hold ignored files in all.
    pub ignored_groups_total: u64,
    /// Repositories that go with the work tree and lose something, or could not be checked, by
    /// path (see [`RepoLoss`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub repositories: Vec<RepoLoss>,
    /// Directories not searched to the end for repositories nested in them (too many entries,
    /// or one that cannot be read), each ending with `/`; the work tree's top level is `./`:
    /// whatever repository they hold goes unchecked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unsearched: Vec<String>,
    /// Linked worktrees inside the work tree whose administrative record is outside what the
    /// deletion removes (see [`LinkedWorktree`]): while there is one, a deletion is refused.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub linked: Vec<LinkedWorktree>,
    /// Mount points inside the work tree (below its top level, as [`quote_path`] writes them):
    /// while there is one, a deletion is refused (moving or deleting through it would take
    /// another file system's files).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    /// Where file systems are mounted could not be read (Linux's `/proc/self/mountinfo`,
    /// macOS's `getfsstat`): a deletion is refused, as with a mount point.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub mounts_unknown: bool,
    /// The work tree's directory is gone: only its record was looked at (no files, only the
    /// repositories the record holds).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub gone: bool,
    /// Bytes on disk of the directory and the record: what the trash item will hold (the
    /// build output in `build_output` not counted).
    #[serde(skip_serializing_if = "is_zero")]
    pub bytes: u64,
    /// The work tree's ignored entries that the deletion deletes instead of moving them to the
    /// trash: regenerable build output and caches (`[threads] park_disposable`) holding no git
    /// repository. Not in `ignored_groups`.
    #[serde(skip_serializing_if = "Listed::is_empty")]
    pub build_output: Listed,
    /// Bytes on disk of `build_output`.
    #[serde(skip_serializing_if = "is_zero")]
    pub build_output_bytes: u64,
}

/// Whether `n` is zero (serde's `skip_serializing_if`).
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// `n` bytes for people: `512 bytes`, `12 KB`, `340 MB`, `1.2 GB` (powers of 1024).
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut v = n as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if v < 10.0 && unit >= 2 {
        format!("{v:.1} {}", UNITS[unit])
    } else {
        format!("{v:.0} {}", UNITS[unit])
    }
}

/// Past this many bytes, a trash item is big: the delete card and the trash list say how to
/// free it sooner (`twr trash empty`).
pub const TRASH_BIG_BYTES: u64 = 1 << 30;

/// Why the work tree `whose` (`api's worktree`, `the worktree <path>`) is not deleted when
/// `mounts` are mounted in it ([`WorktreeInventory::mounts`]), or where file systems are mounted
/// is `unknown`: moving, copying or deleting through a mount point would take another file
/// system's files.
pub fn mount_refusal(whose: &str, mounts: &[String], unknown: bool) -> Option<String> {
    if unknown {
        return Some(format!(
            "where file systems are mounted cannot be read (/proc/self/mountinfo on Linux, \
             getfsstat on macOS), so mount points in {whose} cannot be ruled out"
        ));
    }
    (!mounts.is_empty()).then(|| {
        format!(
            "{whose} has a file system mounted in it at {}: unmount it first",
            mounts.join(", ")
        )
    })
}

impl WorktreeInventory {
    /// Whether `self` has repositories to lose, or directories it could not search for them,
    /// that `old` (what a card said) did not say (see [`RepoLoss::added_since`]; a directory
    /// in one `old` could not search is said with it).
    pub fn repositories_added_since(&self, old: &WorktreeInventory) -> bool {
        RepoLoss::added_since(&self.repositories, &old.repositories)
            || self.unsearched.iter().any(|u| {
                !old.unsearched
                    .iter()
                    .any(|o| o == "./" || (o.ends_with('/') && u.starts_with(o.as_str())))
            })
    }
}

/// A linked worktree checked out inside a work tree a deletion moves to the trash
/// ([`WorktreeInventory::linked`]), whose administrative record (`<common>/worktrees/<name>`,
/// with its `HEAD`, reflogs and index) is outside what the deletion removes (the work tree and
/// its own record): moved away, it would leave that record naming a missing checkout, and
/// `git worktree prune` in its repository (which `git gc` runs) would then clear the record,
/// and `git gc` lose the commits only its `HEAD` or reflogs reach (made on a detached `HEAD`,
/// not pushed). So a deletion is refused while there is one ([`LinkedWorktree::refusal`]):
/// the user moves or removes it with git first. Paths are absolute, each as [`quote_path`]
/// writes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkedWorktree {
    /// Where it is checked out.
    pub path: String,
    /// Its administrative record.
    pub record: String,
    /// Its repository: the main checkout, or the git directory of a bare repository.
    pub repo: String,
    /// It is a worktree of the deleted work tree's own repository (else of another one).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub same_repository: bool,
}

impl LinkedWorktree {
    /// Why a deletion of the work tree `whose` (`x's worktree`) is refused, and what to do.
    pub fn refusal(&self, whose: &str) -> String {
        let of = if self.same_repository {
            "of its own repository"
        } else {
            "of another repository"
        };
        format!(
            "{whose} contains a linked worktree {of} at {path} (its record is {record}): move or \
             remove it first (`git -C {repo} worktree move {path} <elsewhere>`, or `git -C {repo} \
             worktree remove {path}`)",
            path = self.path,
            record = self.record,
            repo = self.repo,
        )
    }
}

/// A repository that goes with a work tree ([`WorktreeInventory::repositories`]): every git
/// directory the deletion removes. A submodule's, under the worktree's administrative record
/// (`modules/<name>`, where git keeps those of a linked worktree; any git directory in the
/// record counts, at any depth) or in the work tree (its `.git` directory); or one nested in
/// the work tree, in any of its directories (tracked, untracked or ignored), whose git
/// directory is in the work tree or the record:
/// a bare repository (a directory that is a git directory itself, as git tells one: a `HEAD`
/// naming a ref or an object, `objects/` and `refs/`), a `.git` directory, or a `.git` file
/// naming one there. Its whole object store goes, so each of its commits that none of its
/// remote-tracking branches reaches is lost. A repository whose git directory is elsewhere (a
/// `.git` file naming one outside) is no `RepoLoss`: its files count with the work tree's, and
/// when that git directory is a linked worktree's record, the deletion is refused
/// ([`LinkedWorktree`]: moving the checkout away would let `git worktree prune` and `git gc`
/// in its repository lose that worktree's commits).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoLoss {
    /// Its work tree's path in the work tree ([`quote_path`]), or a bare repository's; for a
    /// repository in the record that no repository of the work tree has checked out (also every
    /// one when the work tree's directory is gone), its git directory (absolute).
    pub path: String,
    /// Not a submodule: another repository nested in the work tree.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub nested: bool,
    /// Its commits that none of its remote-tracking branches reaches (from its refs, `HEAD`
    /// and reflogs), by id.
    pub commits: Listed,
    /// What of it could not be checked (its commits, or the files of its work tree), and why;
    /// never "nothing lost".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unchecked: Option<String>,
}

impl RepoLoss {
    /// Whether any of `new` has something to lose that `old` (what a card said) did not say:
    /// a repository it did not name, a commit it did not list, or one that cannot be checked
    /// now while it could. What `old` could not check, the user confirmed as such.
    pub fn added_since(new: &[RepoLoss], old: &[RepoLoss]) -> bool {
        new.iter()
            .any(|n| match old.iter().find(|o| o.path == n.path) {
                None => true,
                Some(o) if o.unchecked.is_some() => false,
                Some(o) => n.unchecked.is_some() || n.commits.added_since(&o.commits),
            })
    }
}

/// One thread of a deletion set ([`GitOps::branch_only_commits`]): its repository, and its
/// branch and linked worktree, both deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeleteRefs {
    /// The repository, as the thread names it (any checkout of it: threads whose paths share
    /// a git common directory are one deletion set).
    pub repo: PathBuf,
    /// The thread's branch, whose reflog goes with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The thread's linked worktree: its administrative record (`HEAD`, reflogs and refs of
    /// its own) goes with it, also when the directory is gone already.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// The recorded branches of the box's other threads ([`OtherBranches`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub other_branches: OtherBranches,
}

/// Commits another thread of the same deletion set also reaches ([`OnlyCommits::shared`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharedCommits {
    /// That thread's index in the set.
    pub with: usize,
    /// How many of the lost commits both reach.
    pub commits: u64,
}

/// What deleting a set of threads' branches and worktrees makes unreachable, for one of them
/// ([`GitOps::branch_only_commits`]). What goes with the set is its roots: each branch's tip
/// and every id of its reflog, and each worktree's administrative record (its detached `HEAD`,
/// every id of its reflogs, its own refs). What stays is every other ref of the repository
/// (branches, tags, remote-tracking branches, `refs/stash`, notes, ...), every other reflog,
/// and the `HEAD` of every other worktree. A commit is lost when the set's roots reach it and
/// nothing that stays does.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnlyCommits {
    /// Lost commits the thread's branch (its tip or its reflog) reaches; `None` when there is
    /// no such branch, or not counted ([`OnlyCommits::unchecked`]).
    pub branch: Option<u64>,
    /// Lost commits only the worktree's record reaches (its detached `HEAD`, `HEAD` reflog,
    /// ...), not the thread's branch: made on a detached `HEAD`. `None` when not counted: no
    /// worktree asked about, no record of it in the repository, or
    /// [`OnlyCommits::unchecked`].
    pub head: Option<u64>,
    /// The lost commits it reaches either way, by id.
    pub commits: Listed,
    /// Of those, how many each other thread of the set also reaches (none listed: none).
    pub shared: Vec<SharedCommits>,
    /// The branch its worktree is on when that is not the thread's branch (the thread switched
    /// its worktree to it): the deletion leaves it in the repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout: Option<String>,
    /// The commits of [`OnlyCommits::checkout`] that nothing but it reaches once the set is
    /// gone: no other branch, tag, remote-tracking branch, stash or reflog (its own reflog
    /// left out). Not lost (the branch stays), but on no other branch and not pushed. `None`
    /// when not counted (there is no such branch, or counting failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_only: Option<u64>,
    /// Why its lost commits could not be counted (a reflog, record or object that cannot be
    /// read, ...); the counts above are then `None` and empty, never "nothing lost".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unchecked: Option<String>,
    /// The git common directory of its repository (absolute, symlinks resolved), when it could
    /// be found: deletions in one repository of a box run one after another (`hub::delete`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub common: Option<PathBuf>,
}

/// Git operations of one box. Every call runs on the box that owns the paths.
pub trait GitOps: Send + Sync {
    /// The top level of the work tree that contains `dir`, or `None` when `dir` is not in a
    /// git repository.
    fn repo_root(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<Option<PathBuf>>>;

    /// The git common directory of the repository at `dir` (absolute, symlinks resolved):
    /// what every checkout of one repository shares, and what the hub holds while it removes
    /// something of it (`hub::delete::Reservations`).
    fn git_common_dir(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<PathBuf>>;

    /// Creates a worktree (see [`WorktreeSpec`]). A stale record of a worktree at `path` (its
    /// directory was deleted) is cleared first, so it can be recreated at the same path; other
    /// stale records are left alone (never a repository-wide `git worktree prune`: they may be
    /// the user's worktrees on a disk that is not mounted).
    fn worktree_create(
        &self,
        spec: WorktreeSpec,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeCreated>>;

    /// Removes the worktree at `path` of `repo` unless removing it could lose work (never
    /// forced; the branch is kept): it is kept when dirty ([`RemoveOutcome::KeptDirty`],
    /// untracked files included whatever the user's `status.showUntrackedFiles`), when it has
    /// hidden tracked files, ignored files not known to be regenerable, a git repository in a
    /// tracked directory or commits only its own `HEAD`, reflogs or refs remember
    /// ([`RemoveOutcome::Kept`]), and, with `branch`, when its `HEAD` is
    /// not `refs/heads/<branch>` ([`RemoveOutcome::KeptCheckout`]). A missing directory is
    /// [`RemoveOutcome::Missing`]; only the stale record of that path is cleared.
    fn worktree_remove(
        &self,
        repo: PathBuf,
        path: PathBuf,
        branch: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<RemoveOutcome>>;

    /// Clears the stale records (directory gone) of `repo`'s worktrees that this box manages
    /// (under its worktrees directory); never a repository-wide `git worktree prune`.
    fn worktree_prune(&self, repo: PathBuf) -> BoxFuture<'static, anyhow::Result<()>>;

    /// Whether a directory exists at `path`.
    fn worktree_exists(&self, path: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>>;

    /// Branch, dirty files and ahead/behind of the work tree at `dir` (read without taking
    /// git's optional locks, so it never gets in the way of the agent's own git commands);
    /// with `base`, its merge readiness against that ref too (commits on each side, the files
    /// a merge would conflict in), left out when it can't be read.
    fn snapshot(
        &self,
        dir: PathBuf,
        base: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<GitSnapshot>>;

    /// Moves a thread's worktree and branch to the trash (`thread.delete`, which the user
    /// confirmed): makes the item `<trash>/<item>/`, keeps every commit the branch (its tip
    /// and reflog) and the worktree's record (its `HEAD`, reflogs and own refs) reach under
    /// `refs/twr-trash/<item>/` ([`TRASH_REF_PREFIX`]), moves the worktree's directory and its
    /// administrative record into the item (git forgets the worktree), deletes the branch,
    /// and writes the item's [`TrashManifest`]. A worktree the thread switched to another
    /// branch goes the same way, that branch staying in the repository; one that holds another
    /// thread's checkout ([`OtherBranches`]) is refused before anything is made or moved. A
    /// path that is not a linked worktree of `repo` (its main checkout, another repository, a
    /// plain directory in a work tree) is never touched: an error. A missing worktree or branch is no error. Nothing is deleted: a step
    /// that fails leaves what did not move where it was, and what did in the item.
    /// [`TrashOutcome::item`] is `None` when there was nothing to keep.
    fn worktree_trash(&self, req: TrashRequest)
        -> BoxFuture<'static, anyhow::Result<TrashOutcome>>;

    /// The items of this box's trash, by id.
    fn trash_list(&self) -> BoxFuture<'static, anyhow::Result<Vec<TrashItem>>>;

    /// Purges trash items (see [`TrashPurge`]): deletes each one's refs under
    /// `refs/twr-trash/<item>/` and its directory. Only directories of the trash named by a
    /// valid id whose manifest reads are touched, and never one another operation holds (a
    /// restore under way), or an incomplete one that holds anything its deletion moved (an
    /// incomplete one that holds nothing goes once abandoned: untouched for an hour). Those
    /// are reported as failed.
    fn trash_purge(&self, purge: TrashPurge) -> BoxFuture<'static, anyhow::Result<PurgeOutcome>>;

    /// Makes a thread's worktree and branch again from trash item `req.item`, never moving
    /// or changing the item: creates the branch `req.branch` at the item's branch ref (its
    /// reflog written from the item's copy when the branch has none), or at its `HEAD` ref
    /// when it keeps no branch; lets git add a worktree on it at the first free path of
    /// `req.paths` (or on the other branch the worktree was on when it was deleted, while that
    /// branch still points at the item's `HEAD` ref and is checked out nowhere:
    /// [`RestoreOutcome::checkout`]), marked as this restore's in its record; copies the item's saved files into
    /// it (symlinks as symlinks, nothing followed; tracked files the saved tree lacks
    /// deleted; special files skipped) and the repositories of its submodules into its record.
    /// The index is not restored: staged changes come back as unstaged ones. A step that
    /// fails removes what this restore made, and the item stays as it was.
    fn trash_restore(
        &self,
        req: RestoreRequest,
    ) -> BoxFuture<'static, anyhow::Result<RestoreOutcome>>;

    /// Removes the worktree and branch a restore of trash item `item` made
    /// ([`TrashItem::restored`]: the worktree and its record, the branch only while it points
    /// where the restore put it), when the hub could not record that restore. The item is not
    /// touched. `false` when there was nothing to remove.
    fn trash_restore_undo(&self, item: String) -> BoxFuture<'static, anyhow::Result<bool>>;

    /// What deleting the threads `targets` (each one's branch and linked worktree) would lose,
    /// one result per target in order (see [`OnlyCommits`]). Targets whose repositories share
    /// a git common directory are one deletion set: everything of the set counts as gone, so
    /// a commit only threads of the set reach is lost, and each says how many it shares with
    /// which other thread; a worktree switched to another branch names it, with that branch's
    /// commits nothing else keeps ([`OnlyCommits::checkout`]). What cannot be inspected for a
    /// target is said in its result ([`OnlyCommits::unchecked`]), a worktree holding another
    /// thread's checkout ([`OtherBranches`]) included.
    fn branch_only_commits(
        &self,
        targets: Vec<DeleteRefs>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<OnlyCommits>>>;

    /// What deleting the linked worktree of `repo` at `path` by force
    /// ([`GitOps::worktree_trash`]) takes besides the commits of its repository (see
    /// [`WorktreeInventory`]): of its directory, when it is there, `git status` with every
    /// untracked and ignored file, and tracked files whose changes `git status` hides, of the
    /// work tree and of each repository inside it (submodules, and untracked or ignored nested
    /// repositories); and every repository that goes, in the directory or in the worktree's
    /// administrative record (also when the directory is gone), with its commits on none of
    /// its remote-tracking branches ([`RepoLoss`]).
    fn worktree_inventory(
        &self,
        repo: PathBuf,
        path: PathBuf,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeInventory>>;

    /// The commit `rev` names in the repository whose git directory is `repo` (this and the
    /// reads below name it with `--git-dir`, so git discovers no other): `HEAD`, or a branch
    /// `refs/heads/<name>`; `None` when it names none. Anything else is refused
    /// ([`valid_read_rev`]).
    fn commit_of(
        &self,
        repo: PathBuf,
        rev: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<String>>>;

    /// The entries of directory `dir` (`""`: the top) of commit `commit` (`git ls-tree`, never
    /// the work tree); `None` when `dir` is no directory there.
    fn tree_list(
        &self,
        repo: PathBuf,
        commit: String,
        dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<Vec<TreeEntry>>>>;

    /// The entry at `path` of commit `commit`, its name matched byte for byte; `None` when
    /// there is none.
    fn tree_entry(
        &self,
        repo: PathBuf,
        commit: String,
        path: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<TreeEntry>>>;

    /// How much disk the box has free where its worktrees are, and, with `worktree`, how much
    /// that worktree takes and how much of it is build output ([`DiskReading`]). Read only:
    /// the walk follows no symlink, stops at [`DISK_WALK_MAX`] entries (the reading is then
    /// `partial`), and counts blocks allocated.
    fn disk_usage(
        &self,
        worktree: Option<PathBuf>,
    ) -> BoxFuture<'static, anyhow::Result<DiskReading>>;

    /// Deletes the build output of a thread's worktree (`twr trim`): the ignored entries of
    /// `[threads] park_disposable` holding no git repository, the way a deletion deletes them
    /// (checked again as they go, no symlink followed, never through a mount point), and
    /// nothing else (of it only what [`TrimRequest::only`] names too, when it names any). Only a
    /// linked worktree of `repo`, only where mount points can be told (Linux and macOS), and
    /// refused while another change of the worktree runs; a runtime may stay
    /// alive in it, but none starts until this is over. See [`TrimOutcome`].
    fn worktree_trim(&self, req: TrimRequest) -> BoxFuture<'static, anyhow::Result<TrimOutcome>>;

    /// Whether the repository whose git directory is `repo` is a partial clone (a promisor
    /// remote, a remote with a `partialclonefilter`, or `extensions.partialClone`): reading a missing object would start a fetch,
    /// which writes to its config before any transport runs.
    fn partial_clone(&self, repo: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>>;

    /// Every file under directory `dir` (`""`: the top) of commit `commit`, recursively
    /// (`git ls-tree -r`), each named by its path from the top; subtrees are not listed and
    /// submodules are.
    fn tree_files(
        &self,
        repo: PathBuf,
        commit: String,
        dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Vec<TreeEntry>>>;

    /// The bytes of blob `oid`; an error when it is larger than `max` bytes (nothing read).
    fn blob_read(
        &self,
        repo: PathBuf,
        oid: String,
        max: u64,
    ) -> BoxFuture<'static, anyhow::Result<Vec<u8>>>;

    /// The lines of a commit's text files that contain a fixed string (`git grep` on the
    /// commit's tree, never the work tree); see [`GrepRequest`].
    fn grep_commit(
        &self,
        repo: PathBuf,
        req: GrepRequest,
    ) -> BoxFuture<'static, anyhow::Result<GrepOut>>;
}

/// Most entries the walk of a [`GitOps::disk_usage`] counts before it stops (the reading is
/// then `partial`, a lower bound).
pub const DISK_WALK_MAX: u64 = 2_000_000;

/// A box's disk, and a worktree's share of it ([`GitOps::disk_usage`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DiskReading {
    /// Bytes free for the box's user on the file system of its worktrees (`df`'s available),
    /// when it could be read.
    pub free: Option<u64>,
    /// That file system's size in bytes, when it could be read.
    pub total: Option<u64>,
    /// The worktree asked about, when there was one and it exists.
    pub worktree: Option<WorktreeDisk>,
}

/// What a worktree takes on disk: blocks allocated under it, symlinks not followed, a file
/// with several links counted once.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeDisk {
    /// Bytes in all.
    pub bytes: u64,
    /// Of those, bytes in build output: directories and files named by `[threads]
    /// park_disposable` (`target/`, `node_modules/`, ...), by name alone, whether git ignores
    /// them or not (a trim deletes only ignored ones).
    pub build_bytes: u64,
    /// The walk stopped at [`DISK_WALK_MAX`] entries, or could not read part of the worktree:
    /// the sizes are lower bounds.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
}

/// What [`GitOps::worktree_trim`] trims.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrimRequest {
    /// The thread's repository (its top level).
    pub repo: PathBuf,
    /// The worktree.
    pub worktree: PathBuf,
    /// The thread's recorded branch, and the other threads' ([`OtherBranches`]): a worktree
    /// that proves to be another thread's checkout (moved to the thread's path) is not
    /// trimmed, as a deletion would not take it.
    pub branch: Option<String>,
    /// See `branch`.
    pub other_branches: OtherBranches,
    /// An idle trim's `[threads] idle_trim_disposable` (or its project's): then only the
    /// build output that matches this list too goes (an entry `name/` names directories, `name`
    /// any entry, as in `park_disposable`). `None` (`twr trim`, a disk card): all of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub only: Option<Vec<String>>,
}

/// What a trim did.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrimOutcome {
    /// The entries deleted, as `git status` names them (`target/`).
    pub deleted: Vec<String>,
    /// Bytes they took, measured just before they went (a file with several links counted
    /// once; blocks a link outside them keeps stay allocated).
    pub freed: u64,
    /// What did not go as it should, for people: an entry that could neither be deleted nor
    /// put back, and where it is now.
    pub notes: Vec<String>,
}

/// What a committed tree entry is (its mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    /// `100644`.
    File,
    /// `100755`.
    Executable,
    /// `120000`: the blob holds the link's target.
    Symlink,
    /// `040000`.
    Dir,
    /// `160000`: a commit of another repository.
    Submodule,
}

/// One entry of a committed tree (`git ls-tree --long`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    /// The name (invalid UTF-8 replaced, and then `utf8` is false).
    pub name: String,
    /// Whether the name is valid UTF-8.
    pub utf8: bool,
    /// What it is.
    pub kind: EntryKind,
    /// Size in bytes (blobs only).
    pub size: Option<u64>,
    /// Object id.
    pub oid: String,
}

/// A [`GitOps::grep_commit`] search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepRequest {
    /// The commit ([`valid_commit_id`]).
    pub commit: String,
    /// The fixed string looked for (no newline or NUL).
    pub query: String,
    /// Only under this directory ([`valid_read_path`]); `None`: everywhere.
    pub dir: Option<String>,
    /// Ignore case.
    pub ignore_case: bool,
    /// Most lines per file.
    pub per_file: u32,
    /// Most lines in all; `more` says whether there were others.
    pub max_hits: usize,
    /// Paths (from the top, as `ls-tree` names them, any characters) never searched: git
    /// does not even load them.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// A line [`GitOps::grep_commit`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepHit {
    /// The file, from the repository's top (invalid UTF-8 replaced).
    pub path: String,
    /// Whether the path is valid UTF-8.
    pub utf8: bool,
    /// 1-based line number.
    pub line: u64,
    /// The line (invalid UTF-8 replaced, cut to [`GREP_LINE_MAX`] bytes).
    pub text: String,
}

/// What [`GitOps::grep_commit`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GrepOut {
    /// The lines, in git's order.
    pub hits: Vec<GrepHit>,
    /// More lines matched than `max_hits`.
    pub more: bool,
}

/// Longest line [`GitOps::grep_commit`] returns, in bytes.
pub const GREP_LINE_MAX: usize = 512;

/// Longest path [`valid_read_path`] accepts, in bytes.
pub const READ_PATH_MAX: usize = 4096;

/// Whether `s` is a full commit id (40 or 64 lowercase hex digits).
pub fn valid_commit_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64)
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why `rev` is not one [`GitOps::commit_of`] reads: `HEAD`, or `refs/heads/<name>` whose
/// parts are valid branch components ([`valid_branch_component`]).
pub fn valid_read_rev(rev: &str) -> Result<(), String> {
    if rev == "HEAD" {
        return Ok(());
    }
    match rev.strip_prefix("refs/heads/") {
        Some(name) if !name.is_empty() && name.split('/').all(valid_branch_component) => Ok(()),
        _ => Err(format!("{rev:?} is neither HEAD nor a branch")),
    }
}

/// `path` checked for reading from a commit: relative to the repository's top, `/`
/// separated, without an empty, `.` or `..` part, a control character (NUL included) or a
/// leading `-`, at most [`READ_PATH_MAX`] bytes; a trailing `/` is dropped. `""` is the
/// top.
pub fn valid_read_path(path: &str) -> Result<String, String> {
    if path.len() > READ_PATH_MAX {
        return Err(format!("a path is at most {READ_PATH_MAX} bytes"));
    }
    if path.chars().any(char::is_control) {
        return Err("a path has no control characters".into());
    }
    let path = path.strip_suffix('/').unwrap_or(path);
    if path.is_empty() {
        return Ok(String::new());
    }
    if path.starts_with('/') {
        return Err(format!(
            "{path:?} is absolute: give a path from the repository's top"
        ));
    }
    if path.starts_with('-') {
        return Err(format!("{path:?} starts with -"));
    }
    if path
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(format!("{path:?} has an empty, . or .. part"));
    }
    Ok(path.to_string())
}

/// `git ls-tree -z --long` output, parsed: `<mode> <type> <oid> <size>\t<name>` records ended
/// by NUL. A record of a mode twr does not know is skipped.
pub fn parse_ls_tree(out: &[u8]) -> Result<Vec<TreeEntry>, String> {
    let mut entries = Vec::new();
    for rec in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = rec
            .iter()
            .position(|b| *b == b'\t')
            .ok_or_else(|| "ls-tree record without a tab".to_string())?;
        let (meta, name) = (&rec[..tab], &rec[tab + 1..]);
        let meta = std::str::from_utf8(meta).map_err(|_| "ls-tree record not text".to_string())?;
        let mut f = meta.split_whitespace();
        let (Some(mode), Some(_kind), Some(oid), Some(size)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            return Err(format!("short ls-tree record {meta:?}"));
        };
        let kind = match mode {
            "100644" | "100664" => EntryKind::File,
            "100755" => EntryKind::Executable,
            "120000" => EntryKind::Symlink,
            "040000" => EntryKind::Dir,
            "160000" => EntryKind::Submodule,
            _ => continue,
        };
        let (name_s, utf8) = match std::str::from_utf8(name) {
            Ok(s) => (s.to_string(), true),
            Err(_) => (String::from_utf8_lossy(name).into_owned(), false),
        };
        entries.push(TreeEntry {
            name: name_s,
            utf8,
            kind,
            size: size.parse().ok(),
            oid: oid.to_string(),
        });
    }
    Ok(entries)
}

/// `git grep -z -n <query> <commit>` output, parsed: `<commit>:<path>\0<line>\0<text>\n`
/// records. At most `max_hits` hits; `more` when there were others.
pub fn parse_grep(out: &[u8], commit: &str, max_hits: usize) -> GrepOut {
    let prefix = format!("{commit}:");
    let mut res = GrepOut::default();
    let mut rest = out;
    while !rest.is_empty() {
        // Path, NUL, line number, NUL, text up to the newline.
        let Some(a) = rest.iter().position(|b| *b == 0) else {
            break;
        };
        let path = &rest[..a];
        let after = &rest[a + 1..];
        let Some(b) = after.iter().position(|b| *b == 0) else {
            break;
        };
        let line = std::str::from_utf8(&after[..b])
            .ok()
            .and_then(|s| s.parse().ok());
        let after = &after[b + 1..];
        let end = after
            .iter()
            .position(|b| *b == b'\n')
            .unwrap_or(after.len());
        let text = &after[..end];
        rest = after.get(end + 1..).unwrap_or_default();
        let (Some(line), Some(path)) = (line, path.strip_prefix(prefix.as_bytes())) else {
            continue;
        };
        if res.hits.len() == max_hits {
            res.more = true;
            break;
        }
        let mut t = String::from_utf8_lossy(text).into_owned();
        if t.len() > GREP_LINE_MAX {
            let mut cut = GREP_LINE_MAX;
            while !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t.truncate(cut);
        }
        res.hits.push(GrepHit {
            path: String::from_utf8_lossy(path).into_owned(),
            utf8: std::str::from_utf8(path).is_ok(),
            line,
            text: t,
        });
    }
    res
}

/// `git status --porcelain=v2 --branch`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PorcelainStatus {
    /// `branch.oid`: the `HEAD` commit, `None` before the first commit (`(initial)`).
    pub oid: Option<String>,
    /// `branch.head`: the branch, `None` when `HEAD` is detached.
    pub branch: Option<String>,
    /// `branch.upstream`, when set.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream (`branch.ab`; 0 without an upstream).
    pub ahead: u64,
    /// Commits behind the upstream.
    pub behind: u64,
    /// Changed tracked entries (ordinary `1` and renamed/copied `2` records).
    pub changed: u64,
    /// Unmerged (conflicted) entries (`u` records).
    pub unmerged: u64,
    /// Untracked entries (`?` records; an untracked directory counts once unless untracked
    /// files are listed individually).
    pub untracked: u64,
    /// Ignored entries (`!` records, only with `--ignored`); never dirty.
    pub ignored: u64,
    /// Paths of the ignored entries (a directory ends with `/`), relative to the work tree's
    /// top level (see [`parse_porcelain_v2`] for how they are written).
    pub ignored_paths: Vec<String>,
    /// Paths of the dirty entries (changed, unmerged and untracked; a rename's new path; an
    /// untracked directory ends with `/`), relative to the work tree's top level (likewise).
    pub dirty_paths: Vec<String>,
}

impl PorcelainStatus {
    /// Files that make the work tree dirty: changed, unmerged and untracked.
    pub fn dirty_files(&self) -> u64 {
        self.changed + self.unmerged + self.untracked
    }

    /// The `git.snapshot` payload.
    pub fn snapshot(&self) -> GitSnapshot {
        GitSnapshot {
            branch: self.branch.clone(),
            dirty_files: self.dirty_files(),
            ahead: self.ahead,
            behind: self.behind,
            ..Default::default()
        }
    }
}

/// Whether two snapshots differ in what `git.snapshot` reports (branch, dirty files,
/// ahead/behind); extras are ignored.
pub fn snapshot_changed(old: Option<&GitSnapshot>, new: &GitSnapshot) -> bool {
    match old {
        None => true,
        Some(o) => {
            o.branch != new.branch
                || o.dirty_files != new.dirty_files
                || o.ahead != new.ahead
                || o.behind != new.behind
                || o.base != new.base
                || o.base_ahead != new.base_ahead
                || o.base_behind != new.base_behind
                || o.conflicts != new.conflicts
                || o.conflict_count != new.conflict_count
                || o.conflicted != new.conflicted
        }
    }
}

/// A malformed `git status --porcelain=v2` output.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("malformed git status line {line:?}: {why}")]
pub struct PorcelainError {
    /// The offending record.
    pub line: String,
    /// What is wrong with it.
    pub why: &'static str,
}

/// Parses `git status --porcelain=v2 --branch` output, with `-z` (NUL-terminated records; a
/// rename's original path is the record after it) or without (one record per line; the
/// original path follows a tab). The format is detected from the presence of a NUL. With
/// `-z` paths are raw bytes, kept as [`quote_path`] writes them (two names never read the
/// same); without, as git printed them (quoted by git when `core.quotePath` says so).
///
/// Unknown header lines (`# stash ...`) and unknown record types are ignored, so newer git
/// versions still parse; a known record with missing fields is an error.
pub fn parse_porcelain_v2(out: impl AsRef<[u8]>) -> Result<PorcelainStatus, PorcelainError> {
    let out = out.as_ref();
    let nul = out.contains(&0);
    let mut records: Box<dyn Iterator<Item = &[u8]>> = if nul {
        Box::new(out.split(|b| *b == 0))
    } else {
        Box::new(
            out.split(|b| *b == b'\n')
                .map(|l| l.strip_suffix(b"\r").unwrap_or(l)),
        )
    };
    let path = |raw: &[u8]| {
        if nul {
            quote_path(raw)
        } else {
            String::from_utf8_lossy(raw).into_owned()
        }
    };
    let mut st = PorcelainStatus::default();
    let err = |line: &[u8], why| PorcelainError {
        line: String::from_utf8_lossy(line).into_owned(),
        why,
    };
    while let Some(rec) = records.next() {
        if rec.is_empty() {
            continue;
        }
        if let Some(header) = rec.strip_prefix(b"# ") {
            let header = String::from_utf8_lossy(header);
            let (key, value) = header.split_once(' ').unwrap_or((&header, ""));
            match key {
                "branch.oid" => {
                    st.oid = (value != "(initial)" && !value.is_empty()).then(|| value.into());
                }
                "branch.head" => {
                    if value.is_empty() {
                        return Err(err(rec, "no branch name"));
                    }
                    st.branch = (value != "(detached)").then(|| value.into());
                }
                "branch.upstream" => st.upstream = Some(value.into()),
                "branch.ab" => {
                    let mut parts = value.split(' ');
                    let ahead = parts
                        .next()
                        .and_then(|a| a.strip_prefix('+'))
                        .and_then(|a| a.parse().ok());
                    let behind = parts
                        .next()
                        .and_then(|b| b.strip_prefix('-'))
                        .and_then(|b| b.parse().ok());
                    match (ahead, behind) {
                        (Some(a), Some(b)) => {
                            st.ahead = a;
                            st.behind = b;
                        }
                        _ => return Err(err(rec, "expected +<ahead> -<behind>")),
                    }
                }
                _ => {}
            }
            continue;
        }
        // An unknown record may start with anything (a multi-byte character).
        let (kind, rest) = (rec[0], &rec[1..]);
        match kind {
            // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
            b'1' => {
                if !has_fields(rec, 9) {
                    return Err(err(rec, "too few fields"));
                }
                st.changed += 1;
                st.dirty_paths.push(path(last_field(rec, 9)));
            }
            // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>[\t<origPath>]
            b'2' => {
                if !has_fields(rec, 10) {
                    return Err(err(rec, "too few fields"));
                }
                let mut new = last_field(rec, 10);
                if nul {
                    // The original path is the next record.
                    if records.next().is_none_or(<[u8]>::is_empty) {
                        return Err(err(rec, "rename without its original path"));
                    }
                } else {
                    match new.iter().position(|b| *b == b'\t') {
                        Some(tab) => new = &new[..tab],
                        None => return Err(err(rec, "rename without its original path")),
                    }
                }
                st.changed += 1;
                st.dirty_paths.push(path(new));
            }
            // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
            b'u' => {
                if !has_fields(rec, 11) {
                    return Err(err(rec, "too few fields"));
                }
                st.unmerged += 1;
                st.dirty_paths.push(path(last_field(rec, 11)));
            }
            b'?' | b'!' => {
                if rest.iter().all(|b| *b == b' ') {
                    return Err(err(rec, "no path"));
                }
                let p = path(rest.strip_prefix(b" ").unwrap_or(rest));
                if kind == b'?' {
                    st.untracked += 1;
                    st.dirty_paths.push(p);
                } else {
                    st.ignored += 1;
                    st.ignored_paths.push(p);
                }
            }
            _ => {}
        }
    }
    Ok(st)
}

/// The last of `n` space-separated fields of `rec` (a path, taking the rest; see
/// [`has_fields`]).
fn last_field(rec: &[u8], n: usize) -> &[u8] {
    rec.splitn(n, |b| *b == b' ').nth(n - 1).unwrap_or_default()
}

/// Whether `rec` has `n` space-separated fields, the last one (a path) taking the rest.
fn has_fields(rec: &[u8], n: usize) -> bool {
    let parts: Vec<&[u8]> = rec.splitn(n, |b| *b == b' ').collect();
    parts.len() == n && parts.iter().all(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: impl AsRef<[u8]>) -> PorcelainStatus {
        parse_porcelain_v2(s).unwrap()
    }

    #[test]
    fn clean_branch_without_upstream() {
        let st = parse("# branch.oid 1b2c3d\n# branch.head main\n");
        assert_eq!(st.oid.as_deref(), Some("1b2c3d"));
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream, None);
        assert_eq!((st.ahead, st.behind), (0, 0));
        assert_eq!(st.dirty_files(), 0);
        assert_eq!(
            st.snapshot(),
            GitSnapshot {
                branch: Some("main".into()),
                ..Default::default()
            }
        );
    }

    #[test]
    fn ahead_and_behind_an_upstream() {
        let st = parse(
            "# branch.oid abc\n# branch.head twr/api\n# branch.upstream origin/twr/api\n\
             # branch.ab +3 -12\n",
        );
        assert_eq!(st.upstream.as_deref(), Some("origin/twr/api"));
        assert_eq!((st.ahead, st.behind), (3, 12));
        let s = st.snapshot();
        assert_eq!((s.ahead, s.behind), (3, 12));
    }

    #[test]
    fn detached_head_and_initial_commit() {
        let st = parse("# branch.oid abc\n# branch.head (detached)\n");
        assert_eq!(st.branch, None);
        assert_eq!(st.oid.as_deref(), Some("abc"));
        let st = parse("# branch.oid (initial)\n# branch.head main\n? a.txt\n");
        assert_eq!(st.oid, None);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.untracked, 1);
    }

    #[test]
    fn counts_every_kind_of_dirty_entry() {
        let text = "# branch.oid abc\n\
                    # branch.head main\n\
                    # stash 2\n\
                    1 .M N... 100644 100644 100644 aaa aaa src/lib.rs\n\
                    1 A. N... 000000 100644 100644 000 bbb new file.rs\n\
                    2 R. N... 100644 100644 100644 ccc ccc R100 renamed.rs\told.rs\n\
                    u UU N... 100644 100644 100644 100644 d1 d2 d3 conflict.rs\n\
                    ? untracked.txt\n\
                    ? build/\n\
                    ! target/\n";
        let st = parse(text);
        assert_eq!(st.changed, 3);
        assert_eq!(st.unmerged, 1);
        assert_eq!(st.untracked, 2);
        assert_eq!(st.ignored, 1);
        assert_eq!(st.ignored_paths, vec!["target/".to_string()]);
        assert_eq!(st.dirty_files(), 6);
        assert_eq!(
            st.dirty_paths,
            [
                "src/lib.rs",
                "new file.rs",
                "renamed.rs",
                "conflict.rs",
                "untracked.txt",
                "build/"
            ]
        );
    }

    #[test]
    fn nul_terminated_records_with_renames_and_odd_paths() {
        let text = "# branch.oid abc\0# branch.head feature\0\
                    1 .M N... 100644 100644 100644 aaa aaa has\nnewline.txt\0\
                    2 R. N... 100644 100644 100644 ccc ccc R090 to name.rs\0from name.rs\0\
                    ? spaced name\0!  .env with space\0";
        let st = parse(text);
        assert_eq!(st.ignored_paths, vec![" .env with space".to_string()]);
        assert_eq!(st.branch.as_deref(), Some("feature"));
        // The rename's original path is not a record of its own.
        assert_eq!(st.changed, 2);
        assert_eq!(st.untracked, 1);
        assert_eq!(st.dirty_files(), 3);
        assert_eq!(
            st.dirty_paths,
            ["has\\nnewline.txt", "to name.rs", "spaced name"]
        );
    }

    /// Raw names, not necessarily UTF-8, each read as one string no other name reads as.
    #[test]
    fn nul_terminated_paths_name_one_file_each() {
        let st = parse(
            b"? notes-\x80\0? notes-\x81\0? notes-\\x80\0? caf\xc3\xa9\0! tab\there\0".as_slice(),
        );
        assert_eq!(
            st.dirty_paths,
            ["notes-\\x80", "notes-\\x81", "notes-\\\\x80", "café"]
        );
        assert_eq!(st.ignored_paths, ["tab\\there"]);
        for raw in [
            b"notes-\x80".as_slice(),
            b"notes-\\x80",
            b"a\\\\b\x01\x7f\n\r\t\xff\xc3",
            "\u{9b}é\\u{9b}".as_bytes(),
            b"\\",
            b"x\\",
        ] {
            assert_eq!(unquote_path(&quote_path(raw)), raw, "{raw:?}");
        }
        assert_eq!(quote_path("\u{9b}".as_bytes()), "\\u{9b}");
        // Escapes it never writes are kept as they are.
        for odd in ["\\q", "\\x+f", "\\xz", "\\u{}", "\\u{zz}", "end\\"] {
            assert_eq!(unquote_path(odd), odd.as_bytes(), "{odd:?}");
        }
    }

    /// A [`Listed`] keeps the first items in order and tells what is new, as far as it can.
    #[test]
    fn listed_keeps_the_first_items_and_sees_new_ones() {
        let l = |items: &[&str]| Listed::new(items.iter().map(|s| s.to_string()));
        let old = l(&["b", "a", "b"]);
        assert_eq!(
            (old.items.clone(), old.total),
            (vec!["a".into(), "b".into()], 2)
        );
        assert!(!l(&["a"]).added_since(&old), "fewer");
        assert!(!l(&["b", "a"]).added_since(&old), "the same");
        assert!(l(&["a", "c"]).added_since(&old), "one replaced");
        assert!(l(&["a", "b", "c"]).added_since(&old), "one more");
        let many: Vec<String> = (0..LISTED_MAX + 10).map(|i| format!("{i:05}")).collect();
        let cut = Listed::new(many.clone());
        assert_eq!(
            (cut.items.len(), cut.total),
            (LISTED_MAX, many.len() as u64)
        );
        assert!(!Listed::new(many.clone()).added_since(&cut));
        // One replaced where the cut list reaches: seen; past it: only a larger total is.
        let mut early = many.clone();
        early[0] = "00000a".into();
        assert!(Listed::new(early).added_since(&cut));
        let mut late = many.clone();
        late[LISTED_MAX + 5] = "zzz".into();
        assert!(!Listed::new(late.clone()).added_since(&cut));
        late.push("zzzz".into());
        assert!(Listed::new(late).added_since(&cut));
    }

    /// A repository has more to lose when a card did not name it, did not list a commit of
    /// it, or could check it and cannot now; what the card could not check is not compared. A
    /// directory not searched for repositories that the card did not name is more too.
    #[test]
    fn repository_losses_see_what_is_new() {
        let repo = |path: &str, commits: &[&str], unchecked: bool| RepoLoss {
            path: path.into(),
            commits: Listed::new(commits.iter().map(|c| c.to_string())),
            unchecked: unchecked.then(|| "boom".into()),
            ..Default::default()
        };
        let old = [repo("a", &["c1"], false), repo("b", &[], true)];
        assert!(!RepoLoss::added_since(&old, &old));
        assert!(!RepoLoss::added_since(&[], &old));
        assert!(!RepoLoss::added_since(&[repo("b", &["c9"], false)], &old));
        for new in [
            repo("a", &["c1", "c2"], false),
            repo("a", &["c1"], true),
            repo("c", &["c3"], false),
        ] {
            assert!(
                RepoLoss::added_since(std::slice::from_ref(&new), &old),
                "{new:?}"
            );
        }
        let inv = |unsearched: &[&str]| WorktreeInventory {
            unsearched: unsearched.iter().map(|u| u.to_string()).collect(),
            ..Default::default()
        };
        assert!(!inv(&["t/"]).repositories_added_since(&inv(&["t/", "u/"])));
        assert!(inv(&["t/", "v/"]).repositories_added_since(&inv(&["t/"])));
        // A directory in one the card could not search, or anything when it could not search
        // the top level, is said with it.
        assert!(!inv(&["t/a/", "t/"]).repositories_added_since(&inv(&["t/"])));
        assert!(!inv(&["t/", "v/"]).repositories_added_since(&inv(&["./"])));
        assert!(inv(&["tt/"]).repositories_added_since(&inv(&["t/"])));
        assert!(inv(&["./"]).repositories_added_since(&inv(&["t/"])));
    }

    #[test]
    fn a_path_that_looks_like_a_record_is_just_a_path() {
        // With -z the original path of a rename may be anything, e.g. "? x".
        let text = "# branch.head main\0\
                    2 R. N... 100644 100644 100644 c c R100 new\0? x\0";
        let st = parse(text);
        assert_eq!((st.changed, st.untracked), (1, 0));
    }

    #[test]
    fn empty_output_and_unknown_records() {
        assert_eq!(parse(""), PorcelainStatus::default());
        // Unknown headers and record kinds from a newer git are skipped.
        let st = parse("# branch.head main\n# branch.future x y\nZ something\né x\n");
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.dirty_files(), 0);
    }

    #[test]
    fn malformed_records_are_errors() {
        for bad in [
            "# branch.ab 3 12",
            "# branch.ab +x -1",
            "# branch.ab +1",
            "# branch.head",
            "1 .M N... 100644",
            "2 R. N... 100644 100644 100644 c c R100 new",
            "u UU N... 1 2 3",
            "?",
            "? ",
        ] {
            assert!(parse_porcelain_v2(bad).is_err(), "{bad:?} parsed");
        }
        // A NUL-terminated rename cut short.
        assert!(parse_porcelain_v2("2 R. N... 1 1 1 c c R100 new\0").is_err());
        let e = parse_porcelain_v2("# branch.ab nope").unwrap_err();
        assert!(e.to_string().contains("branch.ab"), "{e}");
    }

    #[test]
    fn snapshot_changes_ignore_extras() {
        let a = GitSnapshot {
            branch: Some("twr/x".into()),
            dirty_files: 1,
            ..Default::default()
        };
        assert!(snapshot_changed(None, &a));
        assert!(!snapshot_changed(Some(&a), &a.clone()));
        let mut ext = crate::event::JsonMap::new();
        ext.insert("k".into(), serde_json::json!(1));
        let with_ext = GitSnapshot {
            ext: Some(ext),
            ..a.clone()
        };
        assert!(!snapshot_changed(Some(&a), &with_ext));
        for changed in [
            GitSnapshot {
                branch: None,
                ..a.clone()
            },
            GitSnapshot {
                dirty_files: 2,
                ..a.clone()
            },
            GitSnapshot {
                ahead: 1,
                ..a.clone()
            },
            GitSnapshot {
                behind: 1,
                ..a.clone()
            },
        ] {
            assert!(snapshot_changed(Some(&a), &changed), "{changed:?}");
        }
    }

    #[test]
    fn read_paths_revs_and_commits_are_strict() {
        assert_eq!(valid_read_path("").unwrap(), "");
        assert_eq!(valid_read_path("docs/").unwrap(), "docs");
        assert_eq!(valid_read_path("src/main.rs").unwrap(), "src/main.rs");
        assert_eq!(valid_read_path("a/-b").unwrap(), "a/-b");
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../b",
            "a//b",
            "./a",
            "-n",
            "a\0b",
            "a\nb",
            "a/.",
        ] {
            assert!(valid_read_path(bad).is_err(), "{bad:?}");
        }
        assert!(valid_read_path(&"a".repeat(READ_PATH_MAX + 1)).is_err());
        assert!(valid_read_rev("HEAD").is_ok());
        assert!(valid_read_rev("refs/heads/twr/api").is_ok());
        for bad in [
            "HEAD~1",
            "main..HEAD",
            "--output=x",
            "refs/heads/",
            "refs/tags/v1",
            "@{-1}",
            "refs/heads/a..b",
            "refs/heads/-x",
        ] {
            assert!(valid_read_rev(bad).is_err(), "{bad:?}");
        }
        assert!(valid_commit_id(&"a".repeat(40)));
        assert!(valid_commit_id(&"0".repeat(64)));
        assert!(!valid_commit_id(&"A".repeat(40)));
        assert!(!valid_commit_id("HEAD"));
    }

    #[test]
    fn ls_tree_and_grep_output_parse() {
        let out = [
            &b"100644 blob aaaa       12\tREADME.md"[..],
            b"040000 tree bbbb        -\tsrc",
            b"120000 blob cccc       11\tlink",
            b"160000 commit dddd       -\tvendor",
            b"100755 blob eeee 5\tx\xff",
        ]
        .join(&0u8);
        let out = &out[..];
        let e = parse_ls_tree(out).unwrap();
        assert_eq!(e.len(), 5);
        assert_eq!(
            (e[0].name.as_str(), e[0].kind, e[0].size),
            ("README.md", EntryKind::File, Some(12))
        );
        assert_eq!((e[1].kind, e[1].size), (EntryKind::Dir, None));
        assert_eq!(e[2].kind, EntryKind::Symlink);
        assert_eq!(e[3].kind, EntryKind::Submodule);
        assert!(!e[4].utf8 && e[4].kind == EntryKind::Executable);
        let c = "f".repeat(40);
        let out = format!("{c}:src/a.rs\x003\x00fn main() {{}}\n{c}:b.txt\x0010\x00x\n");
        let g = parse_grep(out.as_bytes(), &c, 10);
        assert_eq!(g.hits.len(), 2);
        assert_eq!((g.hits[0].path.as_str(), g.hits[0].line), ("src/a.rs", 3));
        assert_eq!(g.hits[0].text, "fn main() {}");
        let g = parse_grep(out.as_bytes(), &c, 1);
        assert!(g.more && g.hits.len() == 1);
        let long = format!("{c}:a\x001\x00{}\n", "é".repeat(400));
        assert!(parse_grep(long.as_bytes(), &c, 5).hits[0].text.len() <= GREP_LINE_MAX);
    }

    #[test]
    fn bytes_read_for_people() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(1023), "1023 bytes");
        assert_eq!(human_bytes(12 * 1024), "12 KB");
        assert_eq!(human_bytes(340 << 20), "340 MB");
        assert_eq!(human_bytes((1 << 30) + (1 << 28)), "1.2 GB");
        assert_eq!(human_bytes(20 << 30), "20 GB");
    }

    #[test]
    fn trash_ids_are_strict() {
        assert!(valid_trash_id("01J00000000000000000000000"));
        for bad in [
            "",
            "01j00000000000000000000000",
            "../00000000000000000000000",
            "01J0000000000000000000000",
            "01J00000000000000000000000/",
        ] {
            assert!(!valid_trash_id(bad), "{bad}");
        }
    }

    #[test]
    fn branch_components() {
        for ok in ["api-1", "a.b_c", "x", "A9", "feature.v2", "a@b"] {
            assert!(valid_branch_component(ok), "{ok:?} refused");
        }
        for bad in [
            "",
            ".hidden",
            "-x",
            "trailing.",
            "x.lock",
            "a..b",
            "a@{1}",
            "@",
            "a/b",
            "sp ace",
            "t~1",
            "c^",
            "q?",
            "st*r",
            "br[",
            "back\\",
            "co:lon",
            "tab\t",
        ] {
            assert!(!valid_branch_component(bad), "{bad:?} accepted");
        }
        assert_eq!(thread_branch("api-1"), "twr/api-1");
    }
}

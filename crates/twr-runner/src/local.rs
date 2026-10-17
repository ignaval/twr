//! The local box's git: [`LocalGit`] implements [`GitOps`] with the `git` CLI (no libgit2),
//! for the hub's own box and for runners on remote boxes.
//!
//! Every command runs as `git -c <SAFE_CONFIG>... -C <dir> ...` with the caller's `GIT_DIR` /
//! `GIT_WORK_TREE` / `GIT_INDEX_FILE` removed (a hub started from a git hook must not act on
//! that repository), no terminal prompts, stdin closed, in a session and process group of its
//! own (no controlling terminal), with its output capped ([`MAX_STDOUT`]). [`SAFE_CONFIG`]
//! overrides what an agent could plant in the repository's config to make twr run code
//! (fsmonitor, hooks, network transports) or hide files from it
//! (`status.showUntrackedFiles=no`). Every command sees the literal object graph: replacement
//! objects (`git replace`) and legacy grafts (`info/grafts`) are ignored, so nothing is judged
//! reachable through a graft that can be removed.
//!
//! No command uses a git transport (`ext::` included, whatever `protocol.<name>.allow` the
//! repository sets) or lazily fetches a partial clone's missing objects, so git itself runs no ssh,
//! credential helper or remote helper; only a driver of the user's own config may reach the network
//! itself (Git LFS does, below). Before every command that could run a program the repository's
//! config names (a filter, a merge or diff driver, `diff.external`, a hook defined in config), twr
//! lists that config and turns each one off with a `-c` ([`LocalGit::neutralized`]). Drivers your
//! global or system config sets are yours and run, on whatever files the repository's attributes
//! give them. Git LFS's filter is one: a checkout twr makes fetches LFS files as `git checkout`
//! would, from the LFS server the repository names (its `.lfsconfig` included), and LFS
//! repositories keep working. That server may be an `ssh://` URL, which Git LFS reaches with the
//! user's `ssh` (their `~/.ssh/config` and agent keys); over HTTPS, a 401 makes it run
//! `git credential fill`, the user's credential helpers, for that host. So a repository can make a
//! checkout authenticate as the user to a host it names. But Git LFS runs programs its config
//! names: custom transfer agents, extensions, its askpass, ssh and credential helpers. Where the
//! repository's config names one of them (never its `.lfsconfig`, from which Git LFS takes no such
//! key), Git LFS's filter is turned off like a driver the repository names, logged once: twr's
//! checkouts of that repository hold LFS files as their pointers.
//!
//! The listing and the command are two git processes. Whatever can write the config the
//! command reads in between (the repository's `config`, a worktree's `config.worktree` where
//! the repository sets `extensions.worktreeConfig`, or a file either includes) can name a
//! driver the listing did not see, for a file the repository's attributes give it, and the
//! command runs it. An agent that may run what it likes gains nothing from that; a sandboxed
//! one that can write its worktree's git directory (Codex's `auto` mode) does, in a repository
//! that reads `config.worktree`. Git can't turn off every driver whatever its name, and taking
//! the attributes from elsewhere (`--attr-source`) leaves `info/attributes` and
//! `core.attributesFile` in force, while dropping the line-ending and encoding attributes a
//! checkout needs. So it is told instead: after each such command (the merge readiness probe
//! too) twr lists the config again where git ran, and a driver named now that had no override
//! (`filter.lfs` for a program named for Git LFS while its filter was on) is logged, naming the
//! directory and the driver; so is a repository git finds there that is
//! not the one listed (a `.git` written into that directory meanwhile). A read (a snapshot's
//! status, the readiness probe) then fails, whatever git returned, so nothing it found is used
//! or kept; a change (a worktree's checkout, a ref update, a removal) keeps its own result, as
//! its caller records what it made and failing it would undo nothing. The task that reaps git
//! makes that check, so it is made and logged also when the caller gave up waiting. The next
//! command turns the driver off like any other. What remains: the one command that raced ran
//! the driver, and a writer that names it and takes it back between the two listings is not
//! seen. `docs/security-model.md` states these residuals for users.
//!
//! Timeouts: `git status` 30 s, other reads 60 s, commands that change the repository or a
//! work tree (`worktree add` / `remove`, `update-ref`, `branch -D`, `reset --hard`,
//! `pack-objects`) 5 minutes. On timeout, or when its caller gives up (within an operation
//! that changes the box, see below, only when the process itself is going away), the command's
//! whole process group is killed, and the call returns only once every process of it has
//! exited and git is reaped: no git twr starts outlives the call (and the reservations held
//! across it) that started it. A killed change may have done part of its work: the caller gets a
//! timeout error that says so, and leaves what it was changing as it is rather than clean it
//! up on a guess. Git's automatic `gc` / maintenance, which would detach from the process
//! group, is off.
//!
//! A worktree change reserves its path ([`crate::workdirs`]) from before its checks until git
//! has exited: no runtime of the box starts in it meanwhile, a removal is refused while one
//! runs in it (kept, with the reason), and [`GitOps::worktree_exists`] waits for a change in
//! progress to finish first.
//!
//! Every operation that changes the box's repositories or worktrees (worktree create, remove
//! and stale-record clearing; the move to the trash, a restore and its undo, a purge with the
//! refs it drops) runs whole as one task of its own ([`LocalGit::supervised`]):
//! the task owns every lock and reservation it takes (a trash item's lock, its paths'
//! reservations), every git it runs and every blocking job it starts (awaited by it, never
//! left running on its own). Its caller only waits for the task; a caller that gives up or
//! goes away (a hub request dropped, the server's connections aborted at shutdown) never stops
//! it, so nothing it holds goes before all of its work is over, and nothing it began is left
//! half done because its caller left. A panic in one of its blocking jobs reaches it only once
//! that job ended. The hub's and a runner's shutdown wait for those still running
//! ([`LocalGit::changes_done`], at most [`twr_core::git::GIT_CHANGE_TIMEOUT`]).
//!
//! Snapshots use `--no-optional-locks`, so a background `git status` never holds `index.lock`
//! while the agent runs its own git commands.
//!
//! Worktrees are never removed by force, and never when that could lose work
//! ([`GitOps::worktree_remove`]):
//!
//! - dirty (changes, untracked files, whatever `status.showUntrackedFiles` says):
//!   [`RemoveOutcome::KeptDirty`];
//! - a checkout that is not the thread's branch (a detached `HEAD`, or another branch):
//!   [`RemoveOutcome::KeptCheckout`] (commits made on a detached `HEAD` are remembered only by
//!   the worktree's own reflog, which removal deletes);
//! - tracked files hidden from `git status` (assume-unchanged, or skip-worktree and present on
//!   disk), ignored files outside the regenerable directories ([`DEFAULT_PARK_DISPOSABLE`],
//!   `target/`, `node_modules/`, ...), a git repository in a tracked directory (`git status`
//!   does not show it, and `git worktree remove` deletes it), or commits that only the
//!   worktree's own record reaches (its detached tip, its `HEAD` reflog and other reflogs, its
//!   own refs: what a deletion counts as going with it) and no branch, tag or remote-tracking
//!   branch does: [`RemoveOutcome::Kept`].
//!
//! Branches are never deleted. Stale worktree records (directory known to be gone: a lookup
//! error other than "not found" keeps the record and is reported) are cleared one at a time,
//! only for the path twr manages (and never when their `HEAD`, reflogs or own refs hold
//! commits nothing else reaches, or cannot be read); never with a repository-wide `git
//! worktree prune`, which would also drop the records of the user's own worktrees on a disk
//! that is not mounted.
//!
//! The exception is [`GitOps::worktree_trash`], after the user confirmed deleting the thread
//! (the `trash` module): its linked worktree (dirty, locked, whatever it holds) and its record
//! move to the box's trash, every commit they and its branch reach is kept under
//! `refs/twr-trash/<item>/`, and the branch is deleted; nothing is deleted until a purge. When
//! the worktree's directory is already gone, its record (locked or not) moves alone; other
//! records are left alone. A directory that is no worktree any more (no `.git`, or one naming a
//! gone record of the repository; in no work tree, nor in the repository's directory) moves as
//! well.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use futures::future::{BoxFuture, FutureExt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use twr_core::event::{GitSnapshot, CONFLICT_FILES_MAX};
use twr_core::git::{
    parse_grep, parse_ls_tree, valid_commit_id, valid_read_path, valid_read_rev, GrepOut,
    GrepRequest, TreeEntry,
};
use twr_core::git::{
    parse_porcelain_v2, quote_path, unquote_path, DeleteRefs, GitOps, IgnoredGroup, LinkedWorktree,
    Listed, OnlyCommits, OtherBranches, PorcelainStatus, PurgeOutcome, RemoveOutcome, RepoLoss,
    RestoreOutcome, RestoreRequest, SharedCommits, TrashItem, TrashOutcome, TrashPurge,
    TrashRequest, WorktreeCreated, WorktreeInventory, WorktreeSpec, BRANCH_PREFIX,
    IGNORED_GROUPS_MAX, TRASH_REF_PREFIX,
};
use twr_core::paths::resolve_lenient;

use crate::workdirs::{Blocked, ChangeKind, WorkdirChange, Workdirs, CHANGE_WAIT};

pub mod dirfd;
mod trash;

/// Longest base ref accepted, in bytes.
const MAX_REF_BYTES: usize = 256;

/// Config every git command twr runs is given (`-c`), overriding the repository's: no
/// fsmonitor daemon or hook (an agent can write the repository's config), no network, and
/// untracked files always listed (a user's `status.showUntrackedFiles=no` would make an
/// untracked file look clean, and removal delete it).
pub const SAFE_CONFIG: &[&str] = &[
    "core.fsmonitor=false",
    "core.hooksPath=/dev/null",
    "status.showUntrackedFiles=normal",
    "status.submoduleSummary=false",
    // Commands go into submodules only where twr says so (`--ignore-submodules`).
    "submodule.recurse=false",
    "protocol.allow=never",
    // No `gc --auto` / maintenance detaching from the command's process group.
    "gc.auto=0",
    "maintenance.auto=false",
    // The literal object graph: no replacement objects (also turned off by
    // `GIT_NO_REPLACE_OBJECTS`, see [`LocalGit::command`]), and no hint on every command
    // about the legacy graft file, which `GIT_GRAFT_FILE=/dev/null` stands in for.
    "core.useReplaceRefs=false",
    "advice.graftFileDeprecated=false",
    // A diff of a submodule is its commits' names, never a diff of its files: that one runs
    // in the submodule with its own config's drivers, which no listing reads (over the
    // repository's `diff.submodule=diff`; an explicit `--submodule=diff` would still win).
    "diff.submodule=short",
];

/// Git subcommands that never run a program a repository's config names (a filter, a merge
/// or diff driver, a hook defined in config): they only read refs, objects and config, and
/// write nothing a hook is run for. Every other command (one that updates a ref or the index
/// too) gets [`LocalGit::neutralized`]'s overrides.
const DRIVERLESS: &[&str] = &[
    "rev-parse",
    "config",
    "for-each-ref",
    "show-ref",
    "rev-list",
    "ls-tree",
    "merge-base",
    "count-objects",
    "fsck",
    "var",
    "version",
];

/// Git subcommands that read objects (or the index) and run a driver only when asked to: by
/// the long options named (or any abbreviation git takes of them), or the short ones (alone or
/// in a cluster). `cat-file` and `grep` with `--textconv` / `--filters`; `ls-files` with
/// `-m` / `--modified`, which compares a racily clean file's content with the index through
/// its clean filter, and, to be safe (git 2.43 runs none for them), `-d` / `--deleted`, which
/// looks at each file in the work tree too, and `-t`. Not `show` or `log`: they run a diff
/// driver's `textconv` unasked (turned off like any other driver). A caller of either also
/// passes `--no-show-signature`: the program `gpg.program` names, which `log.showSignature`
/// runs, is not turned off.
const DRIVERLESS_UNLESS_ASKED: &[(&str, &[&str], &[char])] = &[
    ("cat-file", &["textconv", "filters"], &[]),
    ("grep", &["textconv"], &[]),
    ("ls-files", &["modified", "deleted"], &['m', 'd', 't']),
];

/// Whether git with `args` runs no driver a repository's config could name.
fn runs_no_driver(args: &[OsString]) -> bool {
    let Some(sub) = subcommand(args) else {
        return false;
    };
    if DRIVERLESS.contains(&sub) {
        return true;
    }
    let Some((_, long, short)) = DRIVERLESS_UNLESS_ASKED.iter().find(|(s, ..)| *s == sub) else {
        return false;
    };
    // An argument that is not text could be anything: asked. (Arguments after `--`, paths,
    // are looked at too: a path can only make this say "asked", never the other way.)
    !args.iter().any(|a| {
        let Some(a) = a.to_str() else {
            return true;
        };
        if let Some(name) = a.strip_prefix("--") {
            // Git takes any unambiguous beginning of a long option (`--mod`).
            let name = name.split('=').next().unwrap_or_default();
            !name.is_empty() && long.iter().any(|l| l.starts_with(name))
        } else if let Some(cluster) = a.strip_prefix('-') {
            // `-om`: each letter is an option (a letter in an option's value counts too).
            cluster.chars().any(|c| short.contains(&c))
        } else {
            false
        }
    })
}

/// Git's options before the subcommand that take their value as the next argument (`-C
/// <dir>`): [`subcommand`] skips that value.
const GLOBAL_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--super-prefix",
    "--config-env",
    "--attr-source",
];

/// The subcommand of `args`: its first argument that is neither an option nor the value of a
/// global option given as the next argument (`-C <dir>`, `--git-dir <dir>`). Callers pass
/// none of those (twr adds its own `-c` and `-C` in [`LocalGit::command`]), but a directory
/// named like a read-only subcommand must never pass for one.
fn subcommand(args: &[OsString]) -> Option<&str> {
    let mut args = args.iter();
    while let Some(a) = args.next() {
        let a = a.to_str()?;
        if GLOBAL_WITH_VALUE.contains(&a) {
            args.next();
        } else if !a.starts_with('-') {
            return Some(a);
        }
    }
    None
}

/// The command an override that must succeed runs, whatever git appends to it: git runs a
/// command with a space through its shell as `<command> "$@"`, so it is `:`, the shell's own
/// no-op, which every shell has and which takes any arguments (`exit 0` doesn't: bash, which
/// is `/bin/sh` on macOS, Fedora or Arch, refuses `exit 0 a b`, and the diff fails). A command
/// with no space git runs itself, found through `PATH`, which may name the repository's own
/// directory.
const SUCCEEDS: &str = ": off";

/// The command an override that must fail runs ([`SUCCEEDS`], negated).
const FAILS: &str = "! : off";

/// Overrides, `(key, value)`, that keep the drivers config `names` (`git config --list
/// --name-only -z`) defines from running anything: a filter's `clean`, `smudge` and `process`
/// empty (git then applies no filter) and not `required`; a merge driver [`FAILS`] (the merge
/// of such a file is a conflict, never a program the repository picked); a diff driver's
/// `textconv` [`FAILS`], never read from its cache (`cachetextconv`: notes the repository could
/// plant, keyed by the textconv string), and `command` [`SUCCEEDS`], and so `diff.external` (no
/// driver name: the program a diff runs for every file), which git runs even when empty (it
/// fails then). Where
/// a name makes Git LFS run a program ([`lfs_runs`]), Git LFS's filter (`filter.lfs`, yours)
/// is turned off too: its files stay pointers.
fn neutralize(names: &[u8]) -> anyhow::Result<Vec<(OsString, OsString)>> {
    use std::os::unix::ffi::OsStrExt;
    let mut out: std::collections::BTreeSet<(Vec<u8>, &'static str)> = Default::default();
    for name in names.split(|b| *b == 0).filter(|n| !n.is_empty()) {
        if lfs_runs(name) {
            for (k, v) in [
                (&b"filter.lfs.clean"[..], ""),
                (b"filter.lfs.smudge", ""),
                (b"filter.lfs.process", ""),
                (b"filter.lfs.required", "false"),
            ] {
                out.insert((k.to_vec(), v));
            }
        }
        // `section.subsection.key`: the section and key lowercased by git, the subsection as
        // written (any bytes, dots too).
        let Some(dot) = name.iter().position(|b| *b == b'.') else {
            continue;
        };
        let Some(last) = name.iter().rposition(|b| *b == b'.') else {
            continue;
        };
        if last <= dot {
            // `hook.command` and the like, no name: a hook all the same (git 2.54 and later).
            if matches!(name, b"hook.command" | b"hook.event" | b"hook.enabled") {
                out.insert((b"hook.enabled".to_vec(), "false"));
                out.insert((b"hook.command".to_vec(), SUCCEEDS));
            }
            if name == b"diff.external" {
                out.insert((b"diff.external".to_vec(), SUCCEEDS));
            }
            continue;
        }
        let (section, sub, key) = (&name[..dot], &name[dot + 1..last], &name[last + 1..]);
        // A name `-c` can't carry (`-c` splits at the first `=`): refused, not left to run.
        let driver = matches!(section, b"filter" | b"merge" | b"diff" | b"hook");
        if driver && sub.contains(&b'=') {
            bail!(
                "the repository's config names a driver twr can't turn off ({}): its git \
                 commands don't run there",
                String::from_utf8_lossy(name)
            );
        }
        let set: &[(&[u8], &'static str)] = match (section, key) {
            (b"filter", b"clean" | b"smudge" | b"process" | b"required") => &[
                (b"clean", ""),
                (b"smudge", ""),
                (b"process", ""),
                (b"required", "false"),
            ],
            // Shell builtins, run by git's own shell (a space in it): never a program found
            // through `PATH`, which may name the repository's own directory.
            (b"merge", b"driver") => &[(b"driver", FAILS)],
            // A hook defined in config (git 2.54 and later: `hook.<name>.command` for the
            // events `hook.<name>.event` names), which `core.hooksPath` does not turn off: off,
            // and should it run, it does nothing (allows).
            (b"hook", b"command" | b"event" | b"enabled") => {
                &[(b"enabled", "false"), (b"command", SUCCEEDS)]
            }
            (b"diff", b"textconv" | b"cachetextconv" | b"command") => &[
                (b"textconv", FAILS),
                (b"cachetextconv", "false"),
                (b"command", SUCCEEDS),
            ],
            _ => continue,
        };
        for (k, v) in set {
            let mut full = Vec::with_capacity(name.len() + 8);
            full.extend_from_slice(section);
            full.push(b'.');
            full.extend_from_slice(sub);
            full.push(b'.');
            full.extend_from_slice(k);
            out.insert((full, v));
        }
    }
    Ok(out
        .into_iter()
        .map(|(k, v)| (OsStr::from_bytes(&k).to_owned(), OsString::from(v)))
        .collect())
}

/// Whether the config name `name` makes Git LFS (its filter, which git runs for the files the
/// attributes give it) run a program: a custom transfer agent (`lfs.customtransfer.<agent>.*`,
/// which `lfs.standalonetransferagent`, `lfs.<url>.standalonetransferagent` or the LFS server
/// picks), an extension (`lfs.extension.<name>.*`), the askpass program it runs itself
/// (`core.askpass`), its ssh (`core.sshcommand`), or a credential helper of the `git
/// credential` it runs (`credential.helper`, `credential.<url>.helper`). Git LFS reads names
/// whatever their case. Its `.lfsconfig` (in the work tree) can set none of them: Git LFS
/// takes only a few keys from it, none naming a program.
fn lfs_runs(name: &[u8]) -> bool {
    let n = name.to_ascii_lowercase();
    n.starts_with(b"lfs.customtransfer.")
        || n.starts_with(b"lfs.extension.")
        || (n.starts_with(b"lfs.") && n.ends_with(b".standalonetransferagent"))
        || matches!(&n[..], b"core.askpass" | b"core.sshcommand")
        || (n.starts_with(b"credential.") && n.ends_with(b".helper"))
}

/// Of `git config --list --name-only --show-scope -z` output (`<scope>\0<name>\0` pairs),
/// the names set in a scope a repository controls (`local`, `worktree`), NUL-separated: not
/// `system`, `global` or `command` (twr's own `-c`, the user's environment).
fn repository_scoped(out: &[u8]) -> Vec<u8> {
    let mut names = Vec::new();
    let mut parts = out.split(|b| *b == 0);
    while let (Some(scope), Some(name)) = (parts.next(), parts.next()) {
        if !matches!(scope, b"system" | b"global" | b"command") && !name.is_empty() {
            names.extend_from_slice(name);
            names.push(0);
        }
    }
    names
}

/// The drivers (`filter.x`; `diff.external`, which has no name) of the overrides `after`
/// ([`neutralize`]) that `before` has no override for: what a command run with `before` may
/// have run. Sorted, each once.
fn unturned(before: &[(OsString, OsString)], after: &[(OsString, OsString)]) -> Vec<String> {
    let had: HashSet<&OsString> = before.iter().map(|(k, _)| k).collect();
    let mut new = BTreeSet::new();
    for (key, _) in after.iter().filter(|(k, _)| !had.contains(k)) {
        let key = key.to_string_lossy();
        // `section.subsection.key` without its key; one without a subsection whole.
        let driver = match (key.find('.'), key.rfind('.')) {
            (Some(dot), Some(last)) if last > dot => &key[..last],
            _ => &key[..],
        };
        new.insert(driver.to_string());
    }
    new.into_iter().collect()
}

/// A check made once a git command has ended ([`LocalGit::recheck`]), told whether it
/// succeeded.
type Recheck = Box<dyn FnOnce(bool) -> BoxFuture<'static, anyhow::Result<()>> + Send>;

/// How a git command with `args` is named in messages: its first two arguments.
fn what_of(args: &[OsString]) -> String {
    args.iter()
        .take(2)
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
/// The races [`LocalGit::drivers_unchanged`] found: the directory git ran in, and the drivers
/// (tests: told even when no caller waits any more).
static RACES: std::sync::Mutex<Vec<(PathBuf, String)>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
/// What runs once, between the listing of a repository's drivers and the command they go with,
/// for a command run in a directory under each path (tests: something writing the config the
/// command reads meanwhile).
static BETWEEN_LIST_AND_RUN: std::sync::Mutex<Vec<(PathBuf, Between)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
/// What [`BETWEEN_LIST_AND_RUN`] runs.
type Between = Box<dyn FnOnce() + Send>;

#[cfg(test)]
/// Runs (and forgets) what [`BETWEEN_LIST_AND_RUN`] holds for `dir`.
fn between_list_and_run(dir: &Path) {
    let at = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let hook = {
        let mut hooks = BETWEEN_LIST_AND_RUN.lock().unwrap();
        let k = hooks.iter().position(|(under, _)| at.starts_with(under));
        k.map(|k| hooks.remove(k).1)
    };
    if let Some(hook) = hook {
        hook();
    }
}

/// The checkout `dir` is in: the nearest of it and its parents with a `.git` of its own (a
/// thread working in a repository's subdirectory works in that repository's checkout); `dir`
/// when there is none.
fn checkout_top(dir: &Path) -> PathBuf {
    // Absolute (git reads `--work-tree` after `-C` moved it), its symlinks resolved, as git's
    // `-C` sees it: a link to a repository's subdirectory is in that repository.
    let dir = std::fs::canonicalize(dir)
        .or_else(|_| std::path::absolute(dir))
        .unwrap_or_else(|_| dir.to_path_buf());
    dir.ancestors()
        .find(|d| std::fs::symlink_metadata(d.join(".git")).is_ok())
        .unwrap_or(&dir)
        .to_path_buf()
}

/// A ref name as git printed it, only its line end gone (whitespace can be part of a name);
/// `None` when it is no UTF-8 (git allows that), which a recorded name could not keep: the
/// caller records the commit instead.
fn ref_name(out: &[u8]) -> Option<String> {
    let raw = out.strip_suffix(b"\n").unwrap_or(out);
    String::from_utf8(raw.to_vec()).ok()
}

/// A private directory of its own under the system's temporary one, removed with what is in it
/// when dropped: where a merge readiness probe runs.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> anyhow::Result<Scratch> {
        use std::os::unix::fs::DirBuilderExt;
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("twr-merge-{}-{n}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Scratch(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// How many times a new worktree's files are checked out again while its branch moves.
const CHECKOUT_TRIES: usize = 3;

/// The mark of a worktree whose files twr has not finished checking out, in its own git dir.
const CHECKOUT_PENDING: &str = "twr-checkout-pending";

/// Whether the worktree at `path` is one twr began and did not finish: its mark is there, or
/// its git dir has no index yet (its files were never checked out: the mark could not be
/// written).
fn unfinished(path: &Path) -> bool {
    pending_marker(path).is_some_and(|m| {
        m.exists()
            || m.parent()
                .is_some_and(|admin| admin.is_dir() && !admin.join("index").exists())
    })
}

/// Where the worktree at `path` keeps [`CHECKOUT_PENDING`]: its own git dir, which its `.git`
/// file names ([`dot_git`]: a regular file, no symlink, read only when small); `None` without
/// one.
fn pending_marker(path: &Path) -> Option<PathBuf> {
    let small =
        std::fs::symlink_metadata(path.join(".git")).is_ok_and(|m| m.is_file() && m.len() <= 4096);
    match small.then(|| dot_git(path))? {
        DotGit::Names(admin) => Some(admin.join(CHECKOUT_PENDING)),
        _ => None,
    }
}

/// Most repositories (the command's, the submodules below them) whose config is read before a
/// command: past it, the command does not run.
const CONFIG_CONTEXTS_MAX: usize = 64;

/// Deepest submodule nesting read: past it, the command does not run.
const SUBMODULE_DEPTH_MAX: usize = 4;

/// Where git with `args`, run in `dir`, reads config that can name a driver: `dir`, and the
/// worktree `worktree remove` removes (git checks it is clean first, with `git status` in it);
/// and whether it goes into submodules (a `git status` that doesn't ignore them, a worktree
/// removal's check); and the worktree it removes, if any.
fn contexts_of(dir: &Path, args: &[OsString]) -> (Vec<PathBuf>, bool, Option<PathBuf>) {
    let mut contexts = vec![dir.to_path_buf()];
    let words: Vec<&OsStr> = args
        .iter()
        .map(OsString::as_os_str)
        .filter(|a| !a.to_str().is_some_and(|a| a.starts_with('-')))
        .collect();
    match (subcommand(args), words.get(1).and_then(|w| w.to_str())) {
        (Some("status"), _) => {
            let ignoring = args.iter().any(|a| {
                matches!(
                    a.to_str(),
                    Some("--ignore-submodules=all" | "--ignore-submodules=dirty")
                )
            });
            (contexts, !ignoring, None)
        }
        (Some("worktree"), Some("remove")) => {
            let path = words.last().map(|p| dir.join(p)).filter(|p| p.is_dir());
            contexts.extend(path.clone());
            (contexts, true, path)
        }
        _ => (contexts, false, None),
    }
}

/// Ignored paths removing a worktree may delete: directories (`name/`, matched as any
/// directory component) of build output and caches that are regenerated, and a few files
/// (`name`, matched as any component). Ignored files elsewhere (`.env`, local config, notes)
/// keep the worktree. See [`LocalGit::with_park_disposable`].
pub const DEFAULT_PARK_DISPOSABLE: &[&str] = &[
    "target/",
    "node_modules/",
    "dist/",
    "build/",
    ".venv/",
    "venv/",
    "__pycache__/",
    ".next/",
    ".cache/",
    ".gradle/",
    ".pytest_cache/",
    ".mypy_cache/",
    ".tox/",
    "coverage/",
    ".DS_Store",
];

/// Largest stdout kept from a git command; more is an error.
pub const MAX_STDOUT: usize = 32 << 20;

/// Largest stderr kept from a git command (the rest is discarded).
const MAX_STDERR: usize = 64 << 10;

/// `git status` gives up (and is killed) after this long.
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);

/// Other read-only git commands give up (and are killed) after this long.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Commands that change the repository or a work tree get this long, then are killed (the
/// hub's wait for such an op, [`twr_core::git::CHANGE_REQUEST_TIMEOUT`], is derived from it).
const WORKTREE_TIMEOUT: Duration = twr_core::git::GIT_CHANGE_TIMEOUT;

/// Git of the local machine.
#[derive(Debug, Clone)]
pub struct LocalGit {
    bin: PathBuf,
    /// Ignored paths a removal may delete ([`DEFAULT_PARK_DISPOSABLE`]).
    disposable: Vec<String>,
    /// The worktrees directory whose stale records [`GitOps::worktree_prune`] may clear.
    managed: Option<PathBuf>,
    /// The box's trash directory ([`LocalGit::with_trash_root`]).
    trash: Option<PathBuf>,
    /// Timeouts, shortened by tests.
    timeouts: Timeouts,
    /// Most entries an inventory or a removal looks at searching a worktree for repositories
    /// ([`NESTED_SEARCH_MAX`], lowered by tests).
    search_max: u64,
    /// The box's working-directory reservations ([`crate::workdirs`]): worktree changes take
    /// theirs here, the box's runtimes their uses ([`LocalGit::workdirs`]). Shared by clones.
    workdirs: Arc<Workdirs>,
    /// Moves into the trash copy, as across file systems (tests).
    copy_moves: bool,
    /// The box's repository changes in progress ([`LocalGit::supervised`]). Shared by clones.
    changes: Arc<Changes>,
    /// The last merge readiness read for each work tree and base, with the commits it was
    /// read at ([`LocalGit::readiness`]). Shared by clones.
    readiness_cache: Arc<std::sync::Mutex<ReadinessCache>>,
    /// Run at each step of a restore or a trash move (tests).
    #[cfg(test)]
    step_hook: Option<trash::StepHook>,
}

/// [`LocalGit::readiness`]'s results, by (work tree, base): `(HEAD, base commit, readiness,
/// when)`.
type ReadinessCache = HashMap<(PathBuf, String), (String, String, Readiness, Instant)>;

/// How long a merge readiness is kept while neither tip moves: what else changes it (the
/// repository's attributes, a submodule's history) is seen by then.
const READINESS_FRESH: Duration = Duration::from_secs(300);

/// Most entries [`ReadinessCache`] keeps (it starts over past it).
const READINESS_CACHE_MAX: usize = 512;

/// How a work tree stands against a base, for merging ([`LocalGit::readiness`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Readiness {
    ahead: u64,
    behind: u64,
    /// The conflicting files, all of them (`None`: not known).
    conflicts: Option<Vec<String>>,
    /// Whether the merge conflicts (git may name no file for it); `None`: not known.
    conflicted: Option<bool>,
}

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    status: Duration,
    read: Duration,
    worktree: Duration,
}

impl Default for LocalGit {
    fn default() -> Self {
        LocalGit {
            bin: "git".into(),
            disposable: DEFAULT_PARK_DISPOSABLE
                .iter()
                .map(|s| s.to_string())
                .collect(),
            managed: None,
            trash: None,
            timeouts: Timeouts {
                status: STATUS_TIMEOUT,
                read: READ_TIMEOUT,
                worktree: WORKTREE_TIMEOUT,
            },
            search_max: NESTED_SEARCH_MAX,
            workdirs: Arc::default(),
            copy_moves: false,
            changes: Arc::default(),
            readiness_cache: Arc::default(),
            #[cfg(test)]
            step_hook: None,
        }
    }
}

/// A finished git command.
#[derive(Debug)]
struct Out {
    ok: bool,
    code: Option<i32>,
    /// Raw bytes: paths in git's output are bytes, not necessarily UTF-8. Anything that names
    /// a file goes through [`Out::into_stdout_bytes`]; only text (hashes, messages) is decoded.
    stdout: Vec<u8>,
    stderr: String,
}

impl Out {
    /// stdout as text (invalid UTF-8 replaced: never for paths).
    fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// stdout as text (invalid UTF-8 replaced: never for paths), or an error with git's
    /// message.
    fn into_stdout(self, what: &str) -> anyhow::Result<String> {
        if self.ok {
            Ok(match String::from_utf8(self.stdout) {
                Ok(s) => s,
                Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
            })
        } else {
            Err(self.error(what))
        }
    }

    /// stdout's raw bytes, or an error with git's message.
    fn into_stdout_bytes(self, what: &str) -> anyhow::Result<Vec<u8>> {
        if self.ok {
            Ok(self.stdout)
        } else {
            Err(self.error(what))
        }
    }

    fn error(&self, what: &str) -> anyhow::Error {
        let out = self.text();
        let msg = self.stderr.trim();
        let msg = if msg.is_empty() { out.trim() } else { msg };
        match self.code {
            Some(c) => anyhow!("{what} failed (git exit {c}): {msg}"),
            None => anyhow!("{what} failed (git killed by a signal): {msg}"),
        }
    }

    /// [`Out::error`] without git's message: for a command whose message may quote the
    /// repository's config (an invalid `remote.origin.fetch` holding a URL with a password,
    /// say), which twr must not show or log.
    fn error_without_message(&self, what: &str) -> anyhow::Error {
        match self.code {
            Some(c) => anyhow!("{what} failed (git exit {c})"),
            None => anyhow!("{what} failed (git killed by a signal)"),
        }
    }
}

/// How a git command is run. Either way it runs in a process group of its own, and when it
/// does not finish in time, or its caller gives up, the whole group is killed and waited for
/// ([`LocalGit::run_holding`]): no git twr starts outlives the call that started it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Read-only, given this long.
    Read(Duration),
    /// Read-only on a repository's committed content for someone who may not run anything
    /// (`LocalGit::committed`), given this long: git may not fetch, so no repository's config
    /// can make it run a transport (`ext::<command>`, a promisor remote's lazy fetch).
    Committed(Duration),
    /// Changes the repository or a work tree (`worktree add` / `remove`, `update-ref`,
    /// `branch -D`, `reset --hard`, `pack-objects`), given this long. Killed, it may have done
    /// part of its work: what it was changing is in an unknown state ([`Killed`]).
    Change(Duration),
}

/// A git command that did not finish in time, killed with every process of its group; the
/// error is returned once all of them are gone. A [`Mode::Change`] command may have done part
/// of its work (and left a `.lock` file): callers leave what it was changing as it is, and say
/// so, rather than clean it up.
#[derive(Debug)]
struct Killed {
    what: String,
    dir: PathBuf,
    after: Duration,
    change: bool,
}

impl std::fmt::Display for Killed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git {} in {} timed out after {:?} and was killed with everything it started",
            self.what,
            self.dir.display(),
            self.after
        )?;
        if self.change {
            f.write_str(
                "; what it was changing is in an unknown state (it may also have left a .lock \
                 file)",
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for Killed {}

/// Whether `e` comes from a git command that was killed ([`Killed`]).
fn was_killed(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Killed>())
}

/// How long a killed git's group may take to disappear before each warning that it has not.
const KILL_WARN_EVERY: Duration = Duration::from_secs(5);

/// The process group of a running git, while its leader has not been reaped (so the id names
/// that group and no other): [`Abandon`] kills it when the caller gives up, and
/// [`LocalGit::supervise`] takes it out just before reaping.
type GroupSlot = Arc<std::sync::Mutex<Option<u32>>>;

/// Takes the group id out of `slot`: from then on, nothing but the supervising task signals it.
fn take_group(slot: &GroupSlot) -> Option<u32> {
    slot.lock().unwrap_or_else(|p| p.into_inner()).take()
}

/// Held by the caller of a git command: dropped before the command ended (the caller gave
/// up), it kills the command's group at once, and the supervising task, told by the dropped
/// sender, waits for the group and reaps it.
struct Abandon {
    slot: GroupSlot,
    _gave_up: tokio::sync::oneshot::Sender<()>,
}

impl Drop for Abandon {
    fn drop(&mut self) {
        if let Some(pgid) = take_group(&self.slot) {
            crate::reap::signal(pgid, true, "KILL");
        }
    }
}

/// Why a git command stopped before it ended on its own.
enum Stop {
    TimedOut,
    Abandoned,
}

/// Waits until every process of the killed group `pgid` (`what`) has exited (zombies have);
/// warns every [`KILL_WARN_EVERY`] that one has not (a process in uninterruptible sleep, on a
/// hung file system), and keeps waiting: returning while one runs would let it change things
/// its caller then takes for settled. When the process table cannot be read, gives up waiting
/// for the group (with a warning); its leader is still reaped by the caller.
pub(crate) async fn wait_group_gone(pgid: u32, what: &str) {
    let started = tokio::time::Instant::now();
    let mut warn_at = started + KILL_WARN_EVERY;
    let mut pause = Duration::from_millis(2);
    loop {
        let running = tokio::task::spawn_blocking(move || crate::reap::group_running(pgid))
            .await
            .unwrap_or(None);
        match running {
            Some(false) => return,
            Some(true) => {}
            None => {
                tracing::warn!(pgid, command = %what, "cannot read the process table: not waiting for the rest of a killed git's process group");
                return;
            }
        }
        let now = tokio::time::Instant::now();
        if now >= warn_at {
            tracing::warn!(pgid, command = %what, waited_ms = (now - started).as_millis() as u64, "a killed git has not exited yet (uninterruptible sleep?); still waiting for it");
            warn_at = now + KILL_WARN_EVERY;
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(Duration::from_millis(100));
    }
}

/// The repository changes of one box in progress ([`LocalGit::supervised`]): how many, shared
/// by the clones of its [`LocalGit`].
#[derive(Debug)]
struct Changes(tokio::sync::watch::Sender<usize>);

impl Default for Changes {
    fn default() -> Self {
        Changes(tokio::sync::watch::Sender::new(0))
    }
}

/// One change counted in [`Changes`], until the task running it drops this (once all of its
/// work has ended).
struct Counted(Arc<Changes>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0 .0.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// Reads all of `r`, keeping at most `cap` bytes; returns them and whether more came.
async fn read_capped(
    r: Option<impl AsyncRead + Unpin>,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let Some(mut r) = r else {
        return Ok((Vec::new(), false));
    };
    let mut kept = Vec::new();
    let mut over = false;
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            return Ok((kept, over));
        }
        let room = cap.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
        over |= n > room;
    }
}

impl LocalGit {
    /// Git at `bin` instead of `git` on `PATH`.
    pub fn with_bin(bin: impl Into<PathBuf>) -> Self {
        LocalGit {
            bin: bin.into(),
            ..LocalGit::default()
        }
    }

    /// Ignored paths a worktree removal may delete, replacing [`DEFAULT_PARK_DISPOSABLE`]:
    /// `name/` matches a directory component, `name` any component (config
    /// `[threads] park_disposable`).
    pub fn with_park_disposable(mut self, disposable: Vec<String>) -> Self {
        self.disposable = disposable;
        self
    }

    /// At most `max` entries searched in ignored directories for repositories, per inventory.
    #[cfg(test)]
    fn with_search_max(mut self, max: u64) -> Self {
        self.search_max = max;
        self
    }

    /// The worktrees directory of this box: [`GitOps::worktree_prune`] clears only stale
    /// records of worktrees under it (without it, it clears nothing).
    pub fn with_managed_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.managed = Some(root.into());
        self
    }

    /// The box's trash directory (`<data dir>/trash`): deleted threads' worktrees, records
    /// and branches go there ([`GitOps::worktree_trash`]); without it, a deletion is an error.
    pub fn with_trash_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.trash = Some(root.into());
        self
    }

    /// Moves into the trash copy, as they do across file systems.
    #[cfg(test)]
    fn with_copy_moves(mut self) -> Self {
        self.copy_moves = true;
        self
    }

    /// Runs `hook` at each step of a restore, a trash move or a purge ([`trash::Step`]); an
    /// error it returns fails the operation there.
    #[cfg(test)]
    fn with_step_hook(
        mut self,
        hook: impl Fn(trash::Step, &Path) -> std::io::Result<()> + Send + Sync + 'static,
    ) -> Self {
        self.step_hook = Some(trash::StepHook(Arc::new(hook)));
        self
    }

    /// Runs `op`, one whole operation that changes the box's repositories or worktrees (see
    /// the module docs), as a task of its own, and returns a future that waits for its
    /// outcome. The task owns everything the operation takes (its working-directory
    /// reservations, a trash item's lock), every git it runs and every blocking job it starts
    /// (each awaited by it, never left running on its own); dropping the returned future (the
    /// caller gave up or went away) never stops it: it runs to its end, and what it holds goes
    /// only then. A panic in one of its blocking jobs reaches it once that job is over (an
    /// error the operation handles like any other); a panic of the task itself drops what it
    /// holds while none of its jobs runs, and is an error for the caller. Counted in
    /// [`LocalGit::changes_running`] until the task has ended.
    fn supervised<T: Send + 'static>(
        &self,
        what: &'static str,
        op: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    ) -> impl Future<Output = anyhow::Result<T>> + Send + 'static {
        self.changes.0.send_modify(|n| *n += 1);
        let counted = Counted(Arc::clone(&self.changes));
        let task = tokio::spawn(async move {
            let out = op.await;
            drop(counted);
            out
        });
        async move {
            match task.await {
                Ok(out) => out,
                Err(e) if e.is_panic() => Err(anyhow!("{what} failed: it panicked")),
                Err(e) => Err(anyhow!("{what} failed: {e}")),
            }
        }
    }

    /// How many operations that change the box's repositories or worktrees are running
    /// ([`LocalGit::supervised`]), whether their callers still wait for them or not.
    pub fn changes_running(&self) -> usize {
        *self.changes.0.borrow()
    }

    /// Waits until no operation that changes the box's repositories or worktrees runs
    /// ([`LocalGit::changes_running`]), at most `within`: whether none does.
    pub async fn changes_done(&self, within: Duration) -> bool {
        let mut rx = self.changes.0.subscribe();
        tokio::time::timeout(within, rx.wait_for(|n| *n == 0))
            .await
            .is_ok_and(|r| r.is_ok())
    }

    /// The URL of the `origin` remote of the repository at `dir` as git resolves it (`git
    /// remote get-url origin`: the `url.<base>.insteadOf` rewrites of the config applied; no
    /// network, like every command here), or `None` when the repository has no `origin`. Its
    /// text is the config's and may hold credentials: show it
    /// [`redact`](twr_core::gitremote::redact)ed. Its error never holds git's message, which
    /// may quote the config (`fatal: invalid refspec '<a URL with a password>'`), nor that of
    /// any step before it (the listing of the repository's config twr reads to turn its
    /// drivers off: `fatal: failed to expand user dir in: '<an include path>'`): it is one of a
    /// few fixed messages, whatever failed.
    pub async fn origin_url(&self, dir: &Path) -> anyhow::Result<Option<String>> {
        const WHAT: &str = "git remote get-url origin";
        let out = match self.git(dir, ["remote", "get-url", "origin"]).await {
            Ok(out) => out,
            Err(e) if was_killed(&e) => bail!("{WHAT} timed out and was killed"),
            Err(e) => {
                let missing = e.chain().any(|c| {
                    c.downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                });
                if missing {
                    bail!("{WHAT} failed: git is not installed (not on the PATH)");
                }
                bail!("{WHAT} failed: git could not run there, or read the repository's config");
            }
        };
        if out.ok {
            let url = String::from_utf8_lossy(trim_newline(&out.stdout)).into_owned();
            return Ok((!url.is_empty()).then_some(url));
        }
        if out.stderr.contains("No such remote") {
            return Ok(None);
        }
        Err(out.error_without_message(WHAT))
    }

    /// Whether this git can be run at all (`git --version`, given 10 s): `Err` when it can't
    /// (not installed: [`std::io::ErrorKind::NotFound`]), which [`GitOps::repo_root`] takes
    /// for "no repository" and a search must not.
    pub async fn runs(&self) -> std::io::Result<()> {
        let mut c = tokio::process::Command::new(&self.bin);
        c.arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        // No terminal: `bin` may be a wrapper (or a PATH lookup) that touches one.
        twr_adapters::process::new_session(&mut c);
        let mut child = c.spawn()?;
        match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
            Ok(Ok(s)) if s.success() => Ok(()),
            Ok(Ok(s)) => Err(std::io::Error::other(format!("git --version: {s}"))),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "git --version did not finish within 10 s",
            )),
        }
    }

    /// The trash directory, if set.
    pub fn trash_root(&self) -> Option<&Path> {
        self.trash.as_deref()
    }

    /// The managed worktrees directory, if set.
    pub fn managed_root(&self) -> Option<&Path> {
        self.managed.as_deref()
    }

    /// The box's working-directory reservations: a runtime of the box takes a use of its
    /// working directory here ([`Workdirs::begin_use`]) for as long as it runs, so that no
    /// worktree change of this git removes it under the runtime, nor does a runtime start in a
    /// worktree being changed.
    pub fn workdirs(&self) -> &Arc<Workdirs> {
        &self.workdirs
    }

    #[cfg(test)]
    fn with_timeouts(mut self, status: Duration, read: Duration, worktree: Duration) -> Self {
        self.timeouts = Timeouts {
            status,
            read,
            worktree,
        };
        self
    }

    fn command(
        &self,
        dir: &Path,
        args: &[OsString],
        stdin: bool,
        overrides: &[(OsString, OsString)],
    ) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.bin);
        for kv in SAFE_CONFIG {
            c.arg("-c").arg(kv);
        }
        for (k, v) in overrides {
            let mut kv = k.clone();
            kv.push("=");
            kv.push(v);
            c.arg("-c").arg(kv);
        }
        c.arg("-C").arg(dir);
        // What reads or writes a work tree does it in `dir`: never where a `core.worktree` the
        // repository's config sets points (another checkout's files, its submodules).
        let pinned = matches!(
            subcommand(args),
            Some(
                "reset"
                    | "read-tree"
                    | "checkout"
                    | "status"
                    | "ls-files"
                    | "update-index"
                    | "stash"
                    | "diff"
                    | "merge-tree"
            )
        );
        // A command that names its own git dir (a trash index read from the common dir) is set
        // up as its caller says.
        let own_setup = args
            .iter()
            .any(|a| a.to_str().is_some_and(|a| a.starts_with("--git-dir")));
        if pinned && !own_setup {
            let mut at = OsString::from("--work-tree=");
            at.push(checkout_top(dir));
            c.arg(at);
        }
        c.args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_NAMESPACE")
            // Config from the environment would apply after our `-c`.
            .env_remove("GIT_CONFIG_COUNT")
            // `git config` alone reads this file instead of the repository's: the listing of
            // its drivers ([`LocalGit::neutralized`]) must read what the command reads.
            .env_remove("GIT_CONFIG")
            // The literal object graph: what twr keeps, counts as lost or as
            // reached elsewhere must hold once a replacement (`git replace --graft`) or a
            // legacy graft (`info/grafts`) is gone, so no command sees either, nor objects or
            // a history cut the environment alone adds.
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_GRAFT_FILE", "/dev/null")
            .env_remove("GIT_REPLACE_REF_BASE")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_SHALLOW_FILE")
            .env("GIT_TERMINAL_PROMPT", "0")
            // No transport at all (over any `protocol.<name>.allow` the repository sets, `ext::`
            // included), and no lazy fetch of a missing object in a partial clone, where git
            // knows the variable: no git transport reaches outside the machine (a global LFS
            // filter makes its own requests: module docs).
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_NO_LAZY_FETCH", "1")
            // Programs the environment names for diffs and transports.
            .env_remove("GIT_EXTERNAL_DIFF")
            .env_remove("GIT_SSH")
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_PROXY_COMMAND")
            .env_remove("GIT_ASKPASS")
            // Messages the code matches ("not a git repository") are the untranslated ones.
            .env("LC_ALL", "C")
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Its own session and group: killed as a whole on timeout, out of reach of a
        // terminal's Ctrl-C meant for twr (a `worktree add` must not stop half-way), and
        // without a controlling terminal (a hook or filter that reads one gets an error
        // instead of being stopped).
        twr_adapters::process::new_session(&mut c);
        c
    }

    /// Talks to a spawned git: feeds it `stdin` and reads its capped output until git closes
    /// both. Does not wait for it: [`LocalGit::supervise`] reaps it.
    async fn exchange(
        child: &mut tokio::process::Child,
        stdin: Option<Vec<u8>>,
    ) -> (
        std::io::Result<(Vec<u8>, bool)>,
        std::io::Result<(Vec<u8>, bool)>,
    ) {
        let input = child.stdin.take();
        let feed = async move {
            if let (Some(mut w), Some(data)) = (input, stdin) {
                // A git that exits without reading all of it is fine (EPIPE).
                let _ = w.write_all(&data).await;
            }
        };
        let (_, out, err) = tokio::join!(
            feed,
            read_capped(child.stdout.take(), MAX_STDOUT),
            read_capped(child.stderr.take(), MAX_STDERR)
        );
        (out, err)
    }

    /// What a git that ended on its own (`status`) printed (`out`, `err`).
    fn finished(
        status: std::io::Result<std::process::ExitStatus>,
        out: std::io::Result<(Vec<u8>, bool)>,
        err: std::io::Result<(Vec<u8>, bool)>,
    ) -> anyhow::Result<Out> {
        let status = status.context("waiting for git")?;
        let (out, over) = out.context("reading git's output")?;
        if over {
            bail!("git printed more than {} MiB", MAX_STDOUT >> 20);
        }
        let (err, _) = err.unwrap_or_default();
        Ok(Out {
            ok: status.success(),
            code: status.code(),
            stdout: out,
            stderr: String::from_utf8_lossy(&err).into_owned(),
        })
    }

    /// Runs the spawned git `child` (process group `pgid`, also in `slot` until it is reaped)
    /// to its end: feeds it `stdin`, reads its output, reaps it. When it has not ended within
    /// `limit`, or the caller gave up (`gave_up` resolves: its sender is dropped), kills the
    /// whole group, waits until every process of it has exited ([`wait_group_gone`]) and
    /// reaps it. Returns only then, and only then drops `hold` (the reservation of what the
    /// command changes).
    #[allow(clippy::too_many_arguments)]
    async fn supervise(
        mut child: tokio::process::Child,
        pgid: u32,
        stdin: Option<Vec<u8>>,
        limit: Duration,
        slot: GroupSlot,
        mut gave_up: tokio::sync::oneshot::Receiver<()>,
        hold: Option<WorkdirChange>,
        what: String,
    ) -> Result<anyhow::Result<Out>, Stop> {
        let deadline = tokio::time::Instant::now() + limit;
        let talked = {
            let talk = Self::exchange(&mut child, stdin);
            tokio::pin!(talk);
            tokio::select! {
                r = &mut talk => Ok(r),
                _ = tokio::time::sleep_until(deadline) => Err(Stop::TimedOut),
                _ = &mut gave_up => Err(Stop::Abandoned),
            }
        };
        let ended = match talked {
            Ok((out, err)) => {
                // Its output is closed: it has exited, or is about to. Only this task reaps it,
                // and from here on it alone signals the group (the leader may be reaped below,
                // after which its id is free).
                take_group(&slot);
                let waited = tokio::select! {
                    s = child.wait() => Ok(s),
                    _ = tokio::time::sleep_until(deadline) => Err(Stop::TimedOut),
                    _ = &mut gave_up => Err(Stop::Abandoned),
                };
                waited.map(|status| (status, out, err))
            }
            Err(stop) => Err(stop),
        };
        let result = match ended {
            Ok((status, out, err)) => Ok(Self::finished(status, out, err)),
            Err(stop) => {
                // Not reaped (no wait above completed), so `pgid` still names its group.
                if child.id().is_some() {
                    crate::reap::signal(pgid, true, "KILL");
                }
                wait_group_gone(pgid, &what).await;
                take_group(&slot);
                let started = tokio::time::Instant::now();
                loop {
                    match tokio::time::timeout(KILL_WARN_EVERY, child.wait()).await {
                        Ok(_) => break,
                        Err(_) => {
                            tracing::warn!(pgid, command = %what, waited_ms = started.elapsed().as_millis() as u64, "a killed git has not exited yet; still waiting for it")
                        }
                    }
                }
                Err(stop)
            }
        };
        drop(hold);
        result
    }

    /// Runs `git <args>` in `dir` as `mode` says, with `stdin` fed to it.
    async fn run(
        &self,
        dir: &Path,
        args: Vec<OsString>,
        mode: Mode,
        stdin: Option<Vec<u8>>,
    ) -> anyhow::Result<Out> {
        self.run_holding(dir, args, mode, stdin, None).await
    }

    /// [`LocalGit::run`], keeping `hold` (a worktree change's reservation) until git has
    /// exited. Every git twr runs goes through here: in a process group of its own, supervised
    /// by a task ([`LocalGit::supervise`]) that kills the whole group when it has not finished
    /// within the mode's time, waits until every process of it has exited and reaps it; only
    /// then does this return (a [`Killed`] error) and `hold` go. When the caller gives up (this
    /// future is dropped), the group is killed at once and the task still waits for it and
    /// reaps it before `hold` goes.
    async fn run_holding(
        &self,
        dir: &Path,
        args: Vec<OsString>,
        mode: Mode,
        stdin: Option<Vec<u8>>,
        hold: Option<WorkdirChange>,
    ) -> anyhow::Result<Out> {
        // The drivers the repository's config names run nothing (see [`neutralize`]): each
        // one's override goes in a `-c`, which beats the config files and which every git it
        // runs (a submodule's status) inherits. Read where the command reads its config: its
        // directory, a worktree it removes (which `git status` checks first), and, for a
        // command that goes into submodules, each one's own repository.
        if runs_no_driver(&args) {
            return self
                .spawn_git(dir, args, mode, stdin, hold, Vec::new(), None)
                .await;
        }
        let (contexts, submodules, removes) = contexts_of(dir, &args);
        let overrides = self.neutralized(&contexts, submodules).await?;
        // The repository git finds from `dir`, as the listing saw it: one found there after git
        // ran (a `.git` planted in `dir` meanwhile) is another git may have used.
        let repository = self.repository_of(dir).await?;
        let recheck = self.recheck(
            dir,
            &args,
            &contexts,
            removes.as_deref(),
            submodules,
            overrides.clone(),
            repository,
        );
        #[cfg(test)]
        between_list_and_run(dir);
        self.spawn_git(dir, args, mode, stdin, hold, overrides, Some(recheck))
            .await
    }

    /// [`LocalGit::drivers_unchanged`] for git `args`, run in `dir` with `overrides` (read in
    /// `contexts`, `submodules`; `removes`, the worktree it removes) and `repository` (what
    /// [`LocalGit::repository_of`] found from `dir` then), as a future of its own: the task
    /// that supervises git runs it once git has ended, been killed or given up on and been
    /// reaped, whether or not anyone still waits for the result. The listing and the command
    /// are two git processes: a driver named in between ran, and is said even when no one is
    /// told. Made before git runs: each context's checkout ([`checkout_top`]) is recorded now,
    /// where the config is read again should the command (or a driver of it) remove the
    /// context itself.
    #[allow(clippy::too_many_arguments)]
    fn recheck(
        &self,
        dir: &Path,
        args: &[OsString],
        contexts: &[PathBuf],
        removes: Option<&Path>,
        submodules: bool,
        overrides: Vec<(OsString, OsString)>,
        repository: Option<Vec<u8>>,
    ) -> Recheck {
        let mut tops: Vec<(PathBuf, PathBuf)> = Vec::new();
        for ctx in contexts {
            if !tops.iter().any(|(c, _)| c == ctx) {
                tops.push((ctx.clone(), checkout_top(ctx)));
            }
        }
        let removes = removes.map(checkout_top);
        let (g, dir, what) = (self.clone(), dir.to_path_buf(), what_of(args));
        Box::new(move |succeeded| {
            async move {
                // Only what a removal that succeeded removed may be gone.
                let removed = removes.filter(|_| succeeded);
                g.drivers_unchanged(
                    &dir,
                    &what,
                    &tops,
                    removed.as_deref(),
                    submodules,
                    &overrides,
                    &repository,
                )
                .await
            }
            .boxed()
        })
    }

    /// After git `what` ran in `dir` with `overrides` ([`LocalGit::neutralized`] of
    /// `contexts`, `submodules`) and `repository` found from `dir` before it ran: an error,
    /// also logged, when the config those contexts read names a driver now that `overrides`
    /// did not turn off (git may have run it), when `dir` finds another repository now (git
    /// may have run that one's drivers), or when that can't be told any more: a context and
    /// its checkout gone or unreadable, but for `removed`, the worktree a removal that
    /// succeeded removed. Each context is read where git ran, as git found its repository
    /// there, while it is there; its checkout (`(context, checkout)`) when the command removed
    /// it.
    #[allow(clippy::too_many_arguments)]
    async fn drivers_unchanged(
        &self,
        dir: &Path,
        what: &str,
        contexts: &[(PathBuf, PathBuf)],
        removed: Option<&Path>,
        submodules: bool,
        overrides: &[(OsString, OsString)],
        repository: &Option<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let mut still: Vec<PathBuf> = Vec::new();
        let mut gone = None;
        for (ctx, top) in contexts {
            let at = match std::fs::symlink_metadata(ctx) {
                Err(e) if is_absent(&e) => top,
                _ => ctx,
            };
            match std::fs::symlink_metadata(at) {
                Ok(_) if still.contains(at) => {}
                Ok(_) => still.push(at.clone()),
                Err(e) if is_absent(&e) && removed == Some(top.as_path()) => {}
                Err(e) => {
                    gone = Some(anyhow!("{} can't be read again: {e}", at.display()));
                    break;
                }
            }
        }
        // Read again, then where git ran (while it is there) finds the repository the listing
        // read.
        let listed = match gone {
            Some(e) => Err(e),
            None => match self.neutralized(&still, submodules).await {
                Ok(after) if std::fs::symlink_metadata(dir).is_ok() => self
                    .repository_of(dir)
                    .await
                    .map(|now| (after, now != *repository)),
                Ok(after) => Ok((after, false)),
                Err(e) => Err(e),
            },
        };
        let (new, moved) = match listed {
            Ok((after, moved)) => (unturned(overrides, &after), moved),
            Err(e) => {
                let e = e.context(format!(
                    "after git {what} ran in {}, twr can't tell whether its repository's config \
                     named a program for it to run meanwhile",
                    dir.display()
                ));
                tracing::error!(dir = %dir.display(), command = %what, error = %format!("{e:#}"), "cannot list a repository's drivers again after a git command");
                return Err(e);
            }
        };
        if !new.is_empty() {
            let new = new.join(", ");
            tracing::error!(dir = %dir.display(), command = %what, drivers = %new, "a repository's config named a driver while twr's git ran: git may have run it");
            #[cfg(test)]
            RACES.lock().unwrap().push((dir.to_path_buf(), new.clone()));
            bail!(
                "while git {what} ran in {}, its repository's config came to name {new}, which \
                 twr had not turned off: git may have run it. Something is writing the \
                 repository's git config (its own, a worktree's config.worktree, or a file they \
                 include)",
                dir.display()
            )
        }
        if moved {
            tracing::error!(dir = %dir.display(), command = %what, "git found another repository where it ran than the one whose config twr read: git may have run its drivers");
            #[cfg(test)]
            RACES
                .lock()
                .unwrap()
                .push((dir.to_path_buf(), "another repository".into()));
            bail!(
                "while git {what} ran in {}, git came to find another repository there than the \
                 one whose config twr read (a .git file or directory written there or in a \
                 parent meanwhile): git may have run a program that repository's config names",
                dir.display()
            )
        }
        Ok(())
    }

    /// Runs git as [`LocalGit::run_holding`] says, with config `overrides` (`-c`), and once it
    /// has ended (or been killed) and been reaped, `recheck` ([`LocalGit::recheck`]), in the
    /// same task: what it finds fails a read git itself did not fail (a change keeps its own
    /// result: what it finds is only logged); a failure of git's own (a timeout) stays what is
    /// returned.
    #[allow(clippy::too_many_arguments)]
    async fn spawn_git(
        &self,
        dir: &Path,
        args: Vec<OsString>,
        mode: Mode,
        stdin: Option<Vec<u8>>,
        hold: Option<WorkdirChange>,
        overrides: Vec<(OsString, OsString)>,
        recheck: Option<Recheck>,
    ) -> anyhow::Result<Out> {
        let what = what_of(&args);
        let (limit, change) = match mode {
            Mode::Read(limit) | Mode::Committed(limit) => (limit, false),
            Mode::Change(limit) => (limit, true),
        };
        let mut cmd = self.command(dir, &args, stdin.is_some(), &overrides);
        // A backstop: should the supervising task itself fail, git is not left running.
        cmd.kill_on_drop(true);
        let spawn_err = || format!("running {} in {}", self.bin.display(), dir.display());
        let child = cmd.spawn().with_context(spawn_err)?;
        let pgid = child
            .id()
            .ok_or_else(|| anyhow!("git {what} in {} exited at once", dir.display()))?;
        let slot: GroupSlot = Arc::new(std::sync::Mutex::new(Some(pgid)));
        let (gave_up_tx, gave_up) = tokio::sync::oneshot::channel();
        let _abandon = Abandon {
            slot: Arc::clone(&slot),
            _gave_up: gave_up_tx,
        };
        let supervised =
            Self::supervise(child, pgid, stdin, limit, slot, gave_up, hold, what.clone());
        let task = tokio::spawn(async move {
            let ended = supervised.await;
            let succeeded = matches!(&ended, Ok(Ok(out)) if out.ok);
            let checked = match recheck {
                Some(recheck) => recheck(succeeded).await,
                None => Ok(()),
            };
            // A change that was made stays made: its caller records what it made, and failing
            // it would undo nothing (the race is logged all the same).
            match (ended, checked) {
                (Ok(Ok(_)), Err(raced)) if !change => Ok(Err(raced)),
                (ended, _) => ended,
            }
        });
        match task.await {
            Ok(Ok(r)) => r,
            Ok(Err(Stop::TimedOut)) => {
                tracing::warn!(dir = %dir.display(), command = %what, ?limit, change, "git timed out; killed it with its process group");
                Err(Killed {
                    what,
                    dir: dir.to_path_buf(),
                    after: limit,
                    change,
                }
                .into())
            }
            // Only when this future was dropped, so nobody sees it.
            Ok(Err(Stop::Abandoned)) => Err(anyhow!("git {what} abandoned")),
            Err(e) => Err(anyhow!("git {what} task failed: {e}")),
        }
    }

    /// The overrides ([`neutralize`]) for every filter, merge and diff driver the config git
    /// reads in each of `contexts` defines (the repository's own: its local and worktree
    /// config, the files they include, conditionally too, as they apply there), and with
    /// `submodules`, in each initialized submodule's own repository below them (a recursive
    /// `git status` runs git in each). Read just before the command they go with, and again
    /// after it ([`LocalGit::drivers_unchanged`]). An error when it can't tell them all: the
    /// command does not run.
    async fn neutralized(
        &self,
        contexts: &[PathBuf],
        submodules: bool,
    ) -> anyhow::Result<Vec<(OsString, OsString)>> {
        let mut names: Vec<u8> = Vec::new();
        // (context, depth) still to read. The command's own contexts are read with the work
        // tree pinned and not (a conditional include on the work tree applies as for the
        // command, however it is set up); a submodule's as git's own `status` in it sees it,
        // set up by its own config.
        let mut todo: Vec<(PathBuf, usize)> = contexts.iter().map(|c| (c.clone(), 0)).collect();
        let mut read = 0usize;
        while let Some((ctx, depth)) = todo.pop() {
            read += 1;
            if read > CONFIG_CONTEXTS_MAX {
                bail!(
                    "more than {CONFIG_CONTEXTS_MAX} repositories (submodules) to read the \
                     config of: twr can't tell which programs git would run"
                );
            }
            if depth > 0 {
                self.refuse_moved_worktree(&ctx).await?;
            }
            if depth == 0 {
                // The command's own: read both with the work tree pinned as a command on the
                // work tree is and as git sets it up by itself (a command that writes a ref
                // only), so a conditional include on the work tree is seen either way.
                names.extend(self.repository_drivers(&ctx, true).await?);
                names.push(0);
                names.extend(self.repository_drivers(&ctx, false).await?);
            } else {
                names.extend(self.repository_drivers(&ctx, false).await?);
            }
            names.push(0);
            if submodules {
                for sub in self.submodules_of(&checkout_top(&ctx)).await? {
                    if depth + 1 > SUBMODULE_DEPTH_MAX {
                        bail!(
                            "submodules nested deeper than {SUBMODULE_DEPTH_MAX} in {}: twr \
                             can't tell which programs git would run",
                            ctx.display()
                        );
                    }
                    todo.push((sub, depth + 1));
                }
            }
        }
        let lfs: BTreeSet<String> = names
            .split(|b| *b == 0)
            .filter(|n| lfs_runs(n))
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        if let (false, Some(ctx)) = (lfs.is_empty(), contexts.first()) {
            static TOLD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
            let top = checkout_top(ctx);
            let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
            if !told.contains(&top) {
                let keys = lfs.into_iter().collect::<Vec<_>>().join(", ");
                tracing::warn!(repository = %top.display(), %keys, "Git LFS is off for twr's git here: the repository's config names programs it would run; its files stay pointers in what twr checks out");
                told.push(top);
            }
        }
        neutralize(&names)
    }

    /// The config names set by the repository at `ctx` itself (local and worktree scopes,
    /// and what they include there), NUL-separated; read with the work tree pinned as its
    /// commands are, with `pinned`. What you set yourself, globally or for the
    /// system (Git LFS's filter), you trust. A git before 2.26 has no `--show-scope`: every
    /// name counts as the repository's then.
    async fn repository_drivers(&self, ctx: &Path, pinned: bool) -> anyhow::Result<Vec<u8>> {
        let list = |scoped: bool| {
            let mut args: Vec<OsString> = Vec::new();
            if pinned {
                let mut at = OsString::from("--work-tree=");
                at.push(checkout_top(ctx));
                args.push(at);
            }
            args.extend(
                ["config", "--includes", "--list", "--name-only", "-z"]
                    .iter()
                    .map(OsString::from),
            );
            if scoped {
                args.push("--show-scope".into());
            }
            args
        };
        // Commands that run no driver themselves: straight to git.
        let read = |args: Vec<OsString>| {
            self.spawn_git(
                ctx,
                args,
                Mode::Read(self.timeouts.read),
                None,
                None,
                Vec::new(),
                None,
            )
        };
        let out = read(list(true)).await?;
        if out.ok {
            return Ok(repository_scoped(&out.stdout));
        }
        read(list(false))
            .await?
            .into_stdout_bytes("git config --list")
    }

    /// The repository git finds from `dir`: its git directory and common directory as `git
    /// rev-parse` prints them (`--absolute-git-dir --git-common-dir`), `None` when it finds
    /// none. Compared only with another reading from the same `dir`.
    async fn repository_of(&self, dir: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        let out = self
            .spawn_git(
                dir,
                ["rev-parse", "--absolute-git-dir", "--git-common-dir"]
                    .map(OsString::from)
                    .to_vec(),
                Mode::Read(self.timeouts.read),
                None,
                None,
                Vec::new(),
                None,
            )
            .await?;
        Ok(out.ok.then_some(out.stdout))
    }

    /// An error when the submodule at `ctx` has its work tree elsewhere (its own config's
    /// `core.worktree`): git's `status` in it would go through that other directory, and its
    /// submodules, which twr does not read; the command does not run.
    async fn refuse_moved_worktree(&self, ctx: &Path) -> anyhow::Result<()> {
        let out = self
            .spawn_git(
                ctx,
                vec!["rev-parse".into(), "--show-toplevel".into()],
                Mode::Read(self.timeouts.read),
                None,
                None,
                Vec::new(),
                None,
            )
            .await?;
        // Its bytes as they are, only git's one line end gone: a name may be no UTF-8, or end
        // in a line break of its own.
        let raw = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
        let top = PathBuf::from(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(raw));
        let same = |a: &Path, b: &Path| match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if !out.ok || !same(&top, ctx) {
            bail!(
                "the submodule at {} has its work tree elsewhere ({}): twr can't tell which \
                 programs git would run there",
                ctx.display(),
                top.display()
            );
        }
        Ok(())
    }

    /// The initialized submodules of the work tree at `ctx`: its index's gitlinks whose
    /// directory has a `.git` of its own.
    async fn submodules_of(&self, ctx: &Path) -> anyhow::Result<Vec<PathBuf>> {
        use std::os::unix::ffi::OsStrExt;
        let out = self
            .spawn_git(
                ctx,
                ["ls-files", "--stage", "-z"]
                    .iter()
                    .map(OsString::from)
                    .collect(),
                Mode::Read(self.timeouts.read),
                None,
                None,
                Vec::new(),
                None,
            )
            .await?
            .into_stdout_bytes("git ls-files --stage")?;
        let mut subs = Vec::new();
        // `<mode> <object> <stage>\t<path>`
        for entry in out.split(|b| *b == 0) {
            let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
                continue;
            };
            if !entry.starts_with(b"160000 ") {
                continue;
            }
            let path = ctx.join(OsStr::from_bytes(&entry[tab + 1..]));
            // An unmerged gitlink is listed once per stage: one submodule all the same.
            if !subs.contains(&path) && std::fs::symlink_metadata(path.join(".git")).is_ok() {
                subs.push(path);
            }
        }
        Ok(subs)
    }

    /// A read-only git command (60 s).
    async fn git<I, S>(&self, dir: &Path, args: I) -> anyhow::Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        self.run(dir, args, Mode::Read(self.timeouts.read), None)
            .await
    }

    /// `git status` of the work tree at `dir`: untracked files always listed (untracked
    /// directories collapsed), ignored entries too with `ignored`, and with `submodules` what
    /// changed in a submodule's work tree too (else only its commit).
    async fn porcelain(
        &self,
        dir: &Path,
        ignored: bool,
        submodules: bool,
    ) -> anyhow::Result<PorcelainStatus> {
        let mut args: Vec<OsString> = [
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=normal",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        // Into each submodule's work tree (git runs in it), or only its commit.
        args.push(if submodules {
            "--ignore-submodules=none".into()
        } else {
            "--ignore-submodules=dirty".into()
        });
        if ignored {
            args.push("--ignored=matching".into());
        }
        let out = self
            .run(dir, args, Mode::Read(self.timeouts.status), None)
            .await?
            .into_stdout_bytes("git status")?;
        Ok(parse_porcelain_v2(&out)?)
    }

    /// How the work tree at `dir` stands against `base` (merge readiness): commits on
    /// each side, and the files a merge would conflict in (`git merge-tree --write-tree` with
    /// `--attr-source`, a recent git; unknown with an older one). Kept per work tree while
    /// neither its `HEAD` nor `base` moved (at most [`READINESS_FRESH`]).
    async fn readiness(&self, dir: &Path, base: &str) -> anyhow::Result<Readiness> {
        if base.is_empty()
            || base.starts_with('-')
            // (A recorded base is fully qualified: `refs/remotes/` before a name of the most.)
            || base.len() > MAX_REF_BYTES + "refs/remotes/".len()
            || base.contains('\0')
        {
            bail!("invalid base ref {base:?}");
        }
        let base_oid = self
            .git(
                dir,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{base}^{{commit}}"),
                ],
            )
            .await?;
        if !base_oid.ok {
            bail!("no commit {base:?} in the repository");
        }
        let base_oid = base_oid.text().trim().to_string();
        let head = self
            .git(dir, ["rev-parse", "--verify", "--quiet", "HEAD^{commit}"])
            .await?;
        if !head.ok {
            bail!("the work tree has no commit yet");
        }
        let head = head.text().trim().to_string();
        let key = (dir.to_path_buf(), base.to_string());
        if let Some((h, b, r, at)) = self
            .readiness_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            if *h == head && *b == base_oid && at.elapsed() < READINESS_FRESH {
                return Ok(r.clone());
            }
        }
        let counts = self
            .git(
                dir,
                [
                    "rev-list",
                    "--left-right",
                    "--count",
                    &format!("{base_oid}...{head}"),
                ],
            )
            .await?
            .into_stdout("git rev-list --count")?;
        let mut n = counts
            .split_whitespace()
            .map(|n| n.parse::<u64>().unwrap_or(0));
        let (behind, ahead) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
        // Its result tree goes into the object store, unreferenced (pruned by a gc). It runs
        // from a directory of its own against the worktree's git dir and work tree: a merge
        // driver (turned off, but still git's external one) writes its temporary files where
        // it runs, never into the checkout. The drivers to turn off are read in the worktree,
        // with that same work tree.
        let git_dir = self
            .git(dir, ["rev-parse", "--absolute-git-dir"])
            .await?
            .into_stdout_bytes("git rev-parse --absolute-git-dir")?;
        let git_dir = PathBuf::from(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(
            git_dir.strip_suffix(b"\n").unwrap_or(&git_dir),
        ));
        let overrides = self.neutralized(&[dir.to_path_buf()], false).await?;
        // The repository the listing read (git itself runs on `git_dir`): another one found
        // from `dir` once it ran is one whose config the second listing would read instead.
        let repository = self.repository_of(dir).await?;
        let scratch = Scratch::new()?;
        let mut at_git_dir = OsString::from("--git-dir=");
        at_git_dir.push(&git_dir);
        // Its work tree the checkout, named: a conditional include on the work tree applies as
        // when its drivers were read. Its attributes, which decide how files merge, read from
        // its `HEAD`'s tree (`--attr-source`, a recent git; an older one refuses it, and
        // the conflicts stay unknown): from outside the checkout git finds none of its
        // `.gitattributes`.
        let mut at_work_tree = OsString::from("--work-tree=");
        at_work_tree.push(checkout_top(dir));
        let args: Vec<OsString> = vec![
            format!("--attr-source={head}").into(),
            at_git_dir,
            at_work_tree,
            "merge-tree".into(),
            "--write-tree".into(),
            "--name-only".into(),
            "--no-messages".into(),
            "-z".into(),
            (&base_oid).into(),
            (&head).into(),
        ];
        // Checked again once it ran, as any command: a result that may come from a driver
        // named meanwhile is an error, not kept.
        let recheck = self.recheck(
            dir,
            &["merge-tree".into()],
            &[dir.to_path_buf()],
            None,
            false,
            overrides.clone(),
            repository,
        );
        #[cfg(test)]
        between_list_and_run(dir);
        let merged = self
            .spawn_git(
                scratch.path(),
                args,
                Mode::Read(self.timeouts.read),
                None,
                None,
                overrides,
                Some(recheck),
            )
            .await?;
        drop(scratch);
        let conflicts = match merged.code {
            Some(0) => Some(Vec::new()),
            // `<tree>\0<file>\0<file>\0...`: the files that conflict (quoted as git would a
            // name that is no UTF-8, so two such names stay two).
            Some(1) => Some(
                merged
                    .stdout
                    .split(|b| *b == 0)
                    .skip(1)
                    .filter(|f| !f.is_empty())
                    .map(quote_path)
                    .collect::<Vec<_>>(),
            ),
            // An older git without `--write-tree`, or a merge it could not do now (an object
            // missing from a partial clone): not known, and read again next time.
            _ => None,
        };
        let done = conflicts.is_some();
        let conflicted = match merged.code {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        };
        let r = Readiness {
            ahead,
            behind,
            conflicted,
            conflicts: conflicts.map(|mut c| {
                c.dedup();
                c
            }),
        };
        if done {
            let mut cache = self
                .readiness_cache
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if cache.len() >= READINESS_CACHE_MAX {
                cache.clear();
            }
            cache.insert(key, (head, base_oid, r.clone(), Instant::now()));
        }
        Ok(r)
    }

    /// Checks out the files of the commit the worktree at `path`'s `HEAD` names, into its index
    /// and work tree (`read-tree --reset -u`, which touches no ref); again while its `HEAD`
    /// moved meanwhile (another process on its branch), a few times at most.
    async fn check_out(&self, path: &Path, mode: Mode) -> anyhow::Result<String> {
        let head = |g: &Self| {
            let path = path.to_path_buf();
            let g = g.clone();
            async move {
                g.git(&path, ["rev-parse", "--verify", "HEAD^{commit}"])
                    .await?
                    .into_stdout("git rev-parse HEAD")
                    .map(|s| s.trim().to_string())
            }
        };
        let mut at = head(self).await?;
        for _ in 0..CHECKOUT_TRIES {
            let args: Vec<OsString> = vec![
                "read-tree".into(),
                "--reset".into(),
                "-u".into(),
                "--no-recurse-submodules".into(),
                (&at).into(),
            ];
            let out = self
                .run_holding(path, args, mode, None, None)
                .await?
                .into_stdout("git read-tree (checking out the worktree)")?;
            let now = head(self).await?;
            if now == at {
                return Ok(out);
            }
            at = now;
        }
        bail!(
            "{}'s branch kept moving while its files were checked out",
            path.display()
        )
    }

    /// Removes the worktree at `path` of `repo` that twr began and did not finish (its files
    /// never checked out in full: nothing in it is anyone's work), and `branch`, made for it.
    async fn undo_unfinished(
        &self,
        repo: &Path,
        path: &Path,
        expected: &str,
        branch: Option<(&String, &str)>,
    ) -> anyhow::Result<()> {
        // As for any removal: only a path whose record is proven its own both
        // ways, on the branch twr made it for, goes; nothing is touched otherwise (a symlink
        // there, another checkout put there with a mark of its own).
        let common = self.common_dir(repo).await?;
        let records = worktree_records(&common)?;
        match record_of(&common, &records, path, Some(expected), Some(&[])) {
            Ok(Some(_)) => {}
            Ok(None) => bail!(
                "{} has no worktree record of its own: nothing is touched",
                path.display()
            ),
            Err(e) => bail!("{e}; nothing is touched"),
        }
        let args: Vec<OsString> = vec![
            "worktree".into(),
            "remove".into(),
            "--force".into(),
            path.into(),
        ];
        self.run(repo, args, Mode::Change(self.timeouts.worktree), None)
            .await?
            .into_stdout("git worktree remove (an unfinished worktree)")?;
        // Only while it is still where it was made: a commit another process added to it
        // meanwhile keeps it.
        if let Some((b, at)) = branch {
            let args: Vec<OsString> = vec![
                "update-ref".into(),
                "-d".into(),
                format!("refs/heads/{b}").into(),
                at.into(),
            ];
            let out = self
                .run(repo, args, Mode::Change(self.timeouts.read), None)
                .await?;
            if !out.ok {
                tracing::info!(branch = %b, "kept the branch of an unfinished worktree: it moved since it was made");
            }
        }
        Ok(())
    }

    async fn status(&self, dir: &Path) -> anyhow::Result<GitSnapshot> {
        // Every 30 s: a submodule counts by its commit, its work tree is not gone through
        // (what a removal checks does go through it).
        Ok(self.porcelain(dir, false, false).await?.snapshot())
    }

    async fn create(self, spec: WorktreeSpec) -> anyhow::Result<WorktreeCreated> {
        let WorktreeSpec {
            repo,
            path,
            branch,
            base,
        } = spec;
        if let Some(b) = &base {
            if b.is_empty() || b.starts_with('-') || b.len() > MAX_REF_BYTES || b.contains('\0') {
                bail!("invalid base ref {b:?}");
            }
        }
        if !twr_core::git::valid_branch_component(branch.rsplit('/').next().unwrap_or_default()) {
            bail!("invalid branch name {branch:?}");
        }
        // Reserved until `worktree add` has finished (a removal of the path still running
        // finishes first); nothing starts in it meanwhile.
        let mut change = Some(
            self.workdirs
                .begin_change(&path, ChangeKind::Create, CHANGE_WAIT)
                .await
                .map_err(|b| anyhow!("cannot create the worktree {}: {b}", path.display()))?,
        );
        // One left unfinished (its checkout failed, or the process died during it) goes first.
        if unfinished(&path) {
            // Marked, or nothing in it but its `.git`: git's own doing. A checkout with no
            // index that holds files is left for the user to look at.
            let marked = pending_marker(&path).is_some_and(|m| m.exists());
            let bare = std::fs::read_dir(&path)
                .map(|d| d.flatten().all(|e| e.file_name() == ".git"))
                .unwrap_or(false);
            if !marked && !bare {
                bail!(
                    "{} is a worktree with no index that holds files: remove it yourself",
                    path.display()
                );
            }
            self.undo_unfinished(&repo, &path, &branch, None).await?;
        }
        // A worktree whose directory was deleted is still registered, and git refuses to add
        // another at its path: that record (and only that one) goes.
        if let Err(e) = self.clear_stale_records(&repo, Stale::Path(&path)).await {
            tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "cannot check for a stale worktree record");
        }
        let exists = self
            .git(
                &repo,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await?
            .ok;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let path_arg = path.as_os_str().to_owned();
        let add = Mode::Change(self.timeouts.worktree);
        // What its work merges back into (merge readiness), as a name that keeps meaning that
        // from another checkout: the branch the repository is on for its `HEAD`, a branch,
        // remote branch or tag given by name, else the commit the base named.
        let mut base_ref = None;
        // The commit a branch made for it starts at (what undoing it may delete).
        let mut made_at = None;
        if exists {
            if let Some(b) = &base {
                bail!("branch {branch} already exists; a base ({b}) only applies to a new branch");
            }
            let args: Vec<OsString> = vec![
                "worktree".into(),
                "add".into(),
                "--no-checkout".into(),
                path_arg,
                (&branch).into(),
            ];
            self.run_holding(&repo, args, add, None, None)
                .await?
                .into_stdout("git worktree add")?;
        } else {
            // What it merges back into is read first, and its start through that very name: a
            // checkout switching branches meanwhile can't pair one branch's commit with
            // another's name.
            // On a branch whose name is no UTF-8: it merges into its commit.
            let mut unnamed_branch = false;
            let named: Option<String> = match base.as_deref() {
                // Only git's line end goes: whitespace can be part of a ref's name.
                None | Some("HEAD" | "@") => {
                    let on = self.git(&repo, ["symbolic-ref", "-q", "HEAD"]).await?;
                    let name = on.ok.then(|| ref_name(&on.stdout)).flatten();
                    unnamed_branch = on.ok && name.is_none();
                    name.filter(|b| b.starts_with("refs/heads/"))
                }
                Some(b) => {
                    let full = self
                        .git(&repo, ["rev-parse", "--symbolic-full-name", b])
                        .await?;
                    // Fully qualified: a tag and a branch of one name stay apart.
                    ref_name(&full.stdout).filter(|f| {
                        f.starts_with("refs/heads/")
                            || f.starts_with("refs/remotes/")
                            || f.starts_with("refs/tags/")
                    })
                }
            };
            let start = match (&named, &base) {
                (Some(n), _) => n.clone(),
                (None, Some(b)) => b.clone(),
                (None, None) => "HEAD".into(),
            };
            let commit = self
                .git(
                    &repo,
                    [
                        "rev-parse",
                        "--verify",
                        "--quiet",
                        &format!("{start}^{{commit}}"),
                    ],
                )
                .await?;
            if !commit.ok {
                match base {
                    Some(b) => bail!("{b:?} is not a commit in {}", repo.display()),
                    None => bail!("{} has no commits yet", repo.display()),
                }
            }
            let commit = commit.text().trim().to_string();
            // A name it could not keep (no UTF-8), or no name (a revision like `HEAD~1`): the
            // commit. A detached `HEAD` names nothing to merge into.
            base_ref = match (named, base.as_deref()) {
                (Some(n), _) => Some(n),
                (None, None | Some("HEAD" | "@")) => unnamed_branch.then(|| commit.clone()),
                (None, Some(_)) => Some(commit.clone()),
            };
            made_at = Some(commit.clone());
            let args: Vec<OsString> = vec![
                "worktree".into(),
                "add".into(),
                "--no-checkout".into(),
                "-b".into(),
                (&branch).into(),
                path_arg,
                (&commit).into(),
            ];
            self.run_holding(&repo, args, add, None, None)
                .await?
                .into_stdout("git worktree add")?;
        }
        // Its files only now, in the worktree, where the config git reads is the one checking
        // them out reads (an `includeIf "onbranch:..."` of its branch included): the drivers
        // that config names are turned off before any file is written. Until they are, it is
        // marked unfinished: no one takes it for a worktree (a crash in between leaves the
        // mark, and the next creation at its path starts over).
        let pending = pending_marker(&path);
        let undo = |e: anyhow::Error| async {
            // Nothing in it but what git began to write: it goes, and a branch made for it
            // (only while it is where it was made).
            let made = made_at.as_deref().map(|at| (&branch, at));
            if let Err(undo) = self.undo_unfinished(&repo, &path, &branch, made).await {
                tracing::warn!(path = %path.display(), error = %format!("{undo:#}"), "cannot remove a worktree whose checkout failed");
            }
            e
        };
        if let Some(m) = &pending {
            if let Err(e) = tokio::fs::write(m, b"").await {
                let e = anyhow!(e).context(format!("marking {} unfinished", path.display()));
                return Err(undo(e).await);
            }
        }
        // `read-tree` of the commit its HEAD names, not `reset --hard`, which writes the commit
        // it read back over its branch: another process may move that branch meanwhile.
        // Should it, the files are read again for where it is now.
        if let Err(e) = self.check_out(&path, add).await {
            return Err(undo(e).await);
        }
        if let Some(m) = &pending {
            tokio::fs::remove_file(m)
                .await
                .with_context(|| format!("marking {} done", path.display()))?;
        }
        drop(change.take());
        let head = self
            .git(&path, ["rev-parse", "HEAD"])
            .await?
            .into_stdout("git rev-parse HEAD")?
            .trim()
            .to_string();
        Ok(WorktreeCreated {
            new_branch: !exists,
            head,
            base_ref,
        })
    }

    async fn remove(
        self,
        repo: PathBuf,
        path: PathBuf,
        branch: Option<String>,
    ) -> anyhow::Result<RemoveOutcome> {
        // Reserved from before the checks until `worktree remove` has finished, even if the
        // caller gives up or goes away first: no runtime starts in it meanwhile, and none may
        // be working in it now.
        let change = match self
            .workdirs
            .begin_change(&path, ChangeKind::Remove, CHANGE_WAIT)
            .await
        {
            Ok(c) => c,
            Err(b @ Blocked::InUse { .. }) => {
                tracing::warn!(path = %path.display(), reason = %b, "not removing a worktree a live runtime runs in");
                return Ok(RemoveOutcome::Kept {
                    reason: b.to_string(),
                });
            }
            Err(b @ Blocked::Busy(_)) => {
                bail!("cannot remove the worktree {}: {b}", path.display())
            }
        };
        // Missing only when known to be: an error that does not say "absent" (an ancestor
        // this user cannot search, ...) is an error, and nothing is touched.
        let missing = match tokio::fs::metadata(&path).await {
            Ok(m) => !m.is_dir(),
            Err(e) if is_absent(&e) => true,
            Err(e) => {
                return Err(anyhow!(
                    "cannot tell whether the worktree {} exists: {e}",
                    path.display()
                ))
            }
        };
        if missing {
            self.clear_stale_records(&repo, Stale::Path(&path)).await?;
            return Ok(RemoveOutcome::Missing);
        }
        // `git worktree remove` finds its worktree by the resolved path, so a
        // symlink there, or another checkout put there, would make it remove that one. Only
        // a path whose record is proven its own both ways ([`record_of`]) goes; nothing is
        // touched otherwise. A checkout of any other branch is kept below, whoever's branch it
        // is ([`RemoveOutcome::KeptCheckout`]): no list of the other threads' branches needed.
        let common = self.common_dir(&repo).await?;
        let records = worktree_records(&common)?;
        if let Err(e) = record_of(&common, &records, &path, branch.as_deref(), Some(&[])) {
            tracing::warn!(path = %path.display(), reason = %e, "not removing a worktree whose record is not proven its own");
            return Ok(RemoveOutcome::Kept {
                reason: format!("{e}; nothing is touched"),
            });
        }
        let st = self.porcelain(&path, true, true).await?;
        let dirty = st.dirty_files();
        if dirty > 0 {
            return Ok(RemoveOutcome::KeptDirty { dirty_files: dirty });
        }
        if let Some(branch) = &branch {
            if let Some(head) = self.other_checkout(&path, branch).await? {
                return Ok(RemoveOutcome::KeptCheckout { head });
            }
        }
        if let Some(reason) = self.hidden_files(&path).await? {
            return Ok(RemoveOutcome::Kept { reason });
        }
        let precious: Vec<&String> = st
            .ignored_paths
            .iter()
            .filter(|p| !disposable(p, &self.disposable))
            .collect();
        if !precious.is_empty() {
            return Ok(RemoveOutcome::Kept {
                reason: format!(
                    "{} ignored file(s) not known to be disposable: {}",
                    precious.len(),
                    examples(precious.iter().map(|p| p.as_str()))
                ),
            });
        }
        if let Some(reason) = self.repositories_kept(&path, &st.ignored_paths).await? {
            return Ok(RemoveOutcome::Kept { reason });
        }
        if let Some(commit) = self.unreachable_reflog_commit(&path).await? {
            return Ok(RemoveOutcome::Kept {
                reason: format!(
                    "commits only the worktree's own HEAD, reflogs or refs remember (e.g. {})",
                    &commit[..commit.len().min(12)]
                ),
            });
        }
        let args: Vec<OsString> = vec!["worktree".into(), "remove".into(), path.clone().into()];
        let out = self
            .run_holding(
                &repo,
                args,
                Mode::Change(self.timeouts.worktree),
                None,
                Some(change),
            )
            .await?;
        if !out.ok {
            // Something changed in between: git refuses dirty worktrees itself.
            if let Ok(s) = self.porcelain(&path, false, true).await {
                if s.dirty_files() > 0 {
                    return Ok(RemoveOutcome::KeptDirty {
                        dirty_files: s.dirty_files(),
                    });
                }
            }
            return Err(out.error("git worktree remove"));
        }
        // No prune: `worktree remove` clears its own record, and other stale records are
        // not twr's to clear.
        Ok(RemoveOutcome::Removed)
    }

    /// An error unless `path` is the top level of a linked worktree of `repo`: never its main
    /// checkout, another repository, or a directory inside a work tree.
    async fn check_linked_worktree(&self, repo: &Path, path: &Path) -> anyhow::Result<()> {
        let refuse = |why: &str| anyhow!("not deleting {}: {why}", path.display());
        let same = |a: &Path, b: &Path| match (resolve_lenient(a), resolve_lenient(b)) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        };
        let top = self.git(path, ["rev-parse", "--show-toplevel"]).await?;
        if !top.ok || !same(&path_of(trim_newline(&top.stdout)), path) {
            return Err(refuse("it is not the top level of a git work tree"));
        }
        // `(git dir, common dir)` of the work tree at `dir`, absolute.
        let dirs = |dir: &Path| {
            let dir = dir.to_path_buf();
            async move {
                let out = self
                    .git(
                        &dir,
                        ["rev-parse", "--absolute-git-dir", "--git-common-dir"],
                    )
                    .await?
                    .into_stdout_bytes("git rev-parse --git-common-dir")?;
                let mut lines = out.split(|b| *b == b'\n');
                let git_dir = path_of(lines.next().unwrap_or_default());
                let common = dir.join(path_of(trim_newline(lines.next().unwrap_or_default())));
                anyhow::Ok((git_dir, common))
            }
        };
        let (git_dir, common) = dirs(path).await?;
        let (_, repo_common) = dirs(repo).await?;
        if !same(&common, &repo_common) {
            return Err(refuse(&format!(
                "it is not a worktree of {}",
                repo.display()
            )));
        }
        if same(&git_dir, &common) {
            return Err(refuse("it is the repository's main checkout"));
        }
        Ok(())
    }

    /// Whether the directory `path` (present) was a linked worktree of `repo` and is no
    /// worktree any more, so [`GitOps::worktree_trash`] may move it to the trash:
    /// it has no `.git`, or a `.git` file pointing at a record of `repo` that is gone; and it
    /// is not inside a work tree (`git rev-parse` finds none), the repository's directory or
    /// its git directory. Never another repository, the main checkout, or a directory in one.
    async fn leftover(&self, repo: &Path, path: &Path) -> anyhow::Result<bool> {
        let dot_git = path.join(".git");
        match std::fs::symlink_metadata(&dot_git) {
            Err(e) if is_absent(&e) => {}
            Err(e) => {
                tracing::warn!(path = %dot_git.display(), error = %e, "cannot tell whether a worktree lost its .git");
                return Ok(false);
            }
            Ok(m) if !m.is_file() => return Ok(false),
            Ok(_) => {
                // `gitdir: <record>` (relative to the work tree with
                // `worktree.useRelativePaths`), naming a record of `repo` that is gone.
                let Ok(raw) = std::fs::read(&dot_git) else {
                    return Ok(false);
                };
                let Some(gitdir) = trim_newline(&raw).strip_prefix(b"gitdir: ") else {
                    return Ok(false);
                };
                let gitdir = path.join(path_of(gitdir));
                let common = self
                    .git(repo, ["rev-parse", "--git-common-dir"])
                    .await?
                    .into_stdout_bytes("git rev-parse --git-common-dir")?;
                let admin_root = repo.join(path_of(trim_newline(&common))).join("worktrees");
                let of_repo = match (
                    gitdir.parent().and_then(resolve_lenient),
                    resolve_lenient(&admin_root),
                ) {
                    (Some(a), Some(b)) => a == b,
                    _ => false,
                };
                if !of_repo || !matches!(presence(&gitdir), Presence::Absent) {
                    return Ok(false);
                }
            }
        }
        let top = self.git(path, ["rev-parse", "--show-toplevel"]).await?;
        if top.ok {
            return Ok(false);
        }
        let common = self
            .git(repo, ["rev-parse", "--git-common-dir"])
            .await?
            .into_stdout_bytes("git rev-parse --git-common-dir")?;
        let common = repo.join(path_of(trim_newline(&common)));
        let (Some(dir), Some(repo), Some(common)) = (
            resolve_lenient(path),
            resolve_lenient(repo),
            resolve_lenient(&common),
        ) else {
            return Ok(false);
        };
        Ok(!dir.starts_with(&repo) && !dir.starts_with(&common))
    }

    /// [`GitOps::branch_only_commits`]: the targets go by deletion set, the git common
    /// directory their repositories share (two checkouts of one repository are one set), each
    /// set counted on its own ([`LocalGit::set_losses`]). What cannot be inspected is said in
    /// the target's result; each names its git common directory when it could be found
    /// ([`OnlyCommits::common`]).
    async fn only_commits(self, targets: Vec<DeleteRefs>) -> anyhow::Result<Vec<OnlyCommits>> {
        let mut out = vec![OnlyCommits::default(); targets.len()];
        let mut sets: BTreeMap<PathBuf, Vec<usize>> = BTreeMap::new();
        for (i, t) in targets.iter().enumerate() {
            let common = match self.common_dir(&t.repo).await {
                Ok(c) => c,
                Err(e) => {
                    out[i].unchecked = Some(format!("{e:#}"));
                    continue;
                }
            };
            out[i].common = Some(common.clone());
            if let Some(b) = &t.branch {
                // Joined to the reflog directory: every component must be a valid one.
                if !b.split('/').all(twr_core::git::valid_branch_component) {
                    out[i].unchecked = Some(format!("invalid branch name {b:?}"));
                    continue;
                }
            }
            sets.entry(common).or_default().push(i);
        }
        for (common, members) in sets {
            let set: Vec<&DeleteRefs> = members.iter().map(|&i| &targets[i]).collect();
            match self.set_losses(&common, &set).await {
                Ok(losses) => {
                    for (&i, mut o) in members.iter().zip(losses) {
                        for s in &mut o.shared {
                            s.with = members[s.with];
                        }
                        o.common = Some(common.clone());
                        out[i] = o;
                    }
                }
                Err(e) => {
                    for &i in &members {
                        out[i].unchecked = Some(format!("{e:#}"));
                    }
                }
            }
        }
        Ok(out)
    }

    /// Of `others` ([`OtherBranches`]), the branches of threads of the repository whose git
    /// common directory is `common`: a branch of the same name in another repository is no
    /// other thread's checkout here. One whose repository cannot be resolved counts as this
    /// one's (more is refused, never less). `None` (an older hub's request) stays `None`.
    async fn branches_in(&self, common: &Path, others: &OtherBranches) -> Option<Vec<String>> {
        let others = others.as_ref()?;
        let mut here: HashMap<&Path, bool> = HashMap::new();
        let mut out = Vec::new();
        for o in others {
            let same = match here.get(o.repo.as_path()) {
                Some(same) => *same,
                None => {
                    let same = self.common_dir(&o.repo).await.map_or(true, |c| c == common);
                    here.insert(&o.repo, same);
                    same
                }
            };
            if same {
                out.push(o.branch.clone());
            }
        }
        Some(out)
    }

    /// The git common directory of the repository at `dir`, absolute and with symlinks
    /// resolved: what every checkout of one repository shares.
    async fn common_dir(&self, dir: &Path) -> anyhow::Result<PathBuf> {
        let out = self
            .git(dir, ["rev-parse", "--git-common-dir"])
            .await?
            .into_stdout_bytes("git rev-parse --git-common-dir")?;
        let common = dir.join(path_of(trim_newline(&out)));
        std::fs::canonicalize(&common).with_context(|| format!("resolving {}", common.display()))
    }

    /// What deleting `set` (threads of the repository whose git common directory is `common`)
    /// loses, one result per target (see [`OnlyCommits`]; [`SharedCommits::with`] indexes
    /// `set`). A commit is lost when the set's roots reach it and no surviving root does
    /// (`git rev-list <doomed> --not <surviving>`):
    ///
    /// - doomed: each branch's tip and every id of its reflog (`git branch -D` deletes both),
    ///   and each worktree's administrative record (`worktrees/<id>`, present or stale: the
    ///   deletion clears it either way): its detached `HEAD`, every id of its reflogs and its
    ///   own refs ([`record_roots`]);
    /// - surviving: every other ref (`git for-each-ref`: branches, tags, remote-tracking
    ///   branches, `refs/stash`, notes, ...), every other reflog under `logs/` (the main
    ///   worktree's `HEAD` reflog, the stash's), the main worktree's detached `HEAD`, and every
    ///   other worktree record's roots.
    ///
    /// A doomed reflog, record or object that cannot be read makes that target unchecked
    /// (never "nothing lost"); a surviving one that cannot be read is left out (more counts
    /// as lost, never less).
    ///
    /// A target whose worktree is on another branch than its own ([`switched_own`]) also gets
    /// that branch ([`OnlyCommits::checkout`]) and the commits of it nothing that stays reaches
    /// once that branch's own ref and reflog are left out ([`OnlyCommits::checkout_only`]).
    async fn set_losses(
        &self,
        common: &Path,
        set: &[&DeleteRefs],
    ) -> anyhow::Result<Vec<OnlyCommits>> {
        if !matches!(presence(&common.join("reftable")), Presence::Absent) {
            bail!(
                "{} may use the reftable ref storage, whose reflogs twr cannot read",
                common.display()
            );
        }
        let refs = self
            .git(
                common,
                ["for-each-ref", "--format=%(objectname) %(refname)"],
            )
            .await?
            .into_stdout("git for-each-ref")?;
        let refs: Vec<(&str, &str)> = refs.lines().filter_map(|l| l.split_once(' ')).collect();
        let records = worktree_records(common)?;
        let logs = common.join("logs");
        // Per target: its branch's roots (when the branch exists) and its record's (when it
        // has one), or why they cannot be read.
        let mut roots: Vec<Parts<String>> = Vec::new();
        let mut doomed_refs: HashSet<String> = HashSet::new();
        let mut doomed_records: HashSet<usize> = HashSet::new();
        // Per target: the branch its worktree is on when that is not its own (it stays).
        let mut checkouts: Vec<Option<Vec<u8>>> = Vec::new();
        for t in set {
            let branch = match &t.branch {
                Some(b) => {
                    let name = format!("refs/heads/{b}");
                    match refs.iter().find(|(_, r)| *r == name) {
                        Some((tip, _)) => {
                            doomed_refs.insert(name.clone());
                            read_reflog(&logs.join(&name)).map(|ids| Some(format!("{ids}{tip}\n")))
                        }
                        None => Ok(None),
                    }
                }
                None => Ok(None),
            };
            let mut checkout = None;
            let others = self.branches_in(common, &t.other_branches).await;
            let record = match &t.worktree {
                Some(w) => {
                    match record_of(common, &records, w, t.branch.as_deref(), others.as_deref()) {
                        Ok(Some(k)) => {
                            doomed_records.insert(k);
                            let on = match &t.branch {
                                Some(b) => record_branch(&records[k].admin)
                                    .map(|on| on.filter(|on| on != b.as_bytes())),
                                None => Ok(None),
                            };
                            on.and_then(|on| {
                                checkout = on;
                                record_roots(&records[k].admin).map(Some)
                            })
                        }
                        Ok(None) => Ok(None),
                        Err(e) => Err(e),
                    }
                }
                None => Ok(None),
            };
            checkouts.push(checkout);
            roots.push(branch.and_then(|b| record.map(|r| (b, r))));
        }
        // Every id that stays, with how many of what stays (a ref, a reflog, a `HEAD`, a
        // record) keep it: a switched-to branch's own ref and reflog are then left out of what
        // keeps its commits.
        let mut surviving: HashMap<String, usize> = HashMap::new();
        let mut add = |ids: &str| {
            for id in ids.lines().collect::<BTreeSet<_>>() {
                *surviving.entry(id.to_string()).or_default() += 1;
            }
        };
        // The reflogs that stay, as read.
        let mut kept_logs: HashMap<PathBuf, String> = HashMap::new();
        // The trash's refs and reflogs are no copy that stays: they are purged in their time.
        for (id, name) in &refs {
            if !doomed_refs.contains(*name) && !name.starts_with(TRASH_REF_PREFIX) {
                add(id);
            }
        }
        let doomed_logs: HashSet<PathBuf> = doomed_refs.iter().map(|r| logs.join(r)).collect();
        let trash_logs = logs.join(TRASH_REF_PREFIX.trim_end_matches('/'));
        match files_under(&logs) {
            Ok(files) => {
                for log in files
                    .iter()
                    .filter(|l| !doomed_logs.contains(*l) && !l.starts_with(&trash_logs))
                {
                    match read_reflog(log) {
                        Ok(ids) => {
                            add(&ids);
                            kept_logs.insert(log.clone(), ids);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "a reflog that stays cannot be read: what only it keeps counts as lost")
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "the reflogs that stay cannot be listed: what only they keep counts as lost")
            }
        }
        let head = common.join("HEAD");
        match std::fs::read(&head)
            .map_err(|e| e.to_string())
            .and_then(|h| head_tip(&h))
        {
            Ok(tip) => tip.iter().for_each(|t| add(t)),
            Err(e) => {
                tracing::warn!(head = %head.display(), error = %e, "the main worktree's HEAD cannot be read: what only it keeps counts as lost")
            }
        }
        for (k, r) in records.iter().enumerate() {
            if doomed_records.contains(&k) {
                continue;
            }
            match record_roots(&r.admin) {
                Ok(ids) => add(&ids),
                Err(e) => {
                    tracing::warn!(error = %e, "a worktree record that stays cannot be read: what only it keeps counts as lost")
                }
            }
        }
        // Objects that are gone: a doomed one makes its target unchecked; a surviving one
        // keeps nothing.
        let mut all: BTreeSet<&str> = surviving.keys().map(String::as_str).collect();
        for (b, r) in roots.iter().flatten() {
            all.extend(b.iter().chain(r.iter()).flat_map(|ids| ids.lines()));
        }
        let missing = self.missing_objects(common, &all).await?;
        let not: String = surviving
            .keys()
            .filter(|id| !missing.contains(*id))
            .map(|id| format!("^{id}\n"))
            .collect();
        // Per target: the lost commits its branch reaches, and its record.
        let mut lost: Vec<Parts<Commits>> = Vec::new();
        for r in roots {
            let r = match r {
                Ok((b, w)) => {
                    let gone = b
                        .iter()
                        .chain(w.iter())
                        .flat_map(|ids| ids.lines())
                        .find(|id| missing.contains(*id));
                    match gone {
                        Some(id) => Err(format!(
                            "commit {id} it keeps is missing from the repository"
                        )),
                        None => Ok((b, w)),
                    }
                }
                Err(e) => Err(e),
            };
            lost.push(match r {
                Ok((b, w)) => {
                    let reach = |ids: Option<String>| {
                        let not = &not;
                        async move {
                            match ids {
                                Some(ids) => self.lost_commits(common, ids, not).await.map(Some),
                                None => Ok(None),
                            }
                        }
                    };
                    match (reach(b).await, reach(w).await) {
                        (Ok(b), Ok(w)) => Ok((b, w)),
                        (Err(e), _) | (_, Err(e)) => Err(format!("{e:#}")),
                    }
                }
                Err(e) => Err(e),
            });
        }
        let reach: Vec<HashSet<&String>> = lost
            .iter()
            .map(|l| match l {
                Ok((b, w)) => b.iter().chain(w.iter()).flatten().collect(),
                Err(_) => HashSet::new(),
            })
            .collect();
        // Per target: the commits of the branch its worktree is on (not its own, so it stays)
        // that nothing else keeps once the set is gone: that branch's own ref and reflog left
        // out of what stays.
        let mut checkout_only: Vec<Option<u64>> = Vec::new();
        for on in &checkouts {
            let counted = match on.as_deref().map(std::str::from_utf8) {
                Some(Ok(on)) => {
                    let name = format!("refs/heads/{on}");
                    let tip = refs
                        .iter()
                        .find(|(_, r)| *r == name)
                        .map(|(id, _)| *id)
                        .filter(|id| !doomed_refs.contains(&name) && !missing.contains(*id));
                    match tip {
                        Some(tip) => {
                            let mut own: HashMap<&str, usize> = HashMap::from([(tip, 1)]);
                            if let Some(ids) = kept_logs.get(&logs.join(&name)) {
                                for id in ids.lines().collect::<BTreeSet<_>>() {
                                    *own.entry(id).or_default() += 1;
                                }
                            }
                            let not: String = surviving
                                .iter()
                                .filter(|(id, n)| {
                                    !missing.contains(*id)
                                        && **n > own.get(id.as_str()).copied().unwrap_or(0)
                                })
                                .map(|(id, _)| format!("^{id}\n"))
                                .collect();
                            match self.lost_commits(common, format!("{tip}\n"), &not).await {
                                Ok(c) => Some(c.len() as u64),
                                Err(e) => {
                                    tracing::warn!(branch = %on, error = %format!("{e:#}"), "cannot count the commits of the branch a worktree is on");
                                    None
                                }
                            }
                        }
                        None => None,
                    }
                }
                _ => None,
            };
            checkout_only.push(counted);
        }
        Ok(lost
            .iter()
            .enumerate()
            .map(|(i, l)| match l {
                Err(e) => OnlyCommits {
                    unchecked: Some(e.clone()),
                    ..Default::default()
                },
                Ok((branch, head)) => OnlyCommits {
                    branch: branch.as_ref().map(|b| b.len() as u64),
                    head: head.as_ref().map(|h| {
                        h.iter()
                            .filter(|c| !branch.as_ref().is_some_and(|b| b.contains(*c)))
                            .count() as u64
                    }),
                    commits: Listed::new(reach[i].iter().map(|c| c.to_string())),
                    shared: reach
                        .iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(j, other)| SharedCommits {
                            with: j,
                            commits: reach[i].intersection(other).count() as u64,
                        })
                        .filter(|s| s.commits > 0)
                        .collect(),
                    checkout: checkouts[i]
                        .as_deref()
                        .map(|on| String::from_utf8_lossy(on).into_owned()),
                    checkout_only: checkout_only[i],
                    unchecked: None,
                    common: None,
                },
            })
            .collect())
    }

    /// Which of the object ids `ids` the repository at `common` does not have.
    async fn missing_objects(
        &self,
        common: &Path,
        ids: &BTreeSet<&str>,
    ) -> anyhow::Result<HashSet<String>> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
        let args: Vec<OsString> = ["cat-file", "--batch-check=%(objectname)"]
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
        // `<id> missing` for each one it does not have.
        Ok(out
            .lines()
            .filter_map(|l| l.strip_suffix(" missing"))
            .map(str::to_string)
            .collect())
    }

    /// The commits `ids` (one per line) reach in the repository at `common` that none of `not`
    /// (`^<id>` per line) reaches.
    async fn lost_commits(
        &self,
        common: &Path,
        ids: String,
        not: &str,
    ) -> anyhow::Result<HashSet<String>> {
        if ids.trim().is_empty() {
            return Ok(HashSet::new());
        }
        let args: Vec<OsString> = ["rev-list", "--stdin"].iter().map(OsString::from).collect();
        let out = self
            .run(
                common,
                args,
                Mode::Read(self.timeouts.read),
                Some(format!("{ids}{not}").into_bytes()),
            )
            .await?
            .into_stdout("git rev-list")?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// [`GitOps::worktree_inventory`]: what deleting the linked worktree of `repo` at `path`
    /// removes. The worktree's administrative record ([`record_of`], the one the deletion
    /// clears, whether the directory is there or not; one that cannot be told apart is an
    /// error). Then, when the directory is there, the files of the work tree and of each
    /// repository inside it, at any depth ([`LocalGit::tree_files`]; a repository's under its
    /// path): the submodules checked out in it, the untracked repositories `git status` shows
    /// (`? dir/`), and every repository a walk of its whole directory finds, in tracked,
    /// untracked and ignored directories alike ([`repositories_in`], within
    /// [`LocalGit::search_max`] entries for the whole inventory: a directory not searched to
    /// the end is [`WorktreeInventory::unsearched`], the work tree's top as `./`; a bare one
    /// whose files are all tracked and unchanged loses nothing, [`holds_listed`]). Then the
    /// repositories
    /// that go, in the work tree or in the record ([`LocalGit::doomed_repositories`]), each
    /// with its commits on none of its remote-tracking branches. What of a repository cannot be
    /// checked is said in its [`RepoLoss::unchecked`]; the work tree itself, an error.
    async fn inventory(self, repo: PathBuf, path: PathBuf) -> anyhow::Result<WorktreeInventory> {
        let common = self.common_dir(&repo).await?;
        let records = worktree_records(&common)?;
        let record = record_of(&common, &records, &path, None, None)
            .map_err(|e| anyhow!("its worktree record: {e}"))?
            .map(|k| records[k].admin.clone());
        let gone = match tokio::fs::metadata(&path).await {
            Ok(m) => !m.is_dir(),
            Err(e) if is_absent(&e) => true,
            Err(e) => bail!("cannot tell whether {} exists: {e}", path.display()),
        };
        // Where file systems are mounted in it: a mount point refuses its deletion, and build
        // output is deleted only where they can be told ([`trash::mounts_inside`]).
        let mounts = if gone {
            None
        } else {
            let at = path.clone();
            tokio::task::spawn_blocking(move || trash::mounts_inside(&at))
                .await
                .context("looking for mount points in the worktree")?
        };
        // As the deletion goes: only with a mount list that has none in it.
        let mounts_known = matches!(&mounts, Some(Ok(m)) if m.is_empty());
        let (mut dirty, mut hidden, mut ignored) = (Vec::new(), Vec::new(), Vec::new());
        // The work tree's own ignored entries the deletion deletes ([`trash::build_output`]).
        let mut build: Vec<String> = Vec::new();
        let mut repos: BTreeMap<String, RepoLoss> = BTreeMap::new();
        let mut unsearched: Vec<String> = Vec::new();
        let mut budget = self.search_max;
        // The git directory of each repository inside the work tree, with its path as shown
        // and whether it is nested (no submodule).
        let mut inside: Vec<(PathBuf, String, bool)> = Vec::new();
        // Each work tree in it whose `.git` is a file, with the git directory it names.
        let mut dot_files: Vec<(PathBuf, PathBuf)> = Vec::new();
        // Work trees to look at: the raw path under `path` (empty: the work tree itself),
        // whether it is a nested repository, and how many repositories deep it is.
        let mut trees: Vec<(Vec<u8>, bool, usize)> = if gone {
            Vec::new()
        } else {
            vec![(Vec::new(), false, 0)]
        };
        while let Some((rel, nested, depth)) = trees.pop() {
            let top = rel.is_empty();
            let shown = quote_path(&rel);
            if depth > REPO_DEPTH_MAX {
                note(
                    &mut repos,
                    &shown,
                    nested,
                    "its files (repositories nested too deep)".into(),
                );
                continue;
            }
            let dir = if top {
                path.clone()
            } else {
                path.join(path_of(&rel))
            };
            let tree = match self.tree_files(&dir).await {
                Ok(t) => t,
                Err(e) if top => return Err(e),
                Err(e) => {
                    note(&mut repos, &shown, nested, format!("its files ({e:#})"));
                    continue;
                }
            };
            let join = |p: &[u8]| {
                if top {
                    p.to_vec()
                } else {
                    [rel.as_slice(), b"/", p].concat()
                }
            };
            // Repositories in it: its submodules (gitlinks), the untracked ones `git status`
            // shows (a repository with a `.git` as `dir/`), and every one the walk of the
            // whole directory finds ([`repositories_in`]: in tracked directories too, where
            // `git status` shows none). A bare one goes unless its files are all tracked and
            // unchanged ([`holds_listed`]).
            let listed: Vec<Vec<u8>> = tree
                .dirty
                .iter()
                .chain(&tree.hidden)
                .chain(&tree.ignored)
                .map(|p| join(&unquote_path(p.trim_end_matches('/'))))
                .collect();
            let mut found: Vec<(Vec<u8>, bool, Found)> = Vec::new();
            let mut seen: HashSet<Vec<u8>> = HashSet::new();
            for l in &tree.gitlinks {
                if seen.insert(join(l)) {
                    found.push((join(l), false, Found::WorkTree));
                }
            }
            for d in tree.dirty.iter().filter_map(|p| p.strip_suffix('/')) {
                let d = join(&unquote_path(d));
                if seen.insert(d.clone()) {
                    found.push((d, true, Found::WorkTree));
                }
            }
            let root = path.clone();
            let walked = rel.clone();
            let (repos_in, cut, left) = tokio::task::spawn_blocking(move || {
                let (found, cut) = repositories_in(&root, &walked, &|_| false, &mut budget);
                (found, cut, budget)
            })
            .await
            .context("looking for repositories in the work tree")?;
            budget = left;
            for (r, kind) in repos_in {
                if kind == Found::Bare && !holds_listed(&r, &listed) {
                    continue;
                }
                if seen.insert(r.clone()) {
                    found.push((r, true, kind));
                }
            }
            unsearched.extend(cut.iter().map(|d| match d.as_slice() {
                [] => "./".to_string(),
                d => format!("{}/", quote_path(d)),
            }));
            // A quoted path under a quoted path is the quoted joined path (`/` splits no
            // character or escape).
            if top && mounts_known && !self.disposable.is_empty() {
                let (root, own, list, mut b) = (
                    path.clone(),
                    tree.ignored.clone(),
                    self.disposable.clone(),
                    self.search_max,
                );
                build = tokio::task::spawn_blocking(move || {
                    trash::build_output(&root, &own, &list, &mut b)
                })
                .await
                .context("looking for build output in the work tree")?;
            }
            let under = |p: String| if top { p } else { format!("{shown}/{p}") };
            dirty.extend(tree.dirty.into_iter().map(under));
            hidden.extend(tree.hidden.into_iter().map(under));
            ignored.extend(tree.ignored.into_iter().map(under));
            for (sub, nested, kind) in found {
                let full = path.join(path_of(&sub));
                match kind {
                    // Its own git directory, no work tree: nothing more to list.
                    Found::Bare => inside.push((full, quote_path(&sub), nested)),
                    Found::WorkTree => match gitdir_of(&full) {
                        Ok(None) => {}
                        Ok(Some(gitdir)) => {
                            if gitdir != full.join(".git") {
                                dot_files.push((full.clone(), gitdir.clone()));
                            }
                            inside.push((gitdir, quote_path(&sub), nested));
                            trees.push((sub, nested, depth + 1));
                        }
                        Err(e) => note(&mut repos, &quote_path(&sub), nested, e),
                    },
                }
            }
        }
        let tree = (!gone).then_some(path.as_path());
        // Linked worktrees in it whose records stay behind: the deletion is refused.
        let doomed: Vec<PathBuf> = tree
            .into_iter()
            .chain(record.as_deref())
            .map(|p| resolve_lenient(p).unwrap_or_else(|| absolute(p)))
            .collect();
        let mut linked: Vec<LinkedWorktree> = dot_files
            .iter()
            .filter_map(|(dir, gitdir)| {
                outside_record(dir, gitdir, &doomed)
                    .map(|record| linked_worktree(dir, &record, Some(&common)))
            })
            .collect();
        linked.sort_by(|a, b| a.path.cmp(&b.path));
        for (shown, nested, repo) in self
            .doomed_repositories(tree, record.as_deref(), &inside)
            .await?
        {
            let gitdir = match repo {
                Ok(g) => g,
                Err(e) => {
                    note(&mut repos, &shown, nested, e);
                    continue;
                }
            };
            match self.unpublished_commits(&gitdir).await {
                Ok(c) if c.is_empty() => {}
                Ok(c) => repo_entry(&mut repos, &shown, nested).commits = Listed::new(c),
                Err(e) => note(&mut repos, &shown, nested, format!("its commits ({e:#})")),
            }
        }
        let root = path.clone();
        let paths: Vec<String> = ignored
            .iter()
            .filter(|p| !build.contains(p))
            .cloned()
            .collect();
        let (ignored_groups, ignored_groups_total) =
            tokio::task::spawn_blocking(move || ignored_groups(&root, &paths))
                .await
                .context("counting ignored files")?;
        // Each once, and none in another (the top level `./` holds every one).
        unsearched.sort();
        unsearched.dedup();
        let all = unsearched.clone();
        unsearched.retain(|u| {
            !all.iter()
                .any(|o| o != u && (o == "./" || u.starts_with(o.as_str())))
        });
        // What the trash item will hold on disk: the directory and the record.
        let sized: Vec<PathBuf> = tree
            .map(Path::to_path_buf)
            .into_iter()
            .chain(record.clone())
            .collect();
        let built: Vec<PathBuf> = build
            .iter()
            .map(|p| path.join(path_of(&unquote_path(p.trim_end_matches('/')))))
            .collect();
        let (bytes, build_output_bytes) = tokio::task::spawn_blocking(move || {
            let built: u64 = built.iter().map(|p| trash::disk_usage(p)).sum();
            let all: u64 = sized.iter().map(|p| trash::disk_usage(p)).sum();
            (all.saturating_sub(built), built)
        })
        .await
        .context("measuring the worktree")?;
        let (mounts, mounts_unknown) = match mounts {
            Some(Ok(m)) => (m, false),
            Some(Err(_)) => (Vec::new(), true),
            None => (Vec::new(), false),
        };
        Ok(WorktreeInventory {
            dirty: Listed::new(dirty),
            hidden: Listed::new(hidden),
            ignored: Listed::new(ignored),
            ignored_groups,
            ignored_groups_total,
            repositories: repos.into_values().collect(),
            unsearched,
            linked,
            gone,
            bytes,
            build_output: Listed::new(build),
            build_output_bytes,
            mounts,
            mounts_unknown,
        })
    }

    /// What of the files of the work tree at `dir` (not of the repositories in it) removing it
    /// deletes:
    /// what `git status` shows (each untracked file), what it hides ([`LocalGit::tracked`]) and
    /// what it ignores; and its submodules (gitlinks), by raw path.
    async fn tree_files(&self, dir: &Path) -> anyhow::Result<TreeFiles> {
        let args: Vec<OsString> = [
            "--no-optional-locks",
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
            "--ignore-submodules=none",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        // Raw bytes: a file name need not be UTF-8, and two replaced names would read as one.
        let out = self
            .run(dir, args, Mode::Read(self.timeouts.status), None)
            .await?
            .into_stdout_bytes("git status")?;
        let st = parse_porcelain_v2(&out)?;
        let (hidden, gitlinks) = self.tracked(dir).await?;
        Ok(TreeFiles {
            dirty: st.dirty_paths,
            hidden,
            ignored: st.ignored_paths,
            gitlinks,
        })
    }

    /// The repositories that deleting a worktree removes, each with its path as shown and
    /// whether it is nested (or why it cannot be told apart): every git directory in its
    /// administrative record `record` (when it has one), at any depth ([`repositories_under`]:
    /// the submodules' repositories under `modules/`, where git keeps a linked worktree's, and
    /// any other); and those of `inside` (each repository's git directory inside the work tree
    /// `tree`, when its directory is there, as shown, nested or not) that are in the work tree
    /// or the record (a bare repository, a `.git` directory, or a `.git` file naming one
    /// there), with every git directory in them. A repository whose git directory is elsewhere
    /// (a linked worktree of another repository) loses nothing. One that none of `inside`
    /// names is shown by its git directory.
    async fn doomed_repositories(
        &self,
        tree: Option<&Path>,
        record: Option<&Path>,
        inside: &[(PathBuf, String, bool)],
    ) -> anyhow::Result<Vec<(String, bool, Result<PathBuf, String>)>> {
        let resolved = |p: &Path| resolve_lenient(p).unwrap_or_else(|| absolute(p));
        let record = record.map(resolved);
        let tree = tree.map(resolved);
        let names: BTreeMap<PathBuf, (&String, bool)> = inside
            .iter()
            .map(|(g, shown, nested)| (resolved(g), (shown, *nested)))
            .collect();
        let mut found: BTreeMap<PathBuf, Result<(), String>> = BTreeMap::new();
        let mut scan: Vec<PathBuf> = record.iter().cloned().collect();
        for g in names.keys() {
            let doomed = [&tree, &record]
                .into_iter()
                .flatten()
                .any(|d| g.starts_with(d));
            if doomed {
                found.insert(g.clone(), Ok(()));
                scan.push(g.clone());
            }
        }
        tokio::task::spawn_blocking(move || {
            // Each directory once: one under another is searched with it.
            scan.sort();
            scan.dedup_by(|later, kept| later.starts_with(kept));
            for root in scan {
                let (repos, unread) = repositories_under(&root);
                for r in repos {
                    found.entry(r).or_insert(Ok(()));
                }
                for (at, e) in unread {
                    found.insert(at, Err(e));
                }
            }
            found
        })
        .await
        .context("looking for repositories in git directories")
        .map(|found| {
            found
                .into_iter()
                .map(|(g, r)| {
                    let (shown, nested) = names
                        .get(&g)
                        .map(|(s, n)| (s.to_string(), *n))
                        .unwrap_or_else(|| (quote_path(g.as_os_str().as_bytes()), false));
                    (shown, nested, r.map(|()| g))
                })
                .collect()
        })
    }

    /// The commits of the repository whose git directory is `gitdir` that none of its
    /// remote-tracking branches reaches: from its refs, `HEAD`s and reflogs.
    async fn unpublished_commits(&self, gitdir: &Path) -> anyhow::Result<Vec<String>> {
        // Its work tree may be gone (`core.worktree` names it): none is needed.
        let mut args: Vec<OsString> = Vec::new();
        for (opt, dir) in [("--git-dir=", gitdir), ("--work-tree=", gitdir)] {
            let mut a = OsString::from(opt);
            a.push(dir);
            args.push(a);
        }
        args.extend(
            ["rev-list", "--all", "--reflog", "--not", "--remotes"]
                .iter()
                .map(OsString::from),
        );
        let out = self
            .run(gitdir, args, Mode::Read(self.timeouts.read), None)
            .await?
            .into_stdout("git rev-list")?;
        Ok(out
            .lines()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Of the index of the work tree at `path`: the tracked files whose content on disk differs
    /// from the index while `git status` does not show it ([`quote_path`]), and the submodules
    /// (gitlinks), by raw path. Hidden: marked assume-unchanged or skip-worktree (a sparse
    /// checkout's absent skip-worktree files, and absent assume-unchanged ones, hold nothing
    /// the index does not). Compared without filters (none of the repository's config runs):
    /// a file that differs only by a filter or line-ending conversion counts as changed. One
    /// that is no longer a file of its kind (a directory now, ...) or cannot be looked at
    /// counts as changed.
    async fn tracked(&self, path: &Path) -> anyhow::Result<(Vec<String>, Vec<Vec<u8>>)> {
        // Raw bytes: a file name need not be UTF-8, and a replaced name would look absent.
        let out = self
            .git(path, ["ls-files", "-v", "-s", "-z"])
            .await?
            .into_stdout_bytes("git ls-files")?;
        let mut changed: Vec<String> = Vec::new();
        let mut gitlinks: Vec<Vec<u8>> = Vec::new();
        // Regular files to hash, by raw name, with their index blob; symlinks likewise.
        let mut files: Vec<(Vec<u8>, String)> = Vec::new();
        let mut links: Vec<(Vec<u8>, String, Vec<u8>)> = Vec::new();
        for rec in out.split(|b| *b == 0) {
            // `<tag> <mode> <object> <stage>\t<file>`
            let Some(tab) = rec.iter().position(|b| *b == b'\t') else {
                continue;
            };
            let (head, file) = (&rec[..tab], &rec[tab + 1..]);
            let fields: Vec<&[u8]> = head.split(|b| *b == b' ').collect();
            let [tag, mode, object, stage] = fields[..] else {
                continue;
            };
            if mode == b"160000" {
                // Once per path, also in a conflict.
                if gitlinks.last().is_none_or(|l| l.as_slice() != file) {
                    gitlinks.push(file.to_vec());
                }
                continue;
            }
            let assume_unchanged = !tag.is_empty() && tag.iter().all(u8::is_ascii_lowercase);
            let skip_worktree = tag == b"S" || tag == b"s";
            // A submodule's own state is not the index's; a conflict shows in `git status`.
            if !(assume_unchanged || skip_worktree) || stage != b"0" {
                continue;
            }
            let name = Path::new(OsStr::from_bytes(file));
            let shown = quote_path(file);
            let object = String::from_utf8_lossy(object).into_owned();
            let on_disk = path.join(name);
            match std::fs::symlink_metadata(&on_disk) {
                Err(e) if is_absent(&e) => {}
                Err(_) => changed.push(shown),
                Ok(m) if m.file_type().is_symlink() && mode == b"120000" => {
                    match std::fs::read_link(&on_disk) {
                        Ok(target) => {
                            links.push((file.to_vec(), object, target.into_os_string().into_vec()))
                        }
                        Err(_) => changed.push(shown),
                    }
                }
                Ok(m) if m.is_file() && (mode == b"100644" || mode == b"100755") => {
                    // `--stdin-paths` reads one name per line.
                    if file.contains(&b'\n') {
                        changed.push(shown);
                    } else {
                        files.push((file.to_vec(), object));
                    }
                }
                Ok(_) => changed.push(shown),
            }
        }
        if !files.is_empty() {
            let mut names: Vec<u8> = Vec::new();
            for (f, _) in &files {
                names.extend_from_slice(f);
                names.push(b'\n');
            }
            let args: Vec<OsString> = ["hash-object", "--no-filters", "--stdin-paths"]
                .iter()
                .map(OsString::from)
                .collect();
            let out = self
                .run(path, args, Mode::Read(self.timeouts.read), Some(names))
                .await?
                .into_stdout("git hash-object")?;
            let ids: Vec<&str> = out.lines().map(str::trim).collect();
            if ids.len() != files.len() {
                bail!(
                    "git hash-object printed {} ids for {} files",
                    ids.len(),
                    files.len()
                );
            }
            for ((f, object), id) in files.iter().zip(ids) {
                if id != object {
                    changed.push(quote_path(f));
                }
            }
        }
        for (f, object, target) in links {
            let args: Vec<OsString> = ["hash-object", "--stdin"]
                .iter()
                .map(OsString::from)
                .collect();
            let id = self
                .run(path, args, Mode::Read(self.timeouts.read), Some(target))
                .await?
                .into_stdout("git hash-object")?;
            if id.trim() != object {
                changed.push(quote_path(&f));
            }
        }
        Ok((changed, gitlinks))
    }

    /// `None` when the work tree at `path` has `refs/heads/<branch>` checked out; otherwise
    /// what it has instead (`branch <name>` or `detached at <commit>`).
    async fn other_checkout(&self, path: &Path, branch: &str) -> anyhow::Result<Option<String>> {
        let sym = self.git(path, ["symbolic-ref", "-q", "HEAD"]).await?;
        let want = format!("refs/heads/{branch}");
        if sym.ok {
            // Compared as bytes: a ref name need not be UTF-8.
            if trim_newline(&sym.stdout) == want.as_bytes() {
                return Ok(None);
            }
            let head = sym.text();
            let head = head.trim();
            let name = head.strip_prefix("refs/heads/").unwrap_or(head);
            return Ok(Some(format!("branch {name}")));
        }
        let commit = self
            .git(path, ["rev-parse", "--short", "HEAD"])
            .await?
            .into_stdout("git rev-parse HEAD")?;
        Ok(Some(format!("detached at {}", commit.trim())))
    }

    /// Why tracked files of the work tree at `path` may hold changes `git status` does not
    /// show: marked assume-unchanged, or skip-worktree while present on disk (a sparse
    /// checkout's absent skip-worktree files hold nothing).
    async fn hidden_files(&self, path: &Path) -> anyhow::Result<Option<String>> {
        // Raw bytes: a file name need not be UTF-8, and a replaced name would look absent.
        let out = self
            .git(path, ["ls-files", "-v", "-z"])
            .await?
            .into_stdout_bytes("git ls-files")?;
        let mut hidden: Vec<String> = Vec::new();
        for rec in out.split(|b| *b == 0) {
            let Some(sp) = rec.iter().position(|b| *b == b' ') else {
                continue;
            };
            let (tag, file) = (&rec[..sp], &rec[sp + 1..]);
            let assume_unchanged = !tag.is_empty() && tag.iter().all(u8::is_ascii_lowercase);
            let skip_worktree = tag == b"S" || tag == b"s";
            if assume_unchanged
                || (skip_worktree && may_exist(&path.join(Path::new(OsStr::from_bytes(file)))))
            {
                hidden.push(quote_path(file));
            }
        }
        if hidden.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "{} tracked file(s) hidden from git status (assume-unchanged or skip-worktree): {}",
            hidden.len(),
            examples(hidden.iter().map(String::as_str))
        )))
    }

    /// Why removing the work tree at `path` would delete a git repository in it that the
    /// checks before do not see: a work tree with a `.git` in a tracked directory (`git
    /// status` shows none there; an untracked one keeps the worktree dirty, and git refuses to
    /// remove one with submodules), as the walk of its directory finds it
    /// ([`repositories_in`], within [`LocalGit::search_max`] entries), its ignored directories
    /// `ignored` (as `git status` lists them, all disposable by then) not searched; or that
    /// it could not be searched to the end. A bare repository there is tracked and unchanged
    /// (anything else of it keeps the worktree already): nothing of it is lost.
    async fn repositories_kept(
        &self,
        path: &Path,
        ignored: &[String],
    ) -> anyhow::Result<Option<String>> {
        let skip: HashSet<Vec<u8>> = ignored
            .iter()
            .filter_map(|p| p.strip_suffix('/'))
            .map(unquote_path)
            .collect();
        let root = path.to_path_buf();
        let mut budget = self.search_max;
        let (found, cut) = tokio::task::spawn_blocking(move || {
            repositories_in(&root, &[], &|d| skip.contains(d), &mut budget)
        })
        .await
        .context("looking for repositories in the worktree")?;
        let repos: Vec<String> = found
            .iter()
            .filter(|(_, kind)| *kind == Found::WorkTree)
            .map(|(d, _)| format!("{}/", quote_path(d)))
            .collect();
        if !repos.is_empty() {
            return Ok(Some(format!(
                "{} git repositor{} in it that git status does not show (removing the worktree \
                 would delete {}): {}",
                repos.len(),
                if repos.len() == 1 { "y" } else { "ies" },
                if repos.len() == 1 { "it" } else { "them" },
                examples(repos.iter().map(String::as_str))
            )));
        }
        if !cut.is_empty() {
            let cut: Vec<String> = cut
                .iter()
                .map(|d| match d.as_slice() {
                    [] => "./".to_string(),
                    d => format!("{}/", quote_path(d)),
                })
                .collect();
            return Ok(Some(format!(
                "could not search {} to the end for git repositories (too many files, or \
                 unreadable)",
                examples(cut.iter().map(String::as_str))
            )));
        }
        Ok(None)
    }

    /// A commit that only the work tree's own record remembers — what removing it deletes,
    /// read as a deletion reads it ([`LocalGit::own_roots`]: its detached tip, every id of its
    /// reflogs, its own refs) — and no branch, tag or remote-tracking branch reaches (fewer
    /// survivors than a deletion counts: never less is kept). Both ids of every retained
    /// reflog entry count: after a partial expiry the oldest entry's old id may be the only
    /// thing still reaching a commit (see [`reflog_ids`]); the tip counts even with an empty
    /// or disabled reflog. An unreadable or malformed `HEAD`, reflog or ref is an error (the
    /// worktree is kept).
    async fn unreachable_reflog_commit(&self, path: &Path) -> anyhow::Result<Option<String>> {
        let ids = self.own_roots(path).await?;
        self.unreachable(path, ids).await
    }

    /// What the work tree at `path` keeps alive of its own: for a linked worktree, the roots
    /// of its administrative record ([`record_roots`]: its `HEAD`, reflogs and own refs, which
    /// removing it deletes); for a main work tree, its `HEAD` and `HEAD` reflog
    /// ([`LocalGit::head_ids_of`]).
    async fn own_roots(&self, path: &Path) -> anyhow::Result<String> {
        let out = self
            .git(
                path,
                ["rev-parse", "--absolute-git-dir", "--git-common-dir"],
            )
            .await?
            .into_stdout_bytes("git rev-parse --absolute-git-dir")?;
        let mut lines = out.split(|b| *b == b'\n');
        let git_dir = path_of(lines.next().unwrap_or_default());
        let common = path.join(path_of(trim_newline(lines.next().unwrap_or_default())));
        match (resolve_lenient(&git_dir), resolve_lenient(&common)) {
            (Some(g), Some(c)) if g != c => record_roots(&git_dir).map_err(|e| anyhow!("{e}")),
            (Some(_), Some(_)) => self.head_ids_of(path).await,
            _ => bail!(
                "cannot tell whether {} is a linked worktree",
                path.display()
            ),
        }
    }

    /// [`head_ids`] of the work tree at `path`: its own `HEAD` and `HEAD` reflog.
    async fn head_ids_of(&self, path: &Path) -> anyhow::Result<String> {
        let git_path = |what: &'static str| async move {
            let out = self
                .git(path, ["rev-parse", "--git-path", what])
                .await?
                .into_stdout_bytes("git rev-parse --git-path")?;
            anyhow::Ok(path.join(path_of(trim_newline(&out))))
        };
        let admin = git_path("HEAD").await?;
        let log = git_path("logs/HEAD").await?;
        head_ids(&admin, &log).map_err(|e| anyhow!("{e}"))
    }

    /// The first of `commits` (one per line) that no branch, tag or remote-tracking branch
    /// of the repository at `dir` reaches.
    async fn unreachable(&self, dir: &Path, commits: String) -> anyhow::Result<Option<String>> {
        if commits.trim().is_empty() {
            return Ok(None);
        }
        let args: Vec<OsString> = [
            "rev-list",
            "--stdin",
            "-n1",
            "--not",
            "--branches",
            "--tags",
            "--remotes",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        let out = self
            .run(
                dir,
                args,
                Mode::Read(self.timeouts.read),
                Some(commits.into_bytes()),
            )
            .await?
            .into_stdout("git rev-list")?;
        Ok(out
            .lines()
            .next()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_string))
    }

    /// Clears stale worktree records of `repo` (the worktree's directory is gone) that `which`
    /// selects: their administrative directory (`<common dir>/worktrees/<id>`) is deleted, as
    /// `git worktree prune` would, but only for those. A locked record, or one whose `HEAD`
    /// (a detached tip), reflogs or own refs ([`record_roots`]) hold commits no branch, tag or
    /// remote-tracking branch reaches or cannot be read, is left alone. So is one whose worktree
    /// cannot be told apart from gone (an error other than "not found": permissions, ...):
    /// the others are still considered, then that is an error. Returns how many went.
    async fn clear_stale_records(&self, repo: &Path, which: Stale<'_>) -> anyhow::Result<usize> {
        let common = self
            .git(repo, ["rev-parse", "--git-common-dir"])
            .await?
            .into_stdout_bytes("git rev-parse --git-common-dir")?;
        let common = repo.join(path_of(trim_newline(&common)));
        let admin_root = common.join("worktrees");
        let entries = match std::fs::read_dir(&admin_root) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).with_context(|| format!("reading {}", admin_root.display())),
        };
        let mut cleared = 0;
        let mut undecided: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            let admin = entry.path();
            // Raw bytes: the path need not be UTF-8.
            let Ok(gitdir) = std::fs::read(admin.join("gitdir")) else {
                continue;
            };
            let gitdir = path_of(trim_newline(&gitdir));
            // `worktree.useRelativePaths`: relative to the administrative directory.
            let gitdir = if gitdir.is_absolute() {
                gitdir
            } else {
                admin.join(gitdir)
            };
            let Some(wt) = gitdir.parent().map(Path::to_path_buf) else {
                continue;
            };
            // Which directory a record names is judged resolved (`alias/..` is the parent of
            // alias's target). One that cannot be resolved is never cleared; when it may be
            // one of those selected (as written), that is an error.
            let as_written = match which {
                Stale::Path(p) => wt == p,
                Stale::Under(root) => absolute(&wt).starts_with(absolute(root)),
            };
            let selected = match which {
                Stale::Path(p) if wt == p => Some(true),
                Stale::Path(p) => match (resolve_lenient(&wt), resolve_lenient(p)) {
                    (Some(a), Some(b)) => Some(a == b),
                    _ => None,
                },
                Stale::Under(root) => match (resolve_lenient(&wt), resolve_lenient(root)) {
                    (Some(wt), Some(root)) => Some(wt.starts_with(root)),
                    _ => None,
                },
            };
            match selected {
                Some(true) => {}
                Some(false) => continue,
                None if !as_written => continue,
                None => {
                    tracing::warn!(worktree = %wt.display(), "leaving a worktree record: cannot tell which directory it names");
                    undecided.push(format!(
                        "{}: cannot tell which directory it names",
                        wt.display()
                    ));
                    continue;
                }
            }
            // Only a stale record: its worktree (and the `.git` file in it) is known to be
            // gone. Any doubt keeps it.
            match (presence(&gitdir), presence(&wt)) {
                (Presence::Absent, Presence::Absent) => {}
                (Presence::Present, _) | (_, Presence::Present) => continue,
                (Presence::Unknown(e), _) | (_, Presence::Unknown(e)) => {
                    tracing::warn!(worktree = %wt.display(), error = %e, "leaving a worktree record: cannot tell whether its worktree is gone");
                    undecided.push(format!("{}: {e}", wt.display()));
                    continue;
                }
            }
            match presence(&admin.join("locked")) {
                Presence::Absent => {}
                Presence::Present => {
                    tracing::warn!(worktree = %wt.display(), "leaving a locked stale worktree record");
                    continue;
                }
                Presence::Unknown(e) => {
                    tracing::warn!(worktree = %wt.display(), error = %e, "leaving a stale worktree record whose lock cannot be checked");
                    undecided.push(format!("{} (lock): {e}", wt.display()));
                    continue;
                }
            }
            // What the record keeps alive goes with it ([`record_roots`]: its HEAD, its
            // reflogs and its own refs, as a deletion counts them). Unreadable or unparsable
            // for any reason: kept.
            let commits = match record_roots(&admin) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(worktree = %wt.display(), error = %e, "leaving a stale worktree record whose HEAD, reflogs or refs cannot be read");
                    continue;
                }
            };
            if let Some(c) = self.unreachable(repo, commits).await? {
                tracing::warn!(worktree = %wt.display(), commit = %c, "leaving a stale worktree record whose HEAD, reflogs or refs hold commits nothing else reaches");
                continue;
            }
            std::fs::remove_dir_all(&admin)
                .with_context(|| format!("removing the stale record {}", admin.display()))?;
            tracing::info!(worktree = %wt.display(), "cleared a stale worktree record");
            cleared += 1;
        }
        if !undecided.is_empty() {
            bail!(
                "cannot tell whether {} worktree(s) are gone, their records are kept: {}",
                undecided.len(),
                examples(undecided.iter().map(String::as_str))
            );
        }
        Ok(cleared)
    }
}

/// The commits a worktree's own `HEAD` keeps alive, one per line for `rev-list --stdin`: the
/// detached tip read from the raw `head` file (see [`head_tip`]) and every id of the raw
/// `log` reflog (see [`reflog_ids`]). A reflog proven absent is empty; `HEAD` must be read.
/// Any other read error, or anything malformed, is an error (the caller keeps the worktree).
fn head_ids(head: &Path, log: &Path) -> Result<String, String> {
    let raw = std::fs::read(head).map_err(|e| format!("reading {}: {e}", head.display()))?;
    let tip = head_tip(&raw).map_err(|e| format!("{}: {e}", head.display()))?;
    // Raw bytes: reflog messages need not be UTF-8 (a text read would fail and look like an
    // empty reflog).
    let log_bytes = match std::fs::read(log) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("reading the reflog {}: {e}", log.display())),
    };
    let mut ids =
        reflog_ids(&log_bytes).map_err(|e| format!("the reflog {}: {e}", log.display()))?;
    if let Some(tip) = tip {
        ids.push_str(&tip);
        ids.push('\n');
    }
    Ok(ids)
}

/// The commit a worktree's `HEAD` file (raw bytes) holds when detached; `None` when it is a
/// symbolic ref to a branch (the branch keeps its commit, or it is unborn). Anything else —
/// another symbolic ref, the reftable placeholder `refs/heads/.invalid`, the null id,
/// garbage — is an error: what it keeps alive cannot be told.
fn head_tip(head: &[u8]) -> Result<Option<String>, String> {
    let head = trim_newline(head);
    if let Some(target) = head.strip_prefix(b"ref: ") {
        return match target.strip_prefix(b"refs/heads/") {
            Some(name) if !name.is_empty() && name != b".invalid" => Ok(None),
            _ => Err(format!(
                "HEAD is a symbolic ref to {:?}, not a branch",
                String::from_utf8_lossy(target)
            )),
        };
    }
    if object_id(head) && !head.iter().all(|b| *b == b'0') {
        // ASCII hex, checked above.
        return Ok(Some(String::from_utf8_lossy(head).into_owned()));
    }
    Err("HEAD holds neither a branch nor a commit id".into())
}

/// A full hexadecimal object id (SHA-1 or SHA-256).
fn object_id(id: &[u8]) -> bool {
    matches!(id.len(), 40 | 64) && id.iter().all(u8::is_ascii_hexdigit)
}

/// The object ids a `HEAD` reflog (the raw bytes of `logs/HEAD`) holds, one per line for
/// `rev-list --stdin`: the old and the new id of every entry
/// (`<old> <new> <ident>\t<message>`), the null id left out. Git counts both as reachable
/// from the reflog: once older entries are expired, a commit may be named only as the old id
/// of the oldest entry left. A line that does not start with two object ids is an error (the
/// caller keeps what the reflog protects).
fn reflog_ids(log: &[u8]) -> Result<String, String> {
    let mut ids = String::new();
    for (n, line) in log.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(3, |b| *b == b' ');
        let (Some(old), Some(new), Some(_)) = (fields.next(), fields.next(), fields.next()) else {
            return Err(format!("line {} is not a reflog entry", n + 1));
        };
        for id in [old, new] {
            if !object_id(id) {
                return Err(format!("line {} holds no object id", n + 1));
            }
            if id.iter().all(|b| *b == b'0') {
                continue;
            }
            // ASCII hex, checked above.
            ids.push_str(std::str::from_utf8(id).unwrap_or_default());
            ids.push('\n');
        }
    }
    Ok(ids)
}

/// The ids of the reflog `log` ([`reflog_ids`]); empty when it is proven absent. Any other
/// read error, or anything malformed, is an error.
fn read_reflog(log: &Path) -> Result<String, String> {
    // Raw bytes: reflog messages need not be UTF-8.
    let bytes = match std::fs::read(log) {
        Ok(b) => b,
        Err(e) if is_absent(&e) => return Ok(String::new()),
        Err(e) => return Err(format!("reading the reflog {}: {e}", log.display())),
    };
    reflog_ids(&bytes).map_err(|e| format!("the reflog {}: {e}", log.display()))
}

/// Every file under the directory `dir`, at any depth; none when `dir` is proven absent.
/// Symlinks are not followed (git writes none there).
fn files_under(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if d == dir && is_absent(&e) => continue,
            Err(e) => return Err(format!("reading {}: {e}", d.display())),
        };
        for entry in entries {
            let entry = entry.map_err(|e| format!("reading {}: {e}", d.display()))?;
            let kind = entry
                .file_type()
                .map_err(|e| format!("reading {}: {e}", entry.path().display()))?;
            if kind.is_dir() {
                todo.push(entry.path());
            } else {
                out.push(entry.path());
            }
        }
    }
    Ok(out)
}

/// A linked worktree's administrative record (`<common dir>/worktrees/<id>`).
struct Record {
    admin: PathBuf,
    /// The worktree it names (its `gitdir` file, less `/.git`), or why that cannot be read.
    worktree: Result<PathBuf, String>,
}

/// The worktree records of the repository whose git common directory is `common`.
fn worktree_records(common: &Path) -> anyhow::Result<Vec<Record>> {
    let root = common.join("worktrees");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if is_absent(&e) => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("reading {}", root.display()))?;
        let admin = entry.path();
        if !admin.is_dir() {
            continue;
        }
        let file = admin.join("gitdir");
        let worktree = std::fs::read(&file)
            .map_err(|e| format!("reading {}: {e}", file.display()))
            .and_then(|raw| {
                let gitdir = path_of(trim_newline(&raw));
                // `worktree.useRelativePaths`: relative to the record.
                let gitdir = if gitdir.is_absolute() {
                    gitdir
                } else {
                    admin.join(gitdir)
                };
                // Git writes `<worktree>/.git`; anything else names no worktree for sure.
                gitdir
                    .parent()
                    .filter(|_| gitdir.file_name() == Some(OsStr::new(".git")))
                    .map(Path::to_path_buf)
                    .ok_or_else(|| format!("{} names no worktree's .git", file.display()))
            });
        out.push(Record { admin, worktree });
    }
    Ok(out)
}

/// What the `.git` of the directory `path` is, looked up without following a symlink.
pub(super) enum DotGit {
    /// There is none (or `path` itself is gone).
    Absent,
    /// A file naming a git directory (`gitdir: <dir>`, relative to `path` with
    /// `worktree.useRelativePaths`), joined to `path` as written.
    Names(PathBuf),
    /// Anything else (a directory, a symlink, a file that cannot be read or names nothing):
    /// what it is.
    Other(String),
}

pub(super) fn dot_git(path: &Path) -> DotGit {
    let file = path.join(".git");
    match std::fs::symlink_metadata(&file) {
        Err(e) if is_absent(&e) => DotGit::Absent,
        Err(e) => DotGit::Other(format!("cannot be looked up ({e})")),
        Ok(m) if m.file_type().is_symlink() => DotGit::Other("is a symlink".into()),
        Ok(m) if !m.is_file() => DotGit::Other("is no file".into()),
        Ok(_) => match std::fs::read(&file) {
            Err(e) => DotGit::Other(format!("cannot be read ({e})")),
            Ok(raw) => match trim_newline(&raw).strip_prefix(b"gitdir: ") {
                Some(g) => DotGit::Names(path.join(path_of(g))),
                None => DotGit::Other("names no git directory".into()),
            },
        },
    }
}

/// Which of `records` (those of the repository whose git common directory is `common`) is the
/// one of the linked worktree at `path`, which a deletion moves or clears with it and nothing
/// else. `None`: it has none. An error, and nothing may be touched, when the
/// pointers disagree or cannot be told apart. Proven both ways:
///
/// - `path` is no symlink (`lstat`): a link to another worktree would make that one's
///   record look like its own;
/// - `path`'s `.git` is a file ([`dot_git`]) naming a record directly under the resolved
///   `<common>/worktrees/`, and that record's `gitdir` names `<path>/.git` back (judged
///   resolved), and no other record names `path`. A `.git` naming another worktree's record
///   (a copy of that checkout, or that checkout moved there) is refused;
/// - when `path` has no `.git` (its directory is gone, or what is left of it), the one record
///   whose `gitdir` names `path`; a `.git` that is anything else while a record names `path`
///   is refused;
/// - with the thread's `branch` known, a record whose `HEAD` is on another branch is the
///   thread's own worktree switched to that branch ([`switched_own`]) unless it is another
///   thread's checkout: a record git did not make for `path` (named otherwise: a checkout
///   moved there), or one on another thread's recorded branch (`others`, the hub's; `None`:
///   any other `twr/...` branch). A detached `HEAD` is the thread's own.
///
/// A record whose `gitdir` cannot be read or resolved is an error when no record is proven
/// the worktree's.
fn record_of(
    common: &Path,
    records: &[Record],
    path: &Path,
    branch: Option<&str>,
    others: Option<&[String]>,
) -> Result<Option<usize>, String> {
    let gone = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(format!(
                "{} is a symlink, not the worktree's directory",
                path.display()
            ))
        }
        Ok(_) => false,
        Err(e) if is_absent(&e) => true,
        Err(e) => {
            return Err(format!(
                "cannot tell whether {} exists: {e}",
                path.display()
            ))
        }
    };
    // The records whose `gitdir` names `path`'s `.git`.
    let want = resolve_lenient(&path.join(".git"));
    let mut naming: Vec<usize> = Vec::new();
    let mut undecided = None;
    for (k, r) in records.iter().enumerate() {
        match &r.worktree {
            Ok(wt) if wt == path => naming.push(k),
            Ok(wt) => match (resolve_lenient(&wt.join(".git")), &want) {
                (Some(a), Some(b)) if a == *b => naming.push(k),
                (Some(_), Some(_)) => {}
                _ => {
                    undecided = Some(format!(
                        "cannot tell whether the worktree record {} is its",
                        r.admin.display()
                    ))
                }
            },
            Err(e) => {
                undecided = Some(format!(
                    "cannot tell whether the worktree record {} is its ({e})",
                    r.admin.display()
                ))
            }
        }
    }
    let names = |ks: &[usize]| {
        ks.iter()
            .map(|k| records[*k].admin.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let none = |undecided: Option<String>| match undecided {
        Some(why) => Err(why),
        None => Ok(None),
    };
    let k = match if gone { DotGit::Absent } else { dot_git(path) } {
        DotGit::Names(named) => {
            let resolved = resolve_lenient(&named).ok_or_else(|| {
                format!("cannot resolve {}, which its .git names", named.display())
            })?;
            let root = common.join("worktrees");
            let root = resolve_lenient(&root)
                .ok_or_else(|| format!("cannot resolve {}", root.display()))?;
            let k = if resolved.parent() == Some(root.as_path()) {
                records
                    .iter()
                    .position(|r| r.admin.file_name() == resolved.file_name())
            } else {
                None
            };
            match k {
                Some(k) if naming == [k] => k,
                Some(k) if naming.contains(&k) => {
                    return Err(format!(
                        "the worktree records {} all name {}",
                        names(&naming),
                        path.display()
                    ))
                }
                Some(k) => {
                    let r = &records[k];
                    return Err(match &r.worktree {
                        Ok(wt) => format!(
                            "{} now holds another checkout (its .git names the worktree record \
                             {}, which is {}'s)",
                            path.display(),
                            r.admin.display(),
                            wt.display()
                        ),
                        Err(e) => format!(
                            "its .git names the worktree record {}, which does not name it \
                             back ({e})",
                            r.admin.display()
                        ),
                    });
                }
                None if naming.is_empty() => return none(undecided),
                None => {
                    return Err(format!(
                        "{} holds another checkout (its .git names {}, while the worktree \
                         record {} names it)",
                        path.display(),
                        named.display(),
                        names(&naming)
                    ))
                }
            }
        }
        DotGit::Absent => match naming.as_slice() {
            [] => return none(undecided),
            [k] => *k,
            _ => {
                return Err(format!(
                    "the worktree records {} all name {}",
                    names(&naming),
                    path.display()
                ))
            }
        },
        DotGit::Other(_) if naming.is_empty() => return none(undecided),
        DotGit::Other(what) => {
            return Err(format!(
                "the worktree record {} names {}, but its .git {what}",
                names(&naming),
                path.display()
            ))
        }
    };
    if let Some(b) = branch {
        let admin = &records[k].admin;
        if let Some(on) = record_branch(admin)? {
            if on != b.as_bytes() {
                switched_own(admin, path, b, &on, others)?;
            }
        }
    }
    Ok(Some(k))
}

/// Whether the worktree of record `admin` was on `branch` and left it ([`switched_own`]): its
/// `HEAD` reflog has a `checkout: moving from <branch> to ...` entry, or a restore of the
/// thread made it on another branch (its mark names `branch`).
fn left_branch(admin: &Path, branch: &str) -> bool {
    let from = format!("checkout: moving from {branch} to ");
    let logged = std::fs::read(admin.join("logs").join("HEAD")).is_ok_and(|log| {
        log.split(|b| *b == b'\n').any(|line| {
            line.splitn(2, |b| *b == b'\t')
                .nth(1)
                .is_some_and(|msg| msg.starts_with(from.as_bytes()))
        })
    });
    logged || trash::read_mark(admin).is_some_and(|m| m.branch == branch)
}

/// The branch the `HEAD` of the worktree record `admin` is on (`ref: refs/heads/<branch>`,
/// as bytes: a ref name need not be UTF-8); `None` when it is detached.
fn record_branch(admin: &Path) -> Result<Option<Vec<u8>>, String> {
    let head = admin.join("HEAD");
    let raw = std::fs::read(&head).map_err(|e| format!("reading {}: {e}", head.display()))?;
    Ok(trim_newline(&raw)
        .strip_prefix(b"ref: refs/heads/")
        .map(<[u8]>::to_vec))
}

/// An error unless the worktree record `admin`, proven the one of `path` both ways
/// ([`record_of`]), whose `HEAD` is on the branch `on` and not on the thread's own `branch`,
/// is the thread's own worktree switched to `on` (as threads that integrate their own work
/// do):
///
/// - git made the record for `path`: `git worktree add` names it after the path's last
///   component (with a number after it when that name is taken), and a checkout moved there
///   (`git worktree move`) keeps the name it was made with;
/// - it was on `branch` and left it: git's own `HEAD` reflog of the record has a `checkout:
///   moving from <branch> to ...` entry, or a restore of the thread made it ([`trash::Mark`]
///   naming `branch`), which a checkout made elsewhere on another branch never has (a
///   reflog entry expired since, `gc.reflogExpire`, refuses: switched back, it goes);
/// - `on` is no other thread's recorded branch: one of `others` (the hub's list of the box's
///   other threads), or, when the hub sent none (an older hub), any other `twr/...` branch.
///
/// Its worktree then goes like any other, `on` staying in the repository (it is not the
/// thread's recorded branch), and a park keeps it ([`RemoveOutcome::KeptCheckout`]).
fn switched_own(
    admin: &Path,
    path: &Path,
    branch: &str,
    on: &[u8],
    others: Option<&[String]>,
) -> Result<(), String> {
    let name = admin
        .file_name()
        .map(OsStrExt::as_bytes)
        .unwrap_or_default();
    let base = path.file_name().map(OsStrExt::as_bytes).unwrap_or_default();
    let made_here = !base.is_empty()
        && name
            .strip_prefix(base)
            .is_some_and(|n| n.iter().all(u8::is_ascii_digit));
    if !made_here {
        return Err(format!(
            "{} holds another checkout (its worktree record {} is on the branch {}, not on \
             {branch}, and was made for another path: git names a record after the path it \
             makes it for)",
            path.display(),
            admin.display(),
            String::from_utf8_lossy(on)
        ));
    }
    if !left_branch(admin, branch) {
        return Err(format!(
            "{} holds another checkout (its worktree record {} is on the branch {}, not on \
             {branch}, and git has no record of it leaving {branch}: no `checkout: moving \
             from {branch}` in its HEAD reflog)",
            path.display(),
            admin.display(),
            String::from_utf8_lossy(on)
        ));
    }
    let another = match others {
        Some(others) => others.iter().any(|o| o.as_bytes() == on),
        None => on.starts_with(BRANCH_PREFIX.as_bytes()),
    };
    if another {
        return Err(format!(
            "{} holds another thread's checkout (its worktree record {} is on the branch {}, \
             {}, not on {branch})",
            path.display(),
            admin.display(),
            String::from_utf8_lossy(on),
            if others.is_some() {
                "another thread's branch"
            } else {
                "one of twr's"
            }
        ));
    }
    Ok(())
}

/// The commits a worktree record (its administrative directory `admin`) keeps alive, one per
/// line: its detached `HEAD` ([`head_tip`]), every id of its reflogs (`logs/HEAD`, and those
/// of its own refs) and its own refs (`refs/bisect/...`, `refs/worktree/...`, always loose).
/// Anything that cannot be read or parsed is an error.
fn record_roots(admin: &Path) -> Result<String, String> {
    let head = admin.join("HEAD");
    let raw = read_small_file(&head).map_err(|e| format!("reading {}: {e}", head.display()))?;
    let mut ids = String::new();
    if let Some(tip) = head_tip(&raw).map_err(|e| format!("{}: {e}", head.display()))? {
        ids.push_str(&tip);
        ids.push('\n');
    }
    for log in files_under(&admin.join("logs"))? {
        ids.push_str(&read_reflog(&log)?);
    }
    for r in files_under(&admin.join("refs"))? {
        let raw = read_small_file(&r).map_err(|e| format!("reading {}: {e}", r.display()))?;
        let id = trim_newline(&raw);
        if id.starts_with(b"ref: ") {
            continue;
        }
        if !object_id(id) {
            return Err(format!("{} holds no object id", r.display()));
        }
        // ASCII hex, checked above.
        ids.push_str(std::str::from_utf8(id).unwrap_or_default());
        ids.push('\n');
    }
    Ok(ids)
}

/// The file at `path`, one line by nature (a worktree record's `HEAD` or one of its refs), read
/// through its directory opened ([`dirfd::Dir::read_small`]): a regular file (not a symlink,
/// no FIFO waited on) of at most 64 KiB. The worktree's agent may write its record (Codex
/// `auto`), and a file there is never read whole however large.
fn read_small_file(path: &Path) -> std::io::Result<Vec<u8>> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other(format!(
            "{} names no file",
            path.display()
        )));
    };
    dirfd::Dir::open(dir)?.read_small(name, 1 << 16)
}

/// Which stale records [`LocalGit::clear_stale_records`] clears.
#[derive(Debug, Clone, Copy)]
enum Stale<'a> {
    /// The one of a worktree at this path.
    Path(&'a Path),
    /// Those of worktrees under this directory.
    Under(&'a Path),
}

/// `bytes` without its trailing line break(s).
fn trim_newline(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    while end > 0 && matches!(bytes[end - 1], b'\n' | b'\r') {
        end -= 1;
    }
    &bytes[..end]
}

/// The path git printed as raw bytes (not necessarily UTF-8).
fn path_of(bytes: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

/// What looking a path up (without following a final symlink) established. Anything that
/// deletes on absence acts only on [`Presence::Absent`] (never `Path::exists`, which reads
/// every error as absence).
#[derive(Debug)]
enum Presence {
    Present,
    /// Not found, or a parent is not a directory.
    Absent,
    /// Any other error (an ancestor this user cannot search, I/O, ...): it may exist.
    Unknown(std::io::Error),
}

fn presence(p: &Path) -> Presence {
    match std::fs::symlink_metadata(p) {
        Ok(_) => Presence::Present,
        Err(e) if is_absent(&e) => Presence::Absent,
        Err(e) => Presence::Unknown(e),
    }
}

/// Whether lookup error `e` proves the path absent (not found, or a parent is not a
/// directory); any other error leaves it undecided.
fn is_absent(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `p` may exist: false only when it is known to be absent ([`Presence::Absent`]), so
/// a removal keeps what it cannot check.
fn may_exist(p: &Path) -> bool {
    !matches!(presence(p), Presence::Absent)
}

/// How long `df` may take before the free space is given up as unknown.
const DF_TIMEOUT: Duration = Duration::from_secs(10);

/// `(free, total)` bytes of the file system holding `dir`, as `df -Pk` says it (free: what an
/// ordinary user may still write, its "Available"); `None` when `df` fails or takes longer
/// than [`DF_TIMEOUT`] (it is killed).
async fn disk_free(dir: &Path) -> Option<(Option<u64>, Option<u64>)> {
    let mut cmd = tokio::process::Command::new("df");
    cmd.arg("-Pk")
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    // No terminal: whatever `df` on `PATH` is.
    twr_adapters::process::new_session(&mut cmd);
    let out = tokio::time::timeout(DF_TIMEOUT, cmd.output())
        .await
        .ok()?
        .ok()?;
    out.status
        .success()
        .then(|| parse_df(&String::from_utf8_lossy(&out.stdout)))
}

/// `(free, total)` bytes from `df -Pk` output: its second line's `1024-blocks` and `Available`,
/// found back from the `Capacity` field (`37%`), since the file system's name or the mount
/// point may hold spaces.
fn parse_df(text: &str) -> (Option<u64>, Option<u64>) {
    let Some(line) = text.lines().nth(1) else {
        return (None, None);
    };
    let fields: Vec<&str> = line.split_whitespace().collect();
    let Some(cap) = fields.iter().position(|f| f.ends_with('%')) else {
        return (None, None);
    };
    // GNU df can say a negative Available (the root-reserved blocks in use): none free.
    let kb = |i: Option<usize>| {
        i.and_then(|i| fields.get(i))
            .and_then(|f| f.parse::<i64>().ok())
            .map(|k| u64::try_from(k).unwrap_or(0).saturating_mul(1024))
    };
    (kb(cap.checked_sub(1)), kb(cap.checked_sub(3)))
}

/// `p` made absolute (`..` kept, symlinks not resolved).
fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Whether ignored entry `path` (as [`quote_path`] writes it; a directory ends with `/`) is
/// regenerable per `list` (see [`DEFAULT_PARK_DISPOSABLE`]): a component matches an entry when
/// its raw name is the entry's.
fn disposable(path: &str, list: &[String]) -> bool {
    any_component(path, |c, dir| names_component(list, c, dir))
}

/// Whether ignored entry `path` is regenerable per `list` and per `also` through the same
/// component ([`disposable`] for each): an idle trim limited to `also` deletes what is under a
/// directory both name, never a `build/` that `list` names inside a `node_modules/` that only
/// `also` names.
fn disposable_in_both(path: &str, list: &[String], also: &[String]) -> bool {
    any_component(path, |c, dir| {
        names_component(list, c, dir) && names_component(also, c, dir)
    })
}

/// Whether `matches` holds for a component of `path` (as [`quote_path`] writes it; a
/// directory ends with `/`): its raw name, and whether it is a directory.
fn any_component(path: &str, matches: impl Fn(&[u8], bool) -> bool) -> bool {
    let is_dir = path.ends_with('/');
    let comps: Vec<Vec<u8>> = path
        .trim_end_matches('/')
        .split('/')
        .map(unquote_path)
        .collect();
    let last = comps.len().saturating_sub(1);
    comps
        .iter()
        .enumerate()
        .any(|(i, c)| matches(c, i < last || is_dir))
}

/// Whether an entry of `list` names component `c` (a directory with `dir`): `name/` a
/// directory, `name` anything.
fn names_component(list: &[String], c: &[u8], dir: bool) -> bool {
    list.iter().any(|e| match e.strip_suffix('/') {
        Some(d) => dir && d.as_bytes() == c,
        None => e.as_bytes() == c,
    })
}

/// The first few of `items`, for a message.
fn examples<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.collect();
    let shown: Vec<&str> = items.iter().take(3).copied().collect();
    if items.len() > shown.len() {
        format!("{}, ...", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// Commit ids.
type Commits = HashSet<String>;

/// Of one target of a deletion set ([`LocalGit::set_losses`]): what of its branch (when it
/// exists) and of its worktree's record (when it has one), or why that cannot be told.
type Parts<T> = Result<(Option<T>, Option<T>), String>;

/// What [`LocalGit::tree_files`] found in one work tree.
struct TreeFiles {
    dirty: Vec<String>,
    hidden: Vec<String>,
    ignored: Vec<String>,
    gitlinks: Vec<Vec<u8>>,
}

/// Deepest repository (in a repository ...) of a work tree an inventory looks into.
const REPO_DEPTH_MAX: usize = 16;

/// Most entries a search of a worktree for repositories looks at, all its directories
/// together ([`LocalGit::search_max`]): past it, a directory is not searched to the end.
const NESTED_SEARCH_MAX: u64 = 200_000;

/// The entry of `repos` for the repository shown as `path` (`nested`: no submodule).
fn repo_entry<'a>(
    repos: &'a mut BTreeMap<String, RepoLoss>,
    path: &str,
    nested: bool,
) -> &'a mut RepoLoss {
    repos.entry(path.to_string()).or_insert_with(|| RepoLoss {
        path: path.to_string(),
        nested,
        ..Default::default()
    })
}

/// Notes in `repos` that something of the repository shown as `path` (`nested`: no
/// submodule) could not be checked (`why`).
fn note(repos: &mut BTreeMap<String, RepoLoss>, path: &str, nested: bool, why: String) {
    let r = repo_entry(repos, path, nested);
    r.unchecked = Some(match r.unchecked.take() {
        Some(before) => format!("{before}; {why}"),
        None => why,
    });
}

/// A repository found under a work tree: its raw path, and what kind it is.
type FoundAt = (Vec<u8>, Found);

/// What kind of repository a search of a work tree found ([`repositories_in`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// A work tree: a directory with a `.git` (a directory, or a file naming one).
    WorkTree,
    /// A bare repository: the directory is a git directory itself, or what is left of one
    /// ([`repository_at`]).
    Bare,
}

/// The repositories in the work tree whose top directory is `top` (a raw path under `root`;
/// empty: `root` itself), at any depth, by raw path under `root`: every directory below `top`
/// holding a `.git` ([`Found::WorkTree`]: a directory, or a file naming one) or that is a git
/// directory itself, or what is left of one ([`Found::Bare`], [`repository_at`]). Every
/// directory is looked at, tracked, untracked and ignored alike (`git status` shows no
/// repository inside a tracked directory, and lists an ignored directory as one entry), but
/// for those `skip` names (by raw path under `root`), `top`'s own `.git`, and a repository
/// found (its git directory is searched with the repositories that go,
/// [`repositories_under`], never its object store). Each entry looked at takes one of
/// `budget`. Also the directories not searched to the end, because the budget ran out or a
/// part of one could not be read: `top` itself when its own entries were not all read, else
/// the directory under `top` holding the part (its first component). Symlinks are not
/// followed.
fn repositories_in(
    root: &Path,
    top: &[u8],
    skip: &dyn Fn(&[u8]) -> bool,
    budget: &mut u64,
) -> (Vec<FoundAt>, Vec<Vec<u8>>) {
    let join = |d: &[u8], name: &[u8]| {
        if d.is_empty() {
            name.to_vec()
        } else {
            [d, b"/", name].concat()
        }
    };
    // The directory a part of `d` (`top` or under it) is reported by.
    let cut_at = |d: &[u8]| -> Vec<u8> {
        let rest = match d.strip_prefix(top) {
            Some(r) if !top.is_empty() => r.strip_prefix(b"/").unwrap_or(r),
            Some(r) => r,
            None => d,
        };
        match rest.split(|b| *b == b'/').next() {
            Some(first) if !first.is_empty() => join(top, first),
            _ => top.to_vec(),
        }
    };
    let mut found = Vec::new();
    let mut cut: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut todo = vec![top.to_vec()];
    'walk: while let Some(d) = todo.pop() {
        let full = root.join(path_of(&d));
        if d != top {
            match std::fs::symlink_metadata(full.join(".git")) {
                Ok(_) => {
                    found.push((d, Found::WorkTree));
                    continue;
                }
                Err(e) if is_absent(&e) => {}
                Err(_) => {
                    cut.insert(cut_at(&d));
                    continue;
                }
            }
            match repository_at(&full) {
                Ok(true) => {
                    found.push((d, Found::Bare));
                    continue;
                }
                Ok(false) => {}
                Err(()) => {
                    cut.insert(cut_at(&d));
                }
            }
        }
        let Ok(entries) = std::fs::read_dir(&full) else {
            cut.insert(cut_at(&d));
            continue;
        };
        for e in entries {
            if *budget == 0 {
                cut.insert(cut_at(&d));
                cut.extend(todo.iter().map(|t| cut_at(t)));
                break 'walk;
            }
            *budget -= 1;
            match e.and_then(|e| Ok((e.file_name(), e.file_type()?))) {
                Ok((name, t)) if t.is_dir() && name != ".git" => {
                    let sub = join(&d, name.as_bytes());
                    if !skip(&sub) {
                        todo.push(sub);
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    cut.insert(cut_at(&d));
                }
            }
        }
    }
    // `top` not searched to the end says it for everything in it.
    if cut.contains(top) {
        return (found, vec![top.to_vec()]);
    }
    (found, cut.into_iter().collect())
}

/// Whether the bare repository at `dir` (a raw path under a work tree) holds anything the
/// work tree's commits do not: one of `listed` (the work tree's changed, untracked, hidden
/// and ignored entries, raw paths under it) is in it, is it, or holds it (an ignored
/// directory). A git directory whose files are all tracked and unchanged (a test fixture)
/// loses nothing with the work tree.
fn holds_listed(dir: &[u8], listed: &[Vec<u8>]) -> bool {
    let under = |a: &[u8], b: &[u8]| a.starts_with(b) && a.get(b.len()) == Some(&b'/');
    listed
        .iter()
        .any(|e| e.as_slice() == dir || under(e, dir) || under(dir, e))
}

/// Whether the directory `dir` is a git directory ([`GitDir::Valid`]).
#[cfg(test)]
fn is_git_dir(dir: &Path) -> bool {
    matches!(git_dir_kind(dir), Ok(GitDir::Valid))
}

/// Whether the directory `dir` holds a repository whose objects go with it: a git directory
/// ([`GitDir::Valid`]), or what is left of one ([`GitDir::Damaged`]: git would not take it, its
/// commits cannot be checked, but they go all the same). `Err` when that cannot be told (a
/// part of it that cannot be read).
fn repository_at(dir: &Path) -> Result<bool, ()> {
    git_dir_kind(dir).map(|k| k != GitDir::No)
}

/// What a directory is to git ([`git_dir_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitDir {
    /// No git directory.
    No,
    /// A git directory, by the test git makes of one: a `HEAD` that names a ref (`ref:
    /// refs/...`, or a symlink into `refs/`) or holds an object id, and `objects/` and `refs/`
    /// directories (or symlinks, which git follows: a shared object store).
    Valid,
    /// An object store (`objects/`) with a `HEAD` or `refs/` that git would not take for a git
    /// directory (a `HEAD` that names nothing, `refs/` gone, ...).
    Damaged,
}

/// What the directory `dir` is to git, or `Err` when that cannot be told (a part of it that
/// cannot be read). Symlinks are not followed but for `HEAD`'s target: a symlinked `objects`
/// or `refs` counts as the directory git would take it for, wherever it leads.
fn git_dir_kind(dir: &Path) -> Result<GitDir, ()> {
    let is_dir = |p: &Path| match std::fs::symlink_metadata(p) {
        Ok(m) => Ok(m.is_dir() || m.file_type().is_symlink()),
        Err(e) if is_absent(&e) => Ok(false),
        Err(_) => Err(()),
    };
    if !is_dir(&dir.join("objects"))? {
        return Ok(GitDir::No);
    }
    let refs = is_dir(&dir.join("refs"))?;
    let head = dir.join("HEAD");
    let meta = match std::fs::symlink_metadata(&head) {
        Ok(m) => Some(m),
        Err(e) if is_absent(&e) => None,
        Err(_) => return Err(()),
    };
    let head_ok = match &meta {
        Some(m) if m.file_type().is_symlink() => std::fs::read_link(&head)
            .map_err(|_| ())?
            .starts_with("refs"),
        Some(m) if m.is_file() => {
            let raw = std::fs::read(&head).map_err(|_| ())?;
            let raw = trim_newline(&raw);
            match raw.strip_prefix(b"ref:") {
                Some(target) => target.trim_ascii_start().starts_with(b"refs/"),
                None => object_id(raw),
            }
        }
        _ => false,
    };
    Ok(match (head_ok && refs, meta.is_some() || refs) {
        (true, _) => GitDir::Valid,
        (false, true) => GitDir::Damaged,
        (false, false) => GitDir::No,
    })
}

/// The git directory of the repository whose work tree is `dir` (its `.git`: a directory, or a
/// file naming one); `None` when it is not checked out (no `.git`).
fn gitdir_of(dir: &Path) -> Result<Option<PathBuf>, String> {
    let dot = dir.join(".git");
    let meta = match std::fs::metadata(&dot) {
        Ok(m) => m,
        Err(e) if is_absent(&e) => return Ok(None),
        Err(e) => return Err(format!("reading {}: {e}", dot.display())),
    };
    if meta.is_dir() {
        return Ok(Some(dot));
    }
    let raw = std::fs::read(&dot).map_err(|e| format!("reading {}: {e}", dot.display()))?;
    match trim_newline(&raw).strip_prefix(b"gitdir: ") {
        // Relative to the submodule's work tree.
        Some(g) => Ok(Some(dir.join(path_of(g)))),
        None => Err(format!("{} names no git directory", dot.display())),
    }
}

/// The administrative record of the linked worktree checked out at `dir`, whose `.git` is a
/// file naming `gitdir`, when that record is outside every one of `doomed` (resolved paths:
/// what a deletion removes): a git directory with a `commondir` (a linked worktree's record)
/// whose `gitdir` names `dir` back, or cannot be read. Moving `dir` away would leave it naming
/// a missing checkout, which `git worktree prune` in its repository clears with its `HEAD`,
/// reflogs and index ([`LinkedWorktree`]). `None` for a record in `doomed`, a git directory
/// that is no linked worktree's record (no `commondir`; a separate git directory loses nothing
/// with its work tree), or a record that names another checkout (`dir` is a copy of one) or
/// none (`git worktree prune` clears it either way).
fn outside_record(dir: &Path, gitdir: &Path, doomed: &[PathBuf]) -> Option<PathBuf> {
    let record = resolve_lenient(gitdir).unwrap_or_else(|| absolute(gitdir));
    if doomed.iter().any(|d| record.starts_with(d)) {
        return None;
    }
    if matches!(presence(&record.join("commondir")), Presence::Absent) {
        return None;
    }
    let named = match std::fs::read(record.join("gitdir")) {
        Ok(raw) => path_of(trim_newline(&raw)),
        Err(e) if is_absent(&e) => return None,
        Err(_) => return Some(record),
    };
    // `worktree.useRelativePaths`: relative to the record.
    let named = if named.is_absolute() {
        named
    } else {
        record.join(named)
    };
    let back = matches!(
        (resolve_lenient(&named), resolve_lenient(&dir.join(".git"))),
        (Some(a), Some(b)) if a == b
    );
    back.then_some(record)
}

/// What the deletion of a work tree is refused for, about the linked worktree at `dir` whose
/// record is `record` ([`outside_record`]); `common`: the deleted work tree's repository (its
/// git common directory), when known.
fn linked_worktree(dir: &Path, record: &Path, common: Option<&Path>) -> LinkedWorktree {
    let shown = |p: &Path| quote_path(p.as_os_str().as_bytes());
    // Its repository: the record's `commondir`, relative to the record.
    let theirs = std::fs::read(record.join("commondir"))
        .ok()
        .map(|raw| record.join(path_of(trim_newline(&raw))))
        .map(|c| resolve_lenient(&c).unwrap_or(c));
    let repo = match &theirs {
        Some(c) if c.file_name() == Some(OsStr::new(".git")) => c
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| c.clone()),
        Some(c) => c.clone(),
        None => record.to_path_buf(),
    };
    let same_repository = match (&theirs, common.and_then(resolve_lenient)) {
        (Some(a), Some(b)) => *a == b,
        _ => false,
    };
    LinkedWorktree {
        path: shown(&absolute(dir)),
        record: shown(record),
        repo: shown(&repo),
        same_repository,
    }
}

/// The linked worktrees checked out in the work tree at `root`, at any depth (in the work
/// trees of repositories nested in it too, at most [`REPO_DEPTH_MAX`] deep), whose records
/// are outside `doomed` ([`outside_record`]), each with its record; within `budget` entries
/// ([`repositories_in`]), and whether the search was cut (too many entries, or a part that
/// could not be read).
fn linked_outside(
    root: &Path,
    doomed: &[PathBuf],
    budget: &mut u64,
) -> (Vec<(PathBuf, PathBuf)>, bool) {
    let mut out = Vec::new();
    let mut cut_short = false;
    let mut todo: Vec<(Vec<u8>, usize)> = vec![(Vec::new(), 0)];
    while let Some((top, depth)) = todo.pop() {
        let (found, cut) = repositories_in(root, &top, &|_| false, budget);
        cut_short |= !cut.is_empty();
        for (d, kind) in found {
            if kind != Found::WorkTree {
                continue;
            }
            let full = root.join(path_of(&d));
            match gitdir_of(&full) {
                Ok(Some(g)) if g != full.join(".git") => {
                    if let Some(record) = outside_record(&full, &g, doomed) {
                        out.push((full, record));
                    }
                }
                Ok(_) => {}
                Err(_) => cut_short = true,
            }
            if depth < REPO_DEPTH_MAX {
                todo.push((d, depth + 1));
            } else {
                cut_short = true;
            }
        }
    }
    out.sort();
    (out, cut_short)
}

/// The repositories under `root` (a git directory that goes: a worktree's administrative
/// record, or a repository's own git directory), at any depth, not counting `root` itself:
/// every directory that is a git directory or what is left of one ([`repository_at`]: a
/// submodule's repository under
/// `modules/<name>`, its name possibly with `/`, a bare repository put there, ...), looked
/// inside too (a submodule's repository keeps its own submodules' under its `modules/`), but
/// not into object stores (`objects/` of a git directory); none when `root` is proven absent.
/// And each directory that cannot be read, with why: what is in it goes unchecked. Symlinks
/// are not followed.
fn repositories_under(root: &Path) -> (Vec<PathBuf>, Vec<(PathBuf, String)>) {
    let (mut out, mut unread) = (Vec::new(), Vec::new());
    // Directories to read, and whether each is a git directory.
    let mut todo = vec![(root.to_path_buf(), true)];
    while let Some((d, git_dir)) = todo.pop() {
        let entries = match std::fs::read_dir(&d) {
            Ok(e) => e,
            Err(e) if d == root && is_absent(&e) => continue,
            Err(e) => {
                unread.push((d.clone(), format!("reading {}: {e}", d.display())));
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    unread.push((d.clone(), format!("reading {}: {e}", d.display())));
                    continue;
                }
            };
            let p = entry.path();
            match entry.file_type() {
                Ok(t) if !t.is_dir() => continue,
                Ok(_) => {}
                Err(e) => {
                    unread.push((p.clone(), format!("reading {}: {e}", p.display())));
                    continue;
                }
            }
            if git_dir && entry.file_name() == "objects" {
                continue;
            }
            match repository_at(&p) {
                Ok(true) => {
                    out.push(p.clone());
                    todo.push((p, true));
                }
                Ok(false) => todo.push((p, false)),
                Err(()) => unread.push((p.clone(), format!("cannot read {}", p.display()))),
            }
        }
    }
    (out, unread)
}

/// Most entries [`ignored_groups`] looks at, all directories together: counting stops there
/// (the counts say there are more).
const IGNORED_COUNT_MAX: u64 = 200_000;

/// Ignored entries `ignored` (as `git status --ignored=matching` lists them, under `root`,
/// each as [`quote_path`] writes it) by top-level entry, with how many files each holds,
/// single files first, then directories, each by path: the first [`IGNORED_GROUPS_MAX`], and
/// how many there are in all. A directory git lists is counted on disk (symlinks not
/// followed); one that cannot be read counts what it could, with [`IgnoredGroup::more`].
fn ignored_groups(root: &Path, ignored: &[String]) -> (Vec<IgnoredGroup>, u64) {
    let mut groups: std::collections::BTreeMap<(bool, String), Vec<&String>> =
        std::collections::BTreeMap::new();
    for p in ignored {
        let top = match p.find('/') {
            Some(i) => &p[..=i],
            None => p.as_str(),
        };
        groups
            .entry((top.ends_with('/'), top.to_string()))
            .or_default()
            .push(p);
    }
    let total = groups.len() as u64;
    let mut budget = IGNORED_COUNT_MAX;
    let out = groups
        .into_iter()
        .take(IGNORED_GROUPS_MAX)
        .map(|((_, path), entries)| {
            let mut g = IgnoredGroup {
                path,
                ..Default::default()
            };
            for e in entries {
                if !e.ends_with('/') {
                    g.files += 1;
                    continue;
                }
                let (n, all) = count_files(&root.join(path_of(&unquote_path(e))), &mut budget);
                g.files += n;
                g.more |= !all;
            }
            g
        })
        .collect();
    (out, total)
}

/// The files (anything but a directory) under `dir`, symlinks not followed, each one taking
/// one of `budget`; and whether that is all of them (false: the budget ran out, or a
/// directory could not be read).
fn count_files(dir: &Path, budget: &mut u64) -> (u64, bool) {
    let (mut n, mut all) = (0, true);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            all = false;
            continue;
        };
        for e in entries {
            if *budget == 0 {
                return (n, false);
            }
            *budget -= 1;
            match e.and_then(|e| Ok((e.path(), e.file_type()?))) {
                Ok((p, t)) if t.is_dir() => stack.push(p),
                Ok(_) => n += 1,
                Err(_) => all = false,
            }
        }
    }
    (n, all)
}

impl GitOps for LocalGit {
    fn repo_root(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<Option<PathBuf>>> {
        let g = self.clone();
        async move {
            let out = match g.git(&dir, ["rev-parse", "--show-toplevel"]).await {
                Ok(o) => o,
                Err(e) => {
                    // No git on this box: nothing is a repository.
                    let missing = e.chain().any(|c| {
                        c.downcast_ref::<std::io::Error>()
                            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                    });
                    if missing {
                        tracing::warn!(git = %g.bin.display(), "git is not installed; projects run without worktrees");
                        return Ok(None);
                    }
                    return Err(e);
                }
            };
            if out.ok {
                return Ok(Some(path_of(trim_newline(&out.stdout))));
            }
            if out.stderr.contains("not a git repository") {
                return Ok(None);
            }
            Err(out.error("git rev-parse --show-toplevel"))
        }
        .boxed()
    }

    fn git_common_dir(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<PathBuf>> {
        let g = self.clone();
        async move { g.common_dir(&dir).await }.boxed()
    }

    fn worktree_create(
        &self,
        spec: WorktreeSpec,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeCreated>> {
        self.supervised("creating the worktree", self.clone().create(spec))
            .boxed()
    }

    fn worktree_remove(
        &self,
        repo: PathBuf,
        path: PathBuf,
        branch: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<RemoveOutcome>> {
        self.supervised(
            "removing the worktree",
            self.clone().remove(repo, path, branch),
        )
        .boxed()
    }

    fn worktree_prune(&self, repo: PathBuf) -> BoxFuture<'static, anyhow::Result<()>> {
        let g = self.clone();
        self.supervised("clearing stale worktree records", async move {
            let Some(root) = g.managed.clone() else {
                tracing::debug!("no managed worktrees directory; nothing to prune");
                return Ok(());
            };
            g.clear_stale_records(&repo, Stale::Under(&root))
                .await
                .map(|_| ())
        })
        .boxed()
    }

    fn worktree_exists(&self, path: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>> {
        let workdirs = Arc::clone(&self.workdirs);
        async move {
            // A change of it still running (a removal whose caller gave up) decides first.
            workdirs
                .settled(&path, CHANGE_WAIT)
                .await
                .map_err(|e| anyhow!(e))?;
            // One whose files twr never finished checking out is none.
            if unfinished(&path) {
                return Ok(false);
            }
            match tokio::fs::metadata(&path).await {
                Ok(m) => Ok(m.is_dir()),
                Err(e) if is_absent(&e) => Ok(false),
                Err(e) => Err(anyhow!("{}: {e}", path.display())),
            }
        }
        .boxed()
    }

    fn snapshot(
        &self,
        dir: PathBuf,
        base: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<GitSnapshot>> {
        let g = self.clone();
        async move {
            let mut snap = g.status(&dir).await?;
            if let Some(base) = base {
                match g.readiness(&dir, &base).await {
                    Ok(r) => {
                        let count = r.conflicts.as_ref().map(Vec::len);
                        snap.base = Some(base);
                        snap.base_ahead = Some(r.ahead);
                        snap.base_behind = Some(r.behind);
                        snap.conflicts = r.conflicts.map(|mut c| {
                            c.truncate(CONFLICT_FILES_MAX);
                            c
                        });
                        snap.conflicted = r.conflicted;
                        snap.conflict_count = count.filter(|n| *n > CONFLICT_FILES_MAX).map(|n| n as u64);
                    }
                    Err(e) => {
                        tracing::debug!(dir = %dir.display(), %base, error = %format!("{e:#}"), "no merge readiness");
                    }
                }
            }
            Ok(snap)
        }
        .boxed()
    }

    fn worktree_trash(
        &self,
        req: TrashRequest,
    ) -> BoxFuture<'static, anyhow::Result<TrashOutcome>> {
        self.supervised("moving the worktree to the trash", self.clone().trash(req))
            .boxed()
    }

    fn trash_list(&self) -> BoxFuture<'static, anyhow::Result<Vec<TrashItem>>> {
        self.clone().trash_items().boxed()
    }

    fn disk_usage(
        &self,
        worktree: Option<PathBuf>,
    ) -> BoxFuture<'static, anyhow::Result<twr_core::git::DiskReading>> {
        let g = self.clone();
        async move {
            // The file system of the worktree asked about, else of the box's worktrees: the
            // nearest directory of it that exists.
            let mut at = worktree
                .clone()
                .or_else(|| g.managed.clone())
                .unwrap_or_else(std::env::temp_dir);
            while !at.is_dir() {
                match at.parent() {
                    Some(p) => at = p.to_path_buf(),
                    None => break,
                }
            }
            let (free, total) = disk_free(&at).await.unwrap_or_default();
            let walked = match worktree.filter(|w| w.is_dir()) {
                Some(w) => {
                    let list = g.disposable.clone();
                    Some(
                        tokio::task::spawn_blocking(move || {
                            let _one = trash::DISK_WALK.lock().unwrap_or_else(|e| e.into_inner());
                            trash::worktree_disk(&w, &list)
                        })
                        .await
                        .map_err(|e| anyhow!("measuring the worktree: {e}"))?,
                    )
                }
                None => None,
            };
            Ok(twr_core::git::DiskReading {
                free,
                total,
                worktree: walked,
            })
        }
        .boxed()
    }

    fn worktree_trim(
        &self,
        req: twr_core::git::TrimRequest,
    ) -> BoxFuture<'static, anyhow::Result<twr_core::git::TrimOutcome>> {
        self.supervised("trimming the worktree", self.clone().trim(req))
            .boxed()
    }

    fn trash_purge(&self, purge: TrashPurge) -> BoxFuture<'static, anyhow::Result<PurgeOutcome>> {
        self.supervised("purging the trash", self.clone().purge(purge))
            .boxed()
    }

    fn trash_restore(
        &self,
        req: RestoreRequest,
    ) -> BoxFuture<'static, anyhow::Result<RestoreOutcome>> {
        self.supervised("restoring from the trash", self.clone().restore(req))
            .boxed()
    }

    fn trash_restore_undo(&self, item: String) -> BoxFuture<'static, anyhow::Result<bool>> {
        self.supervised("undoing a restore", self.clone().restore_undo(item))
            .boxed()
    }

    fn branch_only_commits(
        &self,
        targets: Vec<DeleteRefs>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<OnlyCommits>>> {
        self.clone().only_commits(targets).boxed()
    }

    fn worktree_inventory(
        &self,
        repo: PathBuf,
        path: PathBuf,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeInventory>> {
        self.clone().inventory(repo, path).boxed()
    }

    fn commit_of(
        &self,
        repo: PathBuf,
        rev: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<String>>> {
        let g = self.clone();
        async move {
            valid_read_rev(&rev).map_err(|e| anyhow!(e))?;
            let spec = format!("{rev}^{{commit}}");
            let out = g
                .committed(&repo, ["rev-parse", "--verify", "--quiet", spec.as_str()])
                .await?;
            if !out.ok {
                return Ok(None);
            }
            let commit = out.text().trim().to_string();
            if !valid_commit_id(&commit) {
                bail!("git rev-parse {spec} printed {commit:?}");
            }
            Ok(Some(commit))
        }
        .boxed()
    }

    fn tree_list(
        &self,
        repo: PathBuf,
        commit: String,
        dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<Vec<TreeEntry>>>> {
        let g = self.clone();
        async move { g.committed_tree(&repo, &commit, &dir).await }.boxed()
    }

    fn tree_entry(
        &self,
        repo: PathBuf,
        commit: String,
        path: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<TreeEntry>>> {
        let g = self.clone();
        async move {
            let path = valid_read_path(&path).map_err(|e| anyhow!(e))?;
            if path.is_empty() {
                bail!("the top of the repository is a directory, not an entry");
            }
            let (dir, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let Some(entries) = g.committed_tree(&repo, &commit, dir).await? else {
                return Ok(None);
            };
            // A name that is not UTF-8 never equals the (UTF-8) path asked for. A crafted tree
            // can hold one name twice: which is meant is unknown, so neither is read.
            let mut found = entries.into_iter().filter(|e| e.utf8 && e.name == name);
            let first = found.next();
            if found.next().is_some() {
                bail!("the commit has two entries at {path}");
            }
            Ok(first)
        }
        .boxed()
    }

    fn partial_clone(&self, repo: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>> {
        let g = self.clone();
        async move {
            let ext = g
                .committed(&repo, ["config", "--get", "extensions.partialclone"])
                .await?;
            if ext.ok && !ext.text().trim().is_empty() {
                return Ok(true);
            }
            let promisors = g
                .committed(
                    &repo,
                    [
                        "config",
                        "--bool",
                        "--get-regexp",
                        r"^remote\..*\.promisor$",
                    ],
                )
                .await?;
            if promisors.ok
                && promisors
                    .text()
                    .lines()
                    .any(|l| l.trim_end().ends_with(" true"))
            {
                return Ok(true);
            }
            // A remote with a filter is a promisor remote to git whatever `promisor` says.
            let filters = g
                .committed(
                    &repo,
                    [
                        "config",
                        "--get-regexp",
                        r"^remote\..*\.partialclonefilter$",
                    ],
                )
                .await?;
            Ok(filters.ok && !filters.text().trim().is_empty())
        }
        .boxed()
    }

    fn tree_files(
        &self,
        repo: PathBuf,
        commit: String,
        dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Vec<TreeEntry>>> {
        let g = self.clone();
        async move {
            if !valid_commit_id(&commit) {
                bail!("{commit:?} is not a commit id");
            }
            let dir = valid_read_path(&dir).map_err(|e| anyhow!(e))?;
            let mut args: Vec<OsString> = ["ls-tree", "-r", "-z", "--long", "--full-tree"]
                .iter()
                .map(OsString::from)
                .collect();
            args.push((&commit).into());
            if !dir.is_empty() {
                args.push("--".into());
                args.push(format!(":(literal){dir}").into());
            }
            let out = g
                .committed(&repo, args)
                .await?
                .into_stdout_bytes("git ls-tree -r")?;
            parse_ls_tree(&out).map_err(|e| anyhow!(e))
        }
        .boxed()
    }

    fn blob_read(
        &self,
        repo: PathBuf,
        oid: String,
        max: u64,
    ) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> {
        let g = self.clone();
        async move {
            if !valid_commit_id(&oid) {
                bail!("{oid:?} is not an object id");
            }
            // The object itself must be a blob: `cat-file blob` would follow a tag to one,
            // while git grep searches the tag's own text.
            let kind = g
                .committed(&repo, ["cat-file", "-t", oid.as_str()])
                .await?
                .into_stdout("git cat-file -t")?;
            if kind.trim() != "blob" {
                bail!("{oid} is a {}, not a blob", kind.trim());
            }
            let size: u64 = g
                .committed(&repo, ["cat-file", "-s", oid.as_str()])
                .await?
                .into_stdout("git cat-file -s")?
                .trim()
                .parse()
                .context("git cat-file -s printed no size")?;
            if size > max {
                bail!("{oid} is {size} bytes, over {max}");
            }
            g.committed(&repo, ["cat-file", "blob", oid.as_str()])
                .await?
                .into_stdout_bytes("git cat-file blob")
        }
        .boxed()
    }

    fn grep_commit(
        &self,
        repo: PathBuf,
        req: GrepRequest,
    ) -> BoxFuture<'static, anyhow::Result<GrepOut>> {
        let g = self.clone();
        async move {
            if !valid_commit_id(&req.commit) {
                bail!("{:?} is not a commit id", req.commit);
            }
            if req.query.is_empty() || req.query.contains(['\0', '\n', '\r']) {
                bail!("a search is one non-empty line");
            }
            // Every option that changes what is searched or printed is set here, whatever
            // the user's config says (`submodule.recurse`, `grep.column`, ...).
            let mut args: Vec<OsString> = [
                "grep",
                "-z",
                "-n",
                "-I",
                "-F",
                "--no-color",
                "--no-textconv",
                "--no-recurse-submodules",
                "--no-column",
                "--threads=2",
            ]
            .iter()
            .map(OsString::from)
            .collect();
            args.push(format!("--max-count={}", req.per_file.max(1)).into());
            if req.ignore_case {
                args.push("-i".into());
            }
            args.extend([
                "-e".into(),
                OsString::from(&req.query),
                (&req.commit).into(),
            ]);
            args.push("--".into());
            if let Some(dir) = &req.dir {
                let dir = valid_read_path(dir).map_err(|e| anyhow!(e))?;
                if !dir.is_empty() {
                    args.push(format!(":(literal){dir}").into());
                }
            }
            // Names as git listed them: a literal pathspec takes any (a leading `-` or a
            // control character included); only an empty one would mean everything.
            for path in req.exclude.iter().filter(|p| !p.is_empty()) {
                args.push(format!(":(exclude,literal){path}").into());
            }
            let out = g.committed(&repo, args).await?;
            // Exit 1: nothing matched.
            if !out.ok && out.code == Some(1) && out.stderr.trim().is_empty() {
                return Ok(GrepOut::default());
            }
            let out = out.into_stdout_bytes("git grep")?;
            Ok(parse_grep(&out, &req.commit, req.max_hits))
        }
        .boxed()
    }
}

impl LocalGit {
    /// A read-only git command on the committed content of git directory `git_dir`, named
    /// with `--git-dir` so git discovers nothing: no `.git` file or directory in it, or in a
    /// work tree, can point it at another repository, and `safe.bareRepository=explicit`
    /// allows it.
    async fn committed<I, S>(&self, git_dir: &Path, args: I) -> anyhow::Result<Out>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut git_dir_arg = OsString::from("--git-dir=");
        git_dir_arg.push(git_dir);
        let mut all = vec![git_dir_arg];
        all.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
        self.run(git_dir, all, Mode::Committed(self.timeouts.status), None)
            .await
    }

    /// The entries of directory `dir` of commit `commit` in `repo`, `None` when it is no
    /// directory there (missing, or a file).
    async fn committed_tree(
        &self,
        repo: &Path,
        commit: &str,
        dir: &str,
    ) -> anyhow::Result<Option<Vec<TreeEntry>>> {
        if !valid_commit_id(commit) {
            bail!("{commit:?} is not a commit id");
        }
        let dir = valid_read_path(dir).map_err(|e| anyhow!(e))?;
        let spec = format!("{commit}:{dir}");
        let kind = self
            .committed(repo, ["cat-file", "-t", spec.as_str()])
            .await?;
        if !kind.ok || kind.text().trim() != "tree" {
            return Ok(None);
        }
        let out = self
            .committed(repo, ["ls-tree", "-z", "--long", spec.as_str()])
            .await?
            .into_stdout_bytes("git ls-tree")?;
        Ok(Some(parse_ls_tree(&out).map_err(|e| anyhow!(e))?))
    }
}

#[cfg(test)]
mod tests {
    use super::trash::Step;
    use super::*;
    use crate::test_support::install;
    use twr_core::git::LinkedWorktree;
    use twr_core::testkit::git::{branch_exists, commit_all, init_repo, run, worktrees};

    fn spec(repo: &Path, path: &Path, branch: &str, base: Option<&str>) -> WorktreeSpec {
        WorktreeSpec {
            repo: repo.into(),
            path: path.into(),
            branch: branch.into(),
            base: base.map(str::to_string),
        }
    }

    /// A repository's config can name programs git runs: filters (`clean`, `smudge`,
    /// `process`), merge and diff drivers. Twr's git runs none of them, the repository's
    /// (also one an `includeIf "onbranch:..."` brings in for the thread's branch only) or an
    /// initialized submodule's, making a worktree, reading its status or removing it.
    #[tokio::test]
    async fn a_repositorys_drivers_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let marker = dir.path().join("ran");
        let touch = |what: &str| format!("echo {what} >> '{}'; cat", marker.display());
        // A submodule, and the superproject that has it.
        let sub = init_repo(&dir.path().join("sub"));
        std::fs::write(sub.join("s.txt"), "s\n").unwrap();
        commit_all(&sub, "sub file");
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(
            repo.join(".gitattributes"),
            "* filter=evil merge=evil diff=evil\n",
        )
        .unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        run(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "sub",
            ],
        );
        commit_all(&repo, "files");
        // Defined once committed (the test's own commits would run them).
        for (k, v) in [
            ("filter.evil.clean", touch("clean")),
            ("filter.evil.smudge", touch("smudge")),
            ("filter.evil.process", touch("process")),
            ("filter.evil.required", "true".into()),
            ("merge.evil.driver", touch("merge")),
            ("diff.evil.textconv", touch("textconv")),
        ] {
            run(&repo, &["config", k, &v]);
        }
        // Only on a thread's branch: read where the worktree is checked out.
        let onb = dir.path().join("onbranch.cfg");
        std::fs::write(
            &onb,
            format!("[filter \"onb\"]\n\tsmudge = {}\n", touch("onbranch")),
        )
        .unwrap();
        run(
            &repo,
            &[
                "config",
                "includeIf.onbranch:twr/**.path",
                onb.to_str().unwrap(),
            ],
        );
        std::fs::write(repo.join(".git/info/attributes"), "a.txt filter=onb\n").unwrap();
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
        // The submodule initialized in the worktree (its repository under the worktree's own
        // git dir), with a filter of its own.
        run(
            &wt,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
                "-q",
            ],
        );
        let wsub = wt.join("sub");
        run(&wsub, &["config", "filter.subevil.clean", &touch("sub")]);
        let sub_git = PathBuf::from(run(&wsub, &["rev-parse", "--absolute-git-dir"]).trim());
        std::fs::create_dir_all(sub_git.join("info")).unwrap();
        std::fs::write(sub_git.join("info/attributes"), "* filter=subevil\n").unwrap();
        // The same size, a later time: git hashes them again to see.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        for f in [wt.join("a.txt"), wsub.join("s.txt")] {
            let content = std::fs::read_to_string(&f).unwrap();
            std::fs::write(&f, content.replace(['a', 's'], "b")).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&f)
                .unwrap()
                .set_modified(later)
                .unwrap();
        }
        assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 1);
        let names = g
            .neutralized(std::slice::from_ref(&wt), true)
            .await
            .unwrap();
        for want in [
            "filter.subevil.clean",
            "filter.onb.smudge",
            "filter.evil.clean",
        ] {
            assert!(names.iter().any(|(k, _)| k == want), "{want}: {names:?}");
        }
        // What a removal checks goes into the submodule.
        let st = g.porcelain(&wt, true, true).await.unwrap();
        assert!(st.snapshot().dirty_files >= 1);
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        // Plain git runs them (and fails here, the `process` one speaking no protocol): the
        // test would see it.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["checkout", "--", "a.txt"])
            .output()
            .unwrap();
        assert!(marker.exists());
        // A submodule whose own config puts its work tree elsewhere: git's status in it would
        // go there, which twr does not read; nothing runs.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        run(
            &wsub,
            &["config", "core.worktree", elsewhere.to_str().unwrap()],
        );
        let e = g
            .neutralized(std::slice::from_ref(&wt), true)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("work tree elsewhere"), "{e:#}");
    }

    /// A snapshot with a base says how far the work tree is ahead and behind of it and which
    /// files a merge would conflict in, running no merge driver the repository names; a
    /// worktree made from the repository's `HEAD` records the branch that was on.
    #[tokio::test]
    async fn snapshots_tell_merge_readiness_against_a_base() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join("b.txt"), "one\n").unwrap();
        commit_all(&repo, "files");
        let on = run(&repo, &["symbolic-ref", "--short", "HEAD"])
            .trim()
            .to_string();
        let made = g
            .worktree_create(spec(&repo, &dir.path().join("wt"), "twr/x", None))
            .await
            .unwrap();
        assert_eq!(made.base_ref, Some(format!("refs/heads/{on}")));
        let wt = std::fs::canonicalize(dir.path().join("wt")).unwrap();
        let readiness = |s: &GitSnapshot| {
            (
                s.base.clone(),
                s.base_ahead,
                s.base_behind,
                s.conflicts.clone(),
            )
        };
        // Without `--attr-source` (an older git): the counts, conflicts unknown.
        if !twr_core::testkit::git::merge_tree_writes() {
            let s = g.snapshot(wt.clone(), Some(on.clone())).await.unwrap();
            assert_eq!(
                (s.base_ahead, s.base_behind, s.conflicts),
                (Some(0), Some(0), None)
            );
            return;
        }
        // Level with its base: nothing either way, merges cleanly.
        let s = g.snapshot(wt.clone(), Some(on.clone())).await.unwrap();
        assert_eq!(
            readiness(&s),
            (Some(on.clone()), Some(0), Some(0), Some(vec![]))
        );
        // One commit each, on different files: still clean.
        std::fs::write(wt.join("a.txt"), "two\n").unwrap();
        commit_all(&wt, "a on the branch");
        std::fs::write(repo.join("b.txt"), "two\n").unwrap();
        commit_all(&repo, "b on the base");
        let s = g.snapshot(wt.clone(), Some(on.clone())).await.unwrap();
        assert_eq!(
            readiness(&s),
            (Some(on.clone()), Some(1), Some(1), Some(vec![]))
        );
        assert_eq!(s.conflicted, Some(false));
        // The same file on both sides: a conflict. The merge driver the repository names for
        // it would merge it cleanly; it doesn't run (its file counts as conflicting).
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        commit_all(&repo, "a on the base");
        let marker = dir.path().join("ran");
        run(
            &repo,
            &[
                "config",
                "merge.evil.driver",
                &format!("echo merge >> '{}'; true", marker.display()),
            ],
        );
        std::fs::write(repo.join(".git/info/attributes"), "a.txt merge=evil\n").unwrap();
        let s = g.snapshot(wt.clone(), Some(on.clone())).await.unwrap();
        assert_eq!(s.base_ahead, Some(1));
        assert_eq!(s.base_behind, Some(2));
        assert_eq!(s.conflicts, Some(vec!["a.txt".to_string()]));
        assert_eq!(s.conflicted, Some(true));
        assert!(!marker.exists());
        // The turned-off driver's temporary files went nowhere near the checkout.
        let left: Vec<_> = std::fs::read_dir(&wt)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(".merge_file"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        // With `PATH` leading to a directory of the repository's (programs `false`, `true`,
        // `cat` there), still nothing of the repository's runs, and the conflict stays one.
        let bin = repo.join(".bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["false", "true", "cat", "exit"] {
            install(
                &bin.join(name),
                format!("#!/bin/sh\necho {name} >> '{}'\nexit 0\n", marker.display()),
            );
        }
        let wrapper = dir.path().join("git-with-path");
        install(
            &wrapper,
            format!(
                "#!/bin/sh\nPATH='{}':\"$PATH\" exec git \"$@\"\n",
                bin.display()
            ),
        );
        let s = LocalGit::with_bin(&wrapper)
            .snapshot(wt.clone(), Some(on.clone()))
            .await
            .unwrap();
        assert_eq!(s.conflicts, Some(vec!["a.txt".to_string()]));
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        // A base that is no commit: no readiness, the rest as before.
        let s = g.snapshot(wt.clone(), Some("nosuch".into())).await.unwrap();
        assert_eq!(s.base, None);
        assert_eq!(s.branch.as_deref(), Some("twr/x"));
        assert!(g
            .snapshot(wt.clone(), Some("-x".into()))
            .await
            .unwrap()
            .base
            .is_none());
    }

    /// A worktree's files go in it: never where a `core.worktree` the repository sets points
    /// (the main checkout, whose uncommitted file stays). One left unfinished (its checkout
    /// cut short) counts as none, and making it again starts over.
    #[tokio::test]
    async fn a_new_worktrees_files_go_in_it_and_an_unfinished_one_is_redone() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        std::fs::write(repo.join("a.txt"), "mine, not committed\n").unwrap();
        run(&repo, &["config", "extensions.worktreeConfig", "true"]);
        run(&repo, &["config", "core.worktree", repo.to_str().unwrap()]);
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        assert_eq!(
            std::fs::read_to_string(repo.join("a.txt")).unwrap(),
            "mine, not committed\n"
        );
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
        // Its status is its own, not the main checkout's (an edit and a new file there).
        std::fs::write(repo.join("new.txt"), "x").unwrap();
        assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 0);
        // From a subdirectory of a checkout, its status is the checkout's.
        std::fs::create_dir_all(wt.join("deep/er")).unwrap();
        assert_eq!(
            g.snapshot(wt.join("deep/er"), None)
                .await
                .unwrap()
                .dirty_files,
            0
        );
        // Through a symlink to that subdirectory too.
        let link = dir.path().join("link-to-deep");
        std::os::unix::fs::symlink(wt.join("deep"), &link).unwrap();
        assert_eq!(g.snapshot(link, None).await.unwrap().dirty_files, 0);
        run(&repo, &["config", "--unset", "core.worktree"]);
        // Cut short: as if the process died before its checkout ended.
        let marker = pending_marker(&wt).unwrap();
        std::fs::write(&marker, b"").unwrap();
        std::fs::remove_file(wt.join("a.txt")).unwrap();
        assert!(!g.worktree_exists(wt.clone()).await.unwrap());
        let again = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        assert_eq!(std::fs::read_to_string(again.join("a.txt")).unwrap(), "a\n");
        assert!(!pending_marker(&again).unwrap().exists());
        assert!(g.worktree_exists(again).await.unwrap());
    }

    /// Starting over an unfinished worktree removes only one proven twr's, at its path, on its
    /// branch: a symlink there to another checkout with a mark of its own is left alone, with
    /// what is in it.
    #[tokio::test]
    async fn an_unfinished_worktree_is_redone_only_when_it_is_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        let other = new_wt(&g, &repo, &dir.path().join("other"), "twr/other").await;
        std::fs::write(other.join("work.txt"), "someone's work").unwrap();
        std::fs::write(pending_marker(&other).unwrap(), b"").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let e = g
            .worktree_create(spec(&repo, &link, "twr/link", None))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("nothing is touched"), "{e:#}");
        assert_eq!(
            std::fs::read_to_string(other.join("work.txt")).unwrap(),
            "someone's work"
        );
    }

    /// A command on a work tree is pinned to its checkout's top; one that names its own git dir
    /// (the trash's index listing, run from the common dir) is set up as it says.
    #[test]
    fn work_tree_commands_are_pinned_unless_they_name_their_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        let pins = |a: &[&str]| {
            g.command(dir.path(), &args(a), false, &[])
                .as_std()
                .get_args()
                .any(|x| x.to_string_lossy().starts_with("--work-tree="))
        };
        assert!(pins(&["status"]));
        // A relative directory pins its absolute checkout.
        assert!(checkout_top(Path::new("relative/dir")).is_absolute());
        assert!(pins(&["read-tree", "--reset", "-u", "HEAD"]));
        assert!(!pins(&["--git-dir=/x", "ls-files", "--sparse"]));
        assert!(!pins(&["rev-parse", "--show-toplevel"]));
    }

    /// How files merge is the checkout's tracked `.gitattributes`' say: `-merge` makes edits on
    /// both sides of a file a conflict, however far apart.
    #[tokio::test]
    async fn readiness_merges_by_the_checkouts_attributes() {
        if !twr_core::testkit::git::merge_tree_writes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt -merge\n").unwrap();
        std::fs::write(repo.join("c.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        commit_all(&repo, "files");
        let on = run(&repo, &["symbolic-ref", "HEAD"]).trim().to_string();
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        std::fs::write(wt.join("c.txt"), "ONE\ntwo\nthree\nfour\nfive\n").unwrap();
        commit_all(&wt, "first line");
        std::fs::write(repo.join("c.txt"), "one\ntwo\nthree\nfour\nFIVE\n").unwrap();
        commit_all(&repo, "last line");
        let s = g.snapshot(wt.clone(), Some(on)).await.unwrap();
        assert_eq!(s.conflicted, Some(true), "{s:?}");
        assert_eq!(s.conflicts, Some(vec!["c.txt".to_string()]));
    }

    /// A worktree records what its work merges back into as a name that keeps its meaning
    /// from any checkout: the branch for `@` or no base, a branch or tag by name, the commit
    /// for a revision like `HEAD~1`. One without an index (its checkout never ran) is no
    /// worktree; made again, it is redone when nothing is in it, and kept when files are.
    #[tokio::test]
    async fn bases_are_kept_as_stable_names_and_a_worktree_without_index_is_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "one");
        std::fs::write(repo.join("a.txt"), "b\n").unwrap();
        commit_all(&repo, "two");
        run(&repo, &["tag", "v1"]);
        let on = run(&repo, &["symbolic-ref", "--short", "HEAD"])
            .trim()
            .to_string();
        let first = run(&repo, &["rev-parse", "HEAD~1"]).trim().to_string();
        let mut n = 0;
        let mut base_of = |b: Option<&str>| {
            n += 1;
            let (g, repo, path) = (g.clone(), repo.clone(), dir.path().join(format!("w{n}")));
            let b = b.map(str::to_string);
            async move {
                g.worktree_create(spec(&repo, &path, &format!("twr/w{n}"), b.as_deref()))
                    .await
                    .unwrap()
                    .base_ref
            }
        };
        let full = format!("refs/heads/{on}");
        assert_eq!(base_of(None).await.as_deref(), Some(full.as_str()));
        assert_eq!(base_of(Some("@")).await.as_deref(), Some(full.as_str()));
        assert_eq!(base_of(Some(&on)).await.as_deref(), Some(full.as_str()));
        assert_eq!(base_of(Some("v1")).await.as_deref(), Some("refs/tags/v1"));
        // A branch name of 250 bytes, qualified past the input limit, still has its readiness.
        let long = "b".repeat(250);
        run(&repo, &["branch", &long]);
        let wt_long = dir.path().join("wlong");
        let made = g
            .worktree_create(spec(&repo, &wt_long, "twr/wlong", Some(&long)))
            .await
            .unwrap();
        let s = g
            .snapshot(
                std::fs::canonicalize(&wt_long).unwrap(),
                made.base_ref.clone(),
            )
            .await
            .unwrap();
        assert_eq!(s.base_ahead, Some(0), "{made:?}");
        // A branch and a tag of one name stay apart.
        run(&repo, &["branch", "release", "HEAD~1"]);
        run(&repo, &["tag", "release"]);
        assert_eq!(
            base_of(Some("refs/heads/release")).await.as_deref(),
            Some("refs/heads/release")
        );
        assert_eq!(
            base_of(Some("HEAD~1")).await.as_deref(),
            Some(first.as_str())
        );
        // A branch whose name is no UTF-8: its commit, which a name could not keep.
        let odd = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("branch")
            .arg(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(
                b"topic\xff",
            ))
            .status()
            .unwrap();
        if odd.success() {
            let tip = run(&repo, &["rev-parse", "HEAD"]).trim().to_string();
            let name = b"topic\xff";
            let mut full = b"refs/heads/".to_vec();
            full.extend_from_slice(name);
            let st = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .arg("symbolic-ref")
                .arg("HEAD")
                .arg(<OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&full))
                .status()
                .unwrap();
            assert!(st.success());
            assert_eq!(base_of(None).await.as_deref(), Some(tip.as_str()));
            run(
                &repo,
                &["symbolic-ref", "HEAD", &format!("refs/heads/{on}")],
            );
        }
        // No index: unfinished. Files in it: kept; nothing but its `.git`: redone.
        let wt = new_wt(&g, &repo, &dir.path().join("x"), "twr/x").await;
        let admin = pending_marker(&wt).unwrap().parent().unwrap().to_path_buf();
        std::fs::remove_file(admin.join("index")).unwrap();
        assert!(!g.worktree_exists(wt.clone()).await.unwrap());
        let e = g
            .worktree_create(spec(&repo, &wt, "twr/x", None))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("remove it yourself"), "{e:#}");
        for e in std::fs::read_dir(&wt).unwrap().flatten() {
            if e.file_name() != ".git" {
                std::fs::remove_file(e.path()).unwrap();
            }
        }
        let again = new_wt(&g, &repo, &wt, "twr/x").await;
        assert_eq!(std::fs::read_to_string(again.join("a.txt")).unwrap(), "b\n");
    }

    /// The mark of an unfinished worktree is looked for only through a small regular `.git`
    /// file: a FIFO or a link to `/dev/zero` in its place neither hangs nor floods.
    #[tokio::test]
    async fn an_odd_dot_git_names_no_pending_mark() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("fifo");
        std::fs::create_dir(&fifo).unwrap();
        let st = std::process::Command::new("mkfifo")
            .arg(fifo.join(".git"))
            .status()
            .unwrap();
        assert!(st.success());
        let zero = dir.path().join("zero");
        std::fs::create_dir(&zero).unwrap();
        std::os::unix::fs::symlink("/dev/zero", zero.join(".git")).unwrap();
        for d in [&fifo, &zero] {
            assert_eq!(pending_marker(d), None);
            let exists = tokio::time::timeout(
                Duration::from_secs(5),
                LocalGit::default().worktree_exists(d.to_path_buf()),
            )
            .await
            .expect("hung on .git");
            assert!(exists.unwrap());
        }
    }

    /// A submodule named in bytes that are no UTF-8 is read where it is: no command is
    /// refused for it.
    #[tokio::test]
    async fn odd_submodule_names_are_read_where_they_are() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let sub = init_repo(&dir.path().join("sub"));
        std::fs::write(sub.join("s.txt"), "s\n").unwrap();
        commit_all(&sub, "sub file");
        let repo = init_repo(&dir.path().join("repo"));
        // (git refuses a submodule name with a line break.)
        let name = OsString::from_vec(b"odd\xff".to_vec());
        if let Err(e) = std::fs::create_dir(dir.path().join(&name)) {
            eprintln!("skipped: this file system refuses non-UTF-8 names ({e})");
            return;
        }
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "protocol.file.allow=always", "submodule", "add", "-q"])
            .arg(&sub)
            .arg(&name)
            .status()
            .unwrap();
        assert!(st.success());
        commit_all(&repo, "subs");
        let names = g.neutralized(std::slice::from_ref(&repo), true).await;
        assert!(names.is_ok(), "{names:?}");
    }

    #[test]
    fn drivers_are_neutralized_by_name() {
        let names = b"core.bare\0filter.lfs.clean\0filter.a.b.smudge\0merge.m.driver\0\
                      diff.d.textconv\0filter.x\0merge.m.name\0diff.external\0";
        let got: Vec<(String, String)> = neutralize(names)
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        for want in [
            ("filter.lfs.clean", ""),
            ("filter.lfs.smudge", ""),
            ("filter.lfs.process", ""),
            ("filter.lfs.required", "false"),
            ("filter.a.b.clean", ""),
            ("merge.m.driver", FAILS),
            ("diff.d.textconv", FAILS),
            ("diff.d.cachetextconv", "false"),
            ("diff.d.command", SUCCEEDS),
            ("diff.external", SUCCEEDS),
        ] {
            assert!(
                got.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} not in {got:?}"
            );
        }
        assert!(!got
            .iter()
            .any(|(k, _)| k.starts_with("core.") || k == "filter.x"));
        // A name `-c` can't carry is refused.
        assert!(neutralize(b"filter.a=b.clean\0").is_err());
        // A hook without a name, too.
        let flat: Vec<(String, String)> = neutralize(b"hook.event\0hook.command\0")
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        assert_eq!(
            flat,
            [
                ("hook.command".to_string(), SUCCEEDS.to_string()),
                ("hook.enabled".to_string(), "false".to_string())
            ]
        );
        // A hook defined in config is turned off too.
        let hooks: Vec<(String, String)> = neutralize(b"hook.evil.event\0hook.evil.command\0")
            .unwrap()
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        assert_eq!(
            hooks,
            [
                ("hook.evil.command".to_string(), SUCCEEDS.to_string()),
                ("hook.evil.enabled".to_string(), "false".to_string())
            ]
        );
        // Only what a repository sets: not what you set globally (Git LFS's filter).
        let scoped = b"global\0filter.lfs.clean\0local\0filter.evil.smudge\0command\0\
                       filter.c.clean\0worktree\0merge.w.driver\0";
        assert_eq!(
            repository_scoped(scoped),
            b"filter.evil.smudge\0merge.w.driver\0".to_vec()
        );
        assert_eq!(
            subcommand(&["--no-optional-locks".into(), "status".into()]),
            Some("status")
        );
        let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(runs_no_driver(&args(&["rev-parse", "HEAD"])));
        assert!(runs_no_driver(&args(&["cat-file", "--batch"])));
        assert!(!runs_no_driver(&args(&["cat-file", "--filters", "HEAD:a"])));
        assert!(!runs_no_driver(&args(&["--no-optional-locks", "status"])));
        assert!(!runs_no_driver(&args(&["worktree", "add", "x"])));
        // A diff driver's `textconv` runs for them unasked, and `gpg.program` for a signature.
        assert!(!runs_no_driver(&args(&["show", "HEAD"])));
        assert!(!runs_no_driver(&args(&["log", "-1"])));
        assert!(!runs_no_driver(&args(&["grep", "--textconv", "x"])));
        // Abbreviated as git takes them.
        assert!(!runs_no_driver(&args(&["grep", "--textc", "x"])));
        assert!(!runs_no_driver(&args(&["cat-file", "--filt", "HEAD:a"])));
        // `ls-files` reads the index alone, unless it compares files with it.
        assert!(runs_no_driver(&args(&["ls-files", "--stage", "-z"])));
        assert!(runs_no_driver(&args(&["ls-files", "-v", "-s", "-z"])));
        assert!(runs_no_driver(&args(&["ls-files", "--sparse", "-s", "-z"])));
        for asked in [
            &["ls-files", "-m"][..],
            &["ls-files", "--modified"],
            &["ls-files", "--mod"],
            &["ls-files", "-om", "-z"],
            &["ls-files", "-d"],
            &["ls-files", "--deleted"],
            &["ls-files", "-t"],
        ] {
            assert!(!runs_no_driver(&args(asked)), "{asked:?}");
        }
        // A global option's value is no subcommand.
        assert_eq!(
            subcommand(&args(&["-C", "rev-parse", "status"])),
            Some("status")
        );
        assert_eq!(
            subcommand(&args(&["--git-dir", "config", "-c", "a=b", "add", "x"])),
            Some("add")
        );
        assert!(!runs_no_driver(&args(&[
            "-C",
            "rev-parse",
            "checkout",
            "x"
        ])));
        assert_eq!(
            subcommand(&args(&["--git-dir=/x", "ls-tree"])),
            Some("ls-tree")
        );
        // What a listing after a command finds that the one before it did not: by driver.
        let kv = |n: &[u8]| neutralize(n).unwrap();
        let before = kv(b"filter.a.clean\0merge.m.driver\0");
        let after = kv(b"filter.a.smudge\0filter.b.c.process\0diff.external\0merge.m.driver\0");
        assert_eq!(unturned(&before, &after), ["diff.external", "filter.b.c"]);
        assert!(unturned(&after, &before).is_empty());
        // What makes Git LFS run a program turns its filter off; the rest of its config not.
        for name in [
            "lfs.customtransfer.evil.path",
            "lfs.customTransfer.evil.args",
            "lfs.standalonetransferagent",
            "lfs.https://lfs.example/.standalonetransferagent",
            "lfs.extension.x.smudge",
            "core.askpass",
            "core.sshcommand",
            "credential.helper",
            "credential.https://example.invalid.helper",
        ] {
            assert!(lfs_runs(name.as_bytes()), "{name}");
            let off = kv(format!("{name}\0").as_bytes());
            assert!(
                off.contains(&("filter.lfs.process".into(), "".into()))
                    && off.contains(&("filter.lfs.required".into(), "false".into())),
                "{name}: {off:?}"
            );
        }
        for name in [
            "lfs.url",
            "lfs.fetchinclude",
            "lfs.https://lfs.example/.access",
            "remote.origin.lfsurl",
            "core.editor",
            "credential.username",
        ] {
            assert!(!lfs_runs(name.as_bytes()), "{name}");
        }
    }

    #[tokio::test]
    async fn repo_root_finds_the_top_level_or_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(g.repo_root(plain).await.unwrap(), None);
        let repo = init_repo(&dir.path().join("repo"));
        let sub = repo.join("src");
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(g.repo_root(sub).await.unwrap(), Some(repo.clone()));
        // No git binary at all: no repositories.
        let none = LocalGit::with_bin(dir.path().join("no-such-git"));
        assert_eq!(none.repo_root(repo).await.unwrap(), None);
    }

    /// `origin_url`: the URL, none without an origin, and an error without git's message when
    /// git fails, which quotes the config: an invalid `remote.origin.fetch` holding a URL with
    /// a password is not in it.
    #[tokio::test]
    async fn origin_url_never_holds_git_s_message() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        assert_eq!(g.origin_url(&repo).await.unwrap(), None);
        run(&repo, &["remote", "add", "origin", "https://h.example/o/r"]);
        assert_eq!(
            g.origin_url(&repo).await.unwrap().as_deref(),
            Some("https://h.example/o/r")
        );
        run(
            &repo,
            &[
                "config",
                "remote.origin.fetch",
                "https://user:SECRET@host/repo",
            ],
        );
        let e = format!("{:#}", g.origin_url(&repo).await.unwrap_err());
        assert!(e.contains("git remote get-url origin failed"), "{e}");
        assert!(!e.contains("SECRET") && !e.contains("refspec"), "{e}");
        // A step before it fails (the listing of the config twr reads first, which an include
        // of a user that does not exist fails), quoting the config: not in it either.
        let other = init_repo(&dir.path().join("other"));
        run(
            &other,
            &["remote", "add", "origin", "https://h.example/o/r"],
        );
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(other.join(".git/config"))
            .unwrap();
        std::io::Write::write_all(&mut config, b"[include]\n\tpath = ~nosuchuserSECRET/x\n")
            .unwrap();
        let e = format!("{:#}", g.origin_url(&other).await.unwrap_err());
        assert!(e.contains("git remote get-url origin failed"), "{e}");
        assert!(!e.contains("SECRET") && !e.contains("nosuchuser"), "{e}");
        // No git at all: said so, still a fixed message.
        let none = LocalGit::with_bin(dir.path().join("no-such-git"));
        let e = format!("{:#}", none.origin_url(&repo).await.unwrap_err());
        assert!(e.contains("not installed"), "{e}");
        assert!(none.runs().await.is_err());
        g.runs().await.unwrap();
    }

    /// `g` with its trash at `<dir>/trash`.
    fn with_trash(g: &LocalGit, dir: &Path) -> LocalGit {
        g.clone().with_trash_root(dir.join("trash"))
    }

    /// Moves the worktree `wt` of `repo` and `branch` to the trash of `g` as a new item,
    /// deleted at `at` (ms).
    async fn to_trash_at(
        g: &LocalGit,
        repo: &Path,
        wt: &Path,
        branch: Option<&str>,
        at: u64,
    ) -> anyhow::Result<TrashOutcome> {
        to_trash_among(g, repo, wt, branch, at, None).await
    }

    /// [`to_trash_at`], with the hub's list of the other threads' branches `others` (`None`:
    /// an older hub's request, which has none).
    async fn to_trash_among(
        g: &LocalGit,
        repo: &Path,
        wt: &Path,
        branch: Option<&str>,
        at: u64,
        others: Option<&[&str]>,
    ) -> anyhow::Result<TrashOutcome> {
        g.worktree_trash(TrashRequest {
            item: twr_core::event::new_id(),
            repo: repo.into(),
            worktree: wt.into(),
            branch: branch.map(str::to_string),
            other_branches: others.map(|o| others_of(repo, o)),
            info: twr_core::git::TrashInfo {
                thread: "t-id".into(),
                thread_name: "t".into(),
                box_name: "local".into(),
                deleted_at_ms: at,
                losses: "nothing".into(),
                ..Default::default()
            },
        })
        .await
    }

    /// The recorded branches `branches` of other threads of `repo` ([`OtherBranches`]).
    fn others_of(repo: &Path, branches: &[&str]) -> Vec<twr_core::git::OtherBranch> {
        branches
            .iter()
            .map(|b| twr_core::git::OtherBranch {
                repo: repo.to_path_buf(),
                branch: b.to_string(),
            })
            .collect()
    }

    /// [`to_trash_at`] at 1000 ms.
    async fn to_trash(
        g: &LocalGit,
        repo: &Path,
        wt: &Path,
        branch: Option<&str>,
    ) -> anyhow::Result<TrashOutcome> {
        to_trash_at(g, repo, wt, branch, 1000).await
    }

    /// Whether some ref under `refs/twr-trash/<item>/` reaches commit `c`.
    fn kept_by_trash(repo: &Path, item: &str, c: &str) -> bool {
        !run(
            repo,
            &[
                "for-each-ref",
                "--contains",
                c,
                &format!("refs/twr-trash/{item}/"),
            ],
        )
        .is_empty()
    }

    /// Whether the repository has object `id`.
    fn has_object(repo: &Path, id: &str) -> bool {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["cat-file", "-e", id])
            .status()
            .unwrap()
            .success()
    }

    /// Garbage-collects `repo` as hard as git can: every reflog entry expired, every
    /// unreachable object pruned.
    fn gc_now(repo: &Path) {
        run(
            repo,
            &[
                "-c",
                "gc.reflogExpire=now",
                "-c",
                "gc.reflogExpireUnreachable=now",
                "gc",
                "-q",
                "--prune=now",
            ],
        );
    }

    /// Moving a thread to the trash moves its worktree (dirty, untracked and ignored
    /// files and all, but its build output, which it deletes) and its record into the item,
    /// keeps every commit its branch, `HEAD`, reflogs and own refs reached under
    /// `refs/twr-trash/<item>/` (they survive the hardest
    /// garbage collection), deletes the branch, and frees the name at once; what is no linked
    /// worktree of the repository is never touched; nothing to keep makes no item.
    #[tokio::test]
    async fn trash_moves_the_worktree_and_keeps_every_commit() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("worktrees").join("proj").join("t1");
        g.worktree_create(spec(&repo, &wt, "twr/t1", None))
            .await
            .unwrap();
        std::fs::write(wt.join("a.txt"), "a").unwrap();
        commit_all(&wt, "one");
        // Rewritten away: only the branch's (and HEAD's) reflog has it.
        let amended = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["commit", "-q", "--amend", "-m", "one, again"]);
        // Made on a detached HEAD, then left: only the worktree's HEAD reflog has it.
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        commit_all(&wt, "detached");
        let detached = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["checkout", "-q", "twr/t1"]);
        // Only a ref of the worktree's own reaches it.
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("w.txt"), "w").unwrap();
        commit_all(&wt, "worktree ref");
        let own = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["update-ref", "refs/worktree/keep", &own]);
        run(&wt, &["checkout", "-q", "twr/t1"]);
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        std::fs::write(wt.join("untracked.txt"), "x").unwrap();
        std::fs::write(wt.join(".gitignore"), "target/\n*.local\n").unwrap();
        std::fs::write(wt.join("big.local"), vec![7u8; 64 << 10]).unwrap();
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target").join("out.bin"), "o").unwrap();
        // The main checkout, a directory inside the worktree and another repository are
        // refused, and nothing moves.
        let e = to_trash(&g, &repo, &repo, None).await.unwrap_err();
        assert!(format!("{e:#}").contains("main checkout"), "{e:#}");
        std::fs::create_dir(wt.join("sub")).unwrap();
        let e = to_trash(&g, &repo, &wt.join("sub"), None)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("top level"), "{e:#}");
        let other = init_repo(&dir.path().join("other"));
        let e = to_trash(&g, &repo, &other, None).await.unwrap_err();
        assert!(format!("{e:#}").contains("not a worktree of"), "{e:#}");
        assert!(wt.exists() && other.exists());
        assert!(!dir.path().join("trash").exists() || list(&g).await.is_empty());
        // Without a trash directory, nothing is deleted either.
        let e = to_trash(&LocalGit::default(), &repo, &wt, Some("twr/t1"))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("no trash directory"), "{e:#}");

        let out = to_trash(&g, &repo, &wt, Some("twr/t1")).await.unwrap();
        let item = out.item.clone().unwrap();
        let item_dir = out.dir.clone().unwrap();
        assert_eq!(item_dir, dir.path().join("trash").join(&item));
        assert!(out.worktree_moved && out.branch_deleted && !out.worktree_left);
        assert!(out.bytes >= 64 << 10, "{}", out.bytes);
        assert!(!wt.exists());
        assert!(!worktrees(&repo).contains(&wt));
        assert!(!branch_exists(&repo, "twr/t1"));
        for f in ["README.md", "untracked.txt", "big.local", ".git"] {
            assert!(item_dir.join("worktree").join(f).exists(), "{f}");
        }
        // Its build output was deleted, not kept (only on Linux and macOS: elsewhere it is kept).
        let deletes = cfg!(any(target_os = "linux", target_os = "macos"));
        assert_eq!(item_dir.join("worktree/target").exists(), !deletes);
        assert!(item_dir.join("record").join("HEAD").exists());
        let items = list(&g).await;
        assert_eq!(items.len(), 1);
        let m = &items[0].manifest;
        assert!(m.complete && m.worktree_moved && m.record_moved && m.branch_deleted);
        let build: &[&str] = if deletes { &["target/"] } else { &[] };
        assert_eq!(m.build_output, build);
        assert_eq!(
            (m.item.as_str(), m.worktree.as_path()),
            (item.as_str(), wt.as_path())
        );
        assert_eq!(m.head.as_deref(), Some("ref: refs/heads/twr/t1"));
        assert!(
            m.recover.contains(&format!("refs/twr-trash/{item}/branch")),
            "{}",
            m.recover
        );
        let kinds: Vec<&str> = m.refs.iter().map(|r| r.kind.as_str()).collect();
        for k in ["branch", "head", "worktree-ref", "reflog"] {
            assert!(kinds.contains(&k), "{kinds:?}");
        }
        // The branch's reflog came along.
        assert!(
            run(&repo, &["reflog", &format!("refs/twr-trash/{item}/branch")]).contains("amend")
        );
        gc_now(&repo);
        for c in [&amended, &detached, &own, &tip] {
            assert!(kept_by_trash(&repo, &item, c), "{c}");
            assert!(has_object(&repo, c), "{c}");
        }
        // The name is free at once; the trash's refs are no copy that stays: the commit only
        // they and the new branch have counts as the branch's own.
        g.worktree_create(spec(&repo, &wt, "twr/t1", Some(&tip)))
            .await
            .unwrap();
        assert_eq!(branch_only(&g, &repo, "twr/t1", &wt).await, Some(1));

        // Nothing at all left of a thread: no item.
        let gone = dir.path().join("worktrees").join("proj").join("none");
        let out = to_trash(&g, &repo, &gone, Some("twr/none")).await.unwrap();
        assert_eq!(out, TrashOutcome::default());
        // A worktree whose directory is gone: its record and branch go to the trash.
        let wt2 = new_wt(
            &g,
            &repo,
            &dir.path().join("worktrees").join("proj").join("t2"),
            "twr/t2",
        )
        .await;
        std::fs::remove_dir_all(&wt2).unwrap();
        let out = to_trash(&g, &repo, &wt2, Some("twr/t2")).await.unwrap();
        assert!(!out.worktree_moved && out.branch_deleted);
        assert!(out.dir.unwrap().join("record").exists());
        assert!(!worktrees(&repo).contains(&wt2) && !branch_exists(&repo, "twr/t2"));
    }

    /// A deletion deletes the worktree's build output (`[threads] park_disposable`, at any
    /// depth) instead of moving it to the trash, and the inventory names the same entries: never
    /// one that holds a git repository, never what a symlink points at, never other ignored
    /// files; with no list, all of it goes to the trash.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn trash_deletes_build_output_but_not_what_holds_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "k").unwrap();
        let make = |wt: &Path| {
            std::fs::write(
                wt.join(".gitignore"),
                "target/\nbuild/\nnode_modules\n.env\n",
            )
            .unwrap();
            commit_all(wt, "ignore");
            for d in ["target/debug", "crates/x/target", "build/dep"] {
                std::fs::create_dir_all(wt.join(d)).unwrap();
            }
            std::fs::write(wt.join("target/debug/bin"), vec![1u8; 32 << 10]).unwrap();
            std::fs::write(wt.join("crates/x/target/o"), "o").unwrap();
            init_repo(&wt.join("build/dep"));
            std::fs::write(wt.join(".env"), "KEY=1").unwrap();
            std::os::unix::fs::symlink(&outside, wt.join("node_modules")).unwrap();
        };
        let wt = new_wt(&g, &repo, &dir.path().join("wt1"), "twr/b1").await;
        make(&wt);
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(inv.build_output.items, ["crates/x/target/", "target/"]);
        assert!(inv.build_output_bytes >= 32 << 10);
        let groups: Vec<&str> = inv.ignored_groups.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(groups, [".env", "node_modules", "build/"]);

        let out = to_trash(&g, &repo, &wt, Some("twr/b1")).await.unwrap();
        let moved = out.dir.unwrap().join("worktree");
        for gone in ["target", "crates/x/target"] {
            assert!(!moved.join(gone).exists(), "{gone}");
        }
        assert!(moved.join("crates/x").is_dir());
        for kept in [".env", "build/dep/.git", "node_modules"] {
            assert!(moved.join(kept).symlink_metadata().is_ok(), "{kept}");
        }
        assert!(outside.join("keep.txt").exists());
        let m = &list(&g).await[0].manifest;
        assert_eq!(m.build_output, ["crates/x/target/", "target/"]);
        assert!(
            out.bytes < inv.bytes + inv.build_output_bytes,
            "{}",
            out.bytes
        );

        // No list (an older hub's request): everything goes to the trash.
        let none = g.clone().with_park_disposable(Vec::new());
        let wt = new_wt(&none, &repo, &dir.path().join("wt2"), "twr/b2").await;
        make(&wt);
        let out = to_trash(&none, &repo, &wt, Some("twr/b2")).await.unwrap();
        let moved = out.dir.unwrap().join("worktree");
        assert!(moved.join("target/debug/bin").exists());
        assert!(moved.join("crates/x/target/o").exists());
    }

    /// A bare repository whose object store is a symlink (a shared one) holds a repository
    /// all the same: the ignored `target/` holding it is no build output, and it goes to the
    /// trash and comes back with a restore, its `HEAD`, refs and config with it.
    #[tokio::test]
    async fn a_bare_repository_with_a_shared_object_store_is_not_build_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/b").await;
        std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
        commit_all(&wt, "ignore");
        std::fs::create_dir(wt.join("target")).unwrap();
        let bare = wt.join("target/backup.git");
        run(
            &root,
            &[
                "clone",
                "--bare",
                "-q",
                repo.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        std::fs::rename(bare.join("objects"), root.join("store")).unwrap();
        std::os::unix::fs::symlink(root.join("store"), bare.join("objects")).unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert!(inv.build_output.is_empty(), "{:?}", inv.build_output);
        assert_eq!(inv.ignored.items, ["target/"]);

        let out = to_trash(&g, &repo, &wt, Some("twr/b")).await.unwrap();
        let kept = |wt: &Path| {
            let b = wt.join("target/backup.git");
            for f in ["HEAD", "config", "packed-refs"] {
                assert!(b.join(f).is_file(), "{f} in {}", wt.display());
            }
            assert!(b.join("objects").symlink_metadata().unwrap().is_symlink());
        };
        kept(&out.dir.clone().unwrap().join("worktree"));
        assert!(list(&g).await[0].manifest.build_output.is_empty());
        let restored = restore_at(&g, &out.item.unwrap(), "twr/b2", &root.join("wt2"))
            .await
            .unwrap();
        kept(&restored.worktree);
    }

    /// Build output a deletion can neither delete nor put back (a repository made in it once it
    /// was searched, and its place taken meanwhile) is not lost: the item keeps it apart, the
    /// outcome and the manifest name it, and a restore puts it back where it was, or says where
    /// it is when that place is taken.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn build_output_that_cannot_go_back_is_kept_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root).with_step_hook(|step, at| {
            match step {
                Step::BuildOutputSearched => {
                    std::fs::create_dir_all(at.join("sub/.git"))?;
                    std::fs::write(at.join("sub/.git/HEAD"), "ref: refs/heads/main\n")?;
                }
                Step::BuildOutputPuttingBack => {
                    std::fs::create_dir(at)?;
                    std::fs::write(at.join("new.txt"), "new")?;
                }
                _ => {}
            }
            Ok(())
        });
        let repo = init_repo(&root.join("repo"));
        for (i, taken) in [true, false].into_iter().enumerate() {
            let branch = format!("twr/w{i}");
            let wt = new_wt(&g, &repo, &root.join(format!("wt{i}")), &branch).await;
            std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
            commit_all(&wt, "ignore");
            std::fs::create_dir_all(wt.join("target/sub")).unwrap();
            std::fs::write(wt.join("target/sub/o"), "o").unwrap();
            let out = to_trash(&g, &repo, &wt, Some(&branch)).await.unwrap();
            assert_eq!(out.build_output_aside, ["target/"]);
            let item = out.item.unwrap();
            let items = list(&g).await;
            let m = &items
                .iter()
                .find(|i| i.manifest.item == item)
                .unwrap()
                .manifest;
            let kept: Vec<(&str, &str)> = m
                .build_output_aside
                .iter()
                .map(|k| (k.path.as_str(), k.at.as_str()))
                .collect();
            assert_eq!(kept, [("target/", ".twr-build-output.0/0")]);
            assert!(m.build_output.is_empty());
            assert!(
                m.recover.contains(".twr-build-output.0/0 (was target/)"),
                "{}",
                m.recover
            );
            let saved = out.dir.unwrap().join("worktree");
            assert!(saved.join(".twr-build-output.0/0/sub/.git/HEAD").exists());
            assert!(saved.join("target/new.txt").exists());
            if !taken {
                std::fs::remove_dir_all(saved.join("target")).unwrap();
            }
            let r = restore_at(&g, &item, &format!("twr/r{i}"), &root.join(format!("r{i}")))
                .await
                .unwrap();
            let back = r.worktree.join("target/sub");
            let aside = r.worktree.join(".twr-build-output.0/0/sub");
            let note = r.notes.iter().find(|n| n.contains("cannot go back"));
            if taken {
                assert!(
                    note.is_some_and(|n| n.contains(".twr-build-output.0/0")),
                    "{:?}",
                    r.notes
                );
                assert!(aside.join(".git/HEAD").exists() && aside.join("o").exists());
            } else {
                assert!(note.is_none(), "{:?}", r.notes);
                assert!(back.join(".git/HEAD").exists() && back.join("o").exists());
                assert!(!r.worktree.join(".twr-build-output.0").exists());
            }
        }
    }

    /// A deletion stopped once build output was moved aside (the hub or runner gone; here a
    /// hook's error), with a repository in it by then: the next deletion first puts it back
    /// where it was, from what the worktree's record wrote down (nothing in the worktree
    /// counts), and keeps it there (it holds a repository); when its place is taken by then, it
    /// keeps it aside in its own item instead. Either way a restore has it where it was.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn build_output_a_stopped_deletion_left_aside_goes_back() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let stop = Arc::new(AtomicBool::new(true));
        let once = Arc::clone(&stop);
        let g = with_trash(&LocalGit::default(), &root).with_step_hook(move |step, at| {
            if step == Step::BuildOutputSearched && once.swap(false, Ordering::SeqCst) {
                std::fs::create_dir_all(at.join("dep/.git"))?;
                std::fs::write(at.join("dep/.git/HEAD"), "ref: refs/heads/main\n")?;
                std::fs::write(at.join("dep/src.rs"), "fn f() {}\n")?;
                return Err(std::io::Error::other("stopped here"));
            }
            Ok(())
        });
        let repo = init_repo(&root.join("repo"));
        for (i, taken) in [false, true].into_iter().enumerate() {
            stop.store(true, Ordering::SeqCst);
            let branch = format!("twr/s{i}");
            let wt = new_wt(&g, &repo, &root.join(format!("wt{i}")), &branch).await;
            std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
            commit_all(&wt, "ignore");
            std::fs::create_dir_all(wt.join("target")).unwrap();
            std::fs::write(wt.join("target/o"), "o").unwrap();

            let e = to_trash(&g, &repo, &wt, Some(&branch)).await.unwrap_err();
            assert!(format!("{e:#}").contains("stopped here"), "{e:#}");
            assert!(wt.join(".twr-build-output.0/0/dep/.git/HEAD").exists());
            assert!(!wt.join("target").exists());
            if taken {
                std::fs::write(wt.join("target"), "new").unwrap();
            }

            let out = to_trash(&g, &repo, &wt, Some(&branch)).await.unwrap();
            let item = out.item.unwrap();
            let saved = out.dir.unwrap().join("worktree");
            if taken {
                assert_eq!(out.build_output_aside, ["target/"]);
                let owner = saved.join(".twr-build-output.0/owner");
                assert_eq!(std::fs::read_to_string(owner).unwrap(), item);
                std::fs::remove_file(saved.join("target")).unwrap();
            } else {
                assert!(out.build_output_aside.is_empty());
                assert!(saved.join("target/dep/.git/HEAD").exists());
                assert!(!saved.join(".twr-build-output.0").exists());
            }
            let r = restore_at(&g, &item, &format!("twr/r{i}"), &root.join(format!("r{i}")))
                .await
                .unwrap();
            for f in ["target/dep/.git/HEAD", "target/dep/src.rs", "target/o"] {
                assert!(r.worktree.join(f).exists(), "{taken}: {f}");
            }
            assert!(!r.worktree.join(".twr-build-output.0").exists());
        }
    }

    /// The journal (and an aside directory's owner) is written to a new file of its own and
    /// renamed over the old one: a symlink where its temporary file would go is not followed (an
    /// outside file stays as it was), a FIFO there blocks nothing, and a symlink in the
    /// journal's place is replaced, not written through.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_journal_is_written_without_following_anything() {
        use std::os::unix::fs::{symlink, FileTypeExt};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        let fifo = |p: &Path| {
            let made = std::process::Command::new("mkfifo")
                .arg(p)
                .status()
                .unwrap();
            assert!(made.success());
        };
        for (i, planted) in ["symlink", "fifo"].into_iter().enumerate() {
            let (wt, record) = (root.join(format!("wt{i}")), root.join(format!("record{i}")));
            std::fs::create_dir_all(wt.join("target")).unwrap();
            std::fs::write(wt.join("target/o"), "o").unwrap();
            std::fs::create_dir(&record).unwrap();
            let journal = record.join(trash::JOURNAL);
            let tmp = record.join(format!("{}.tmp", trash::JOURNAL));
            match planted {
                "symlink" => symlink(&sentinel, &tmp).unwrap(),
                _ => fifo(&tmp),
            }
            symlink(&sentinel, &journal).unwrap();
            let stop = |step: Step, _: &Path| match step {
                Step::BuildOutputSearched => Err(std::io::Error::other("stopped")),
                _ => Ok(()),
            };
            let (build, mut budget) = (["target/".to_string()], NESTED_SEARCH_MAX);
            let e = trash::drop_build_output(
                &wt,
                &build,
                "item",
                &journal,
                Vec::new(),
                &mut budget,
                &stop,
            )
            .unwrap_err();
            assert_eq!(e.to_string(), "stopped");
            assert_eq!(
                std::fs::read_to_string(&sentinel).unwrap(),
                "keep",
                "{planted}"
            );
            let meta = std::fs::symlink_metadata(&journal).unwrap();
            assert!(meta.is_file(), "{planted}");
            let j: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
            assert_eq!(j["staged"][0]["path"], "target/", "{j}");
            let left = std::fs::symlink_metadata(&tmp).unwrap().file_type();
            assert!(left.is_symlink() || left.is_fifo(), "{planted}");
        }
    }

    /// A takeover of build output a stopped deletion left aside can itself stop anywhere and be
    /// taken up again, and loses no entry: the journal names the new item (the old one as
    /// previous) before any owner is rewritten, each aside directory's owner is rewritten once
    /// (two slots in one directory), and an error writing it fails the takeover with the
    /// journal intact. A symlink where the owner's temporary file would go is not followed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_takeover_can_stop_anywhere_and_loses_nothing() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, "keep").unwrap();
        // A work tree whose deletion by item `A` stopped with `target/` and `out/` aside in one
        // directory, both places taken since.
        let tree = |name: &str| {
            let (wt, record) = (root.join(name), root.join(format!("{name}.record")));
            let aside = wt.join(".twr-build-output.0");
            for slot in ["0", "1"] {
                std::fs::create_dir_all(aside.join(slot)).unwrap();
                std::fs::write(aside.join(slot).join("f"), slot).unwrap();
            }
            std::fs::write(aside.join("owner"), "A").unwrap();
            symlink(&sentinel, aside.join("owner.tmp")).unwrap();
            std::fs::write(wt.join("target"), "taken").unwrap();
            std::fs::write(wt.join("out"), "taken").unwrap();
            std::fs::create_dir(&record).unwrap();
            let journal = record.join(trash::JOURNAL);
            let j = serde_json::json!({"item": "A", "staged": [
                {"path": "target/", "at": ".twr-build-output.0/0"},
                {"path": "out/", "at": ".twr-build-output.0/1"},
            ]});
            std::fs::write(&journal, j.to_string()).unwrap();
            (wt, aside, journal)
        };
        let journal_of = |journal: &Path| -> serde_json::Value {
            serde_json::from_slice(&std::fs::read(journal).unwrap()).unwrap()
        };
        let owner = |aside: &Path| std::fs::read_to_string(aside.join("owner")).unwrap();
        let go = |_: Step, _: &Path| Ok(());
        let stop_at = |at: Step| {
            move |step: Step, _: &Path| {
                if step == at {
                    Err(std::io::Error::other("stopped"))
                } else {
                    Ok(())
                }
            }
        };
        let both = |carried: Vec<twr_core::git::KeptAside>| {
            let paths: Vec<String> = carried.into_iter().map(|k| k.path).collect();
            assert_eq!(paths, ["target/", "out/"]);
        };

        // Two slots in one directory: both taken over, the owner rewritten once.
        let (wt, aside, journal) = tree("a");
        both(trash::reconcile_staged(&wt, &journal, "B", &go).unwrap());
        assert_eq!(owner(&aside), "B");
        let j = journal_of(&journal);
        assert_eq!(
            (&j["item"], j.get("previous")),
            (&serde_json::json!("B"), None),
            "{j}"
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");

        // Stopped before the owner is rewritten, after it, or by an error writing it: the
        // journal names B with A as previous, and the next takeover takes either.
        for (i, at) in [Step::BuildOutputTakingOver, Step::BuildOutputTakenOver]
            .into_iter()
            .enumerate()
        {
            let (wt, aside, journal) = tree(&format!("b{i}"));
            assert!(trash::reconcile_staged(&wt, &journal, "B", &stop_at(at)).is_err());
            let j = journal_of(&journal);
            assert_eq!(
                (&j["item"], &j["previous"]),
                (&serde_json::json!("B"), &serde_json::json!(["A"])),
                "{j}"
            );
            assert_eq!(j["staged"].as_array().unwrap().len(), 2, "{j}");
            assert_eq!(owner(&aside), if i == 0 { "A" } else { "B" });
            both(trash::reconcile_staged(&wt, &journal, "C", &go).unwrap());
            assert_eq!(owner(&aside), "C");
        }
        let (wt, aside, journal) = tree("c");
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o500)).unwrap();
        let e = trash::reconcile_staged(&wt, &journal, "B", &go).unwrap_err();
        assert!(format!("{e:#}").contains("writing the owner"), "{e:#}");
        std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o700)).unwrap();
        let j = journal_of(&journal);
        assert_eq!(j["staged"].as_array().unwrap().len(), 2, "{j}");
        both(trash::reconcile_staged(&wt, &journal, "C", &go).unwrap());
        assert_eq!(owner(&aside), "C");

        // What was taken over goes back with a restore once its places are free.
        for f in ["target", "out"] {
            std::fs::remove_file(wt.join(f)).unwrap();
        }
        let kept = trash::reconcile_staged(&wt, &journal, "D", &go).unwrap();
        assert!(kept.is_empty() && !journal.exists());
        for f in ["target/f", "out/f"] {
            assert!(wt.join(f).exists(), "{f}");
        }
    }

    /// `mountinfo` names each mount point in its fifth field, with octal escapes for a space,
    /// a tab, a newline and a backslash; those inside a work tree are below its top level, by
    /// whole components (`target-old` is not under `target`, the work tree itself is not in
    /// it).
    #[test]
    fn mount_points_are_read_from_mountinfo() {
        let text = b"22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
            36 22 8:1 /data /w/target/shared rw - ext4 /dev/sda1 rw\n\
            37 22 0:31 / /w/sp\\040ace rw shared:2 - tmpfs t rw\n\
            38 22 0:32 / /w-old/target rw - tmpfs t rw\n\
            39 22 0:33 / /w/tab\\011and\\134back\\012 rw - tmpfs t rw\n\
            garbage\n";
        let want: Vec<PathBuf> = [
            "/",
            "/w/target/shared",
            "/w/sp ace",
            "/w-old/target",
            "/w/tab\tand\\back\n",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(trash::mount_points(text), want);

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = root.join("w");
        std::fs::create_dir(&wt).unwrap();
        let line = |p: &Path| format!("36 22 8:1 / {} rw - ext4 /dev/sda1 rw\n", p.display());
        let text: String = [
            root.join("w/vendor"),
            root.join("w/target/shared"),
            root.join("w"),
            root.join("w-old/x"),
            root.clone(),
            PathBuf::from("/"),
        ]
        .iter()
        .map(|p| line(p))
        .collect();
        let _fake = FakeMounts::read(&wt, Some(text));
        assert_eq!(
            trash::mounts_inside(&wt).unwrap().unwrap(),
            ["target/shared", "vendor"]
        );
    }

    /// `statfs`'s `f_mntonname` buffers (macOS's `getfsstat`) name mount points up to their
    /// first NUL (a buffer without one, whole); one that is not absolute is left out. Those
    /// inside a work tree are told as from `mountinfo`.
    #[test]
    fn mount_points_are_read_from_c_strings() {
        use std::ffi::c_char;
        fn buf(s: &[u8]) -> [c_char; 20] {
            let mut b = [0 as c_char; 20];
            for (i, c) in s.iter().enumerate() {
                b[i] = *c as c_char;
            }
            b
        }
        let bufs = [
            buf(b"/"),
            buf(b"/w/target/shared"),
            buf(b"/w/sp ace\0junk"),
            buf(b"map auto_home"),
            buf(b""),
            buf(b"/Volumes/\xc3\xa9t\xff"),
            buf(b"/exactly-twenty-byte"),
        ];
        let names = trash::mount_names(bufs.iter().map(|b| &b[..]));
        let want: Vec<PathBuf> = [
            &b"/"[..],
            b"/w/target/shared",
            b"/w/sp ace",
            b"/Volumes/\xc3\xa9t\xff",
            b"/exactly-twenty-byte",
        ]
        .iter()
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        .collect();
        assert_eq!(names, want);

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let wt = root.join("w");
        std::fs::create_dir(&wt).unwrap();
        let points = vec![
            root.join("w/vendor"),
            root.join("w/target/shared"),
            root.join("w/target/shared"),
            root.join("w"),
            root.join("w-old/x"),
            root.clone(),
            PathBuf::from("/"),
        ];
        let _fake = FakeMounts::set(&wt, trash::MountList::Points(points));
        assert_eq!(
            trash::mounts_inside(&wt).unwrap().unwrap(),
            ["target/shared", "vendor"]
        );
    }

    /// While it lives, the mount list of work trees under `under` is `list`
    /// ([`trash::MountList`]).
    struct FakeMounts(PathBuf);

    impl FakeMounts {
        fn set(under: &Path, list: trash::MountList) -> FakeMounts {
            let under = std::fs::canonicalize(under).unwrap();
            trash::MOUNTINFO.lock().unwrap().push((under.clone(), list));
            FakeMounts(under)
        }

        /// `/proc/self/mountinfo` reads `text` (`None`: it cannot be read).
        fn read(under: &Path, text: Option<String>) -> FakeMounts {
            let list = match text {
                Some(t) => trash::MountList::Read(t.into_bytes()),
                None => trash::MountList::Unreadable,
            };
            FakeMounts::set(under, list)
        }
    }

    impl Drop for FakeMounts {
        fn drop(&mut self) {
            trash::MOUNTINFO
                .lock()
                .unwrap()
                .retain(|(u, _)| *u != self.0);
        }
    }

    /// A worktree with a file system mounted in it is not deleted at all (moving, copying or
    /// deleting through the mount point would take that file system's files): not one with a
    /// mount at a directory holding an ignored file (`vendor/.DS_Store`), nor one in build
    /// output (`target/shared`), nor one whose mount points cannot be told; nothing is made,
    /// moved or deleted, and the inventory says so. A mount outside it, or at the worktree
    /// itself, stops nothing.
    #[tokio::test]
    async fn a_worktree_with_a_mount_point_in_it_is_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let line = |p: &Path| format!("36 22 8:1 / {} rw - ext4 /dev/sda1 rw\n", p.display());
        for (i, (at, refused)) in [
            (
                Some("vendor"),
                Some("mounted in it at vendor: unmount it first"),
            ),
            (
                Some("target/shared"),
                Some("mounted in it at target/shared"),
            ),
            (None, Some("where file systems are mounted cannot be read")),
            (Some(""), None),
            (Some("/elsewhere"), None),
        ]
        .into_iter()
        .enumerate()
        {
            let branch = format!("twr/m{i}");
            let wt = new_wt(&g, &repo, &root.join(format!("wt{i}")), &branch).await;
            std::fs::write(wt.join(".gitignore"), ".DS_Store\ntarget/\n").unwrap();
            std::fs::create_dir_all(wt.join("vendor")).unwrap();
            std::fs::write(wt.join("vendor/lib.rs"), "x").unwrap();
            commit_all(&wt, "vendor");
            std::fs::write(wt.join("vendor/.DS_Store"), "d").unwrap();
            std::fs::create_dir_all(wt.join("target/shared")).unwrap();
            std::fs::write(wt.join("target/shared/keep"), "k").unwrap();
            // `/elsewhere`: beside the worktree (mountinfo names resolved paths).
            let text = at.map(|a| match a.strip_prefix('/') {
                Some(beside) => line(&root.join(beside)),
                None => line(&wt.join(a)),
            });
            let _fake = FakeMounts::read(&wt, text);
            let inv = g
                .worktree_inventory(repo.clone(), wt.clone())
                .await
                .unwrap();
            let out = to_trash(&g, &repo, &wt, Some(&branch)).await;
            match refused {
                Some(why) => {
                    let e = format!("{:#}", out.unwrap_err());
                    assert!(e.contains(why) && e.contains("nothing is touched"), "{e}");
                    for f in ["vendor/.DS_Store", "vendor/lib.rs", "target/shared/keep"] {
                        assert!(wt.join(f).exists(), "{at:?}: {f}");
                    }
                    assert!(branch_exists(&repo, &branch));
                    assert!(list(&g).await.iter().all(|i| i.manifest.worktree != wt));
                    match at {
                        Some(a) => assert_eq!(inv.mounts, [a]),
                        None => assert!(inv.mounts_unknown),
                    }
                }
                None => {
                    out.unwrap();
                    assert!(inv.mounts.is_empty() && !inv.mounts_unknown, "{at:?}");
                    assert!(!wt.exists());
                }
            }
        }
    }

    /// Where there is no mount list (any system but Linux and macOS; here the list made
    /// unavailable), mount points cannot be told: no build output is listed or deleted, and all
    /// of it goes to the trash with the worktree, an ignored file under a directory that could
    /// be a mount point (`vendor/.DS_Store`) too, as before build output was deleted, and comes
    /// back with a restore.
    #[tokio::test]
    async fn without_a_mount_list_no_build_output_is_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/n").await;
        std::fs::write(wt.join(".gitignore"), ".DS_Store\ntarget/\n").unwrap();
        std::fs::create_dir_all(wt.join("vendor")).unwrap();
        std::fs::write(wt.join("vendor/lib.rs"), "x").unwrap();
        commit_all(&wt, "vendor");
        std::fs::write(wt.join("vendor/.DS_Store"), "d").unwrap();
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target/o"), "o").unwrap();
        let disposable = vec![".DS_Store".to_string(), "target/".to_string()];
        let g = g.with_park_disposable(disposable);
        let _fake = FakeMounts::set(&wt, trash::MountList::None);
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert!(inv.build_output.is_empty() && inv.build_output_bytes == 0);
        assert!(inv.mounts.is_empty() && !inv.mounts_unknown);
        let groups: Vec<&str> = inv.ignored_groups.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(groups, ["target/", "vendor/"]);

        let out = to_trash(&g, &repo, &wt, Some("twr/n")).await.unwrap();
        let saved = out.dir.unwrap().join("worktree");
        for f in ["target/o", "vendor/.DS_Store", "vendor/lib.rs"] {
            assert!(saved.join(f).exists(), "{f}");
        }
        let items = list(&g).await;
        assert!(items[0].manifest.build_output.is_empty());
        // And it comes back with a restore.
        let r = restore_at(&g, &out.item.unwrap(), "twr/n2", &root.join("wt2"))
            .await
            .unwrap();
        for f in ["target/o", "vendor/.DS_Store"] {
            assert!(r.worktree.join(f).exists(), "{f}");
        }
    }

    /// A bind mount in a worktree (a directory of the same file system mounted in its build
    /// output): the deletion is refused, and what the mount shows is untouched, permissions
    /// and all. Needs root (it mounts), so it is run by hand: `cargo test -p twr-runner --
    /// --ignored bind_mount`.
    #[tokio::test]
    #[ignore = "needs root: it makes a bind mount"]
    async fn a_bind_mount_in_a_worktree_refuses_its_deletion() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/b").await;
        std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
        commit_all(&wt, "ignore");
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("ro")).unwrap();
        std::fs::write(outside.join("ro/keep"), "k").unwrap();
        std::fs::set_permissions(outside.join("ro"), std::fs::Permissions::from_mode(0o555))
            .unwrap();
        let shared = wt.join("target/shared");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(wt.join("target/o"), "o").unwrap();
        let mounted = std::process::Command::new("mount")
            .arg("--bind")
            .arg(&outside)
            .arg(&shared)
            .status()
            .unwrap();
        assert!(mounted.success());
        let out = to_trash(&g, &repo, &wt, Some("twr/b")).await;
        let _ = std::process::Command::new("umount").arg(&shared).status();
        let e = format!("{:#}", out.unwrap_err());
        assert!(e.contains("mounted in it at target/shared"), "{e}");
        assert!(outside.join("ro/keep").exists() && wt.join("target/o").exists());
        let mode = std::fs::metadata(outside.join("ro"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o555);
    }

    /// Files that merely look like build output kept aside (a tracked
    /// `.twr-build-output.0/0/source.rs` and `0.from`) are the worktree's own: with no build
    /// output kept aside, a deletion and a restore leave them as they are.
    #[tokio::test]
    async fn files_that_look_like_kept_build_output_round_trip_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/f").await;
        let files = [
            (".twr-build-output.0/0/source.rs", "fn main() {}\n"),
            (".twr-build-output.0/0.from", "recovered/"),
        ];
        for (f, body) in files {
            std::fs::create_dir_all(wt.join(f).parent().unwrap()).unwrap();
            std::fs::write(wt.join(f), body).unwrap();
        }
        commit_all(&wt, "fixtures");
        let out = to_trash(&g, &repo, &wt, Some("twr/f")).await.unwrap();
        assert!(out.build_output_aside.is_empty());
        let r = restore_at(&g, &out.item.unwrap(), "twr/f2", &root.join("wt2"))
            .await
            .unwrap();
        for (f, body) in files {
            assert_eq!(std::fs::read_to_string(r.worktree.join(f)).unwrap(), body);
        }
        assert!(!r.worktree.join("recovered").exists());
        assert!(r.notes.is_empty(), "{:?}", r.notes);
    }

    /// A restore puts back only the build output its item kept aside, from a directory whose
    /// owner names the item, through real directories only (made when missing): never through
    /// a symlink on the way (`crates` -> outside), never with an owner that is a symlink or a
    /// FIFO (none is read, nothing blocks), and a `.from` is never read (the manifest says
    /// where it goes; a symlink to a FIFO there blocks nothing).
    #[cfg(target_os = "linux")]
    #[test]
    fn putting_build_output_back_follows_no_symlink_and_reads_no_odd_file() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("x")).unwrap();
        let fifo = |p: &Path| {
            let made = std::process::Command::new("mkfifo")
                .arg(p)
                .status()
                .unwrap();
            assert!(made.success());
        };
        fifo(&root.join("fifo"));
        // A work tree `name` with `crates/x/target/` kept aside by item `item`.
        let tree = |name: &str| {
            let wt = root.join(name);
            let aside = wt.join(".twr-build-output.0");
            std::fs::create_dir_all(aside.join("0/sub")).unwrap();
            std::fs::write(aside.join("0/sub/o"), "o").unwrap();
            std::fs::write(aside.join("owner"), "item").unwrap();
            std::fs::write(aside.join("0.from"), "crates/x/target/").unwrap();
            (wt, aside)
        };
        let kept = [twr_core::git::KeptAside {
            path: "crates/x/target/".into(),
            at: ".twr-build-output.0/0".into(),
        }];
        let stays = |aside: &Path, notes: &[String], why: &str| {
            assert_eq!(notes.len(), 1, "{notes:?}");
            assert!(notes[0].contains(why), "{why}: {notes:?}");
            assert!(aside.join("0/sub/o").exists());
        };

        let (wt, aside) = tree("a");
        assert!(trash::put_back_aside(&wt, "item", &kept).is_empty());
        assert!(wt.join("crates/x/target/sub/o").exists() && !aside.exists());

        let (wt, aside) = tree("b");
        symlink(&outside, wt.join("crates")).unwrap();
        let notes = trash::put_back_aside(&wt, "item", &kept);
        stays(&aside, &notes, "crates on the way is not a directory");
        assert!(!outside.join("x/target").exists());

        let (wt, aside) = tree("c");
        let notes = trash::put_back_aside(&wt, "another", &kept);
        stays(&aside, &notes, "not made by the deletion of this item");

        let (wt, aside) = tree("d");
        std::fs::remove_file(aside.join("owner")).unwrap();
        symlink(root.join("fifo"), aside.join("owner")).unwrap();
        let notes = trash::put_back_aside(&wt, "item", &kept);
        stays(&aside, &notes, "cannot go back");

        let (wt, aside) = tree("e");
        std::fs::remove_file(aside.join("owner")).unwrap();
        fifo(&aside.join("owner"));
        let notes = trash::put_back_aside(&wt, "item", &kept);
        stays(&aside, &notes, "not a regular file");

        let (wt, aside) = tree("f");
        std::fs::remove_file(aside.join("0.from")).unwrap();
        symlink(root.join("fifo"), aside.join("0.from")).unwrap();
        assert!(trash::put_back_aside(&wt, "item", &kept).is_empty());
        assert!(wt.join("crates/x/target/sub/o").exists() && !aside.exists());

        // Nothing kept, or kept somewhere that is no aside slot: nothing moves.
        let (wt, aside) = tree("g");
        assert!(trash::put_back_aside(&wt, "item", &[]).is_empty());
        let odd = [twr_core::git::KeptAside {
            path: "crates/x/target/".into(),
            at: "src/0".into(),
        }];
        let notes = trash::put_back_aside(&wt, "item", &odd);
        stays(&aside, &notes, "not where build output is kept");
    }

    /// Deleting build output follows no symlink and deletes no repository, whatever changed
    /// since the inventory listed it: an ancestor, the entry itself or a directory in it
    /// swapped for a symlink, a repository moved into it, or one made in it once it was moved
    /// aside and searched. What it does not delete is put back; read-only directories in it
    /// go, but one it cannot search keeps all of it.
    #[cfg(target_os = "linux")]
    #[test]
    fn deleting_build_output_follows_no_symlink_and_keeps_repositories() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let outside = root.join("outside");
        for d in ["x/target", "target", "sub"] {
            std::fs::create_dir_all(outside.join(d)).unwrap();
            std::fs::write(outside.join(d).join("keep"), "k").unwrap();
        }
        let list = vec!["target/".to_string()];
        // A work tree `name` with the files `files`, and its build output among `entries`.
        let tree = |name: &str, files: &[&str], entries: &[&str]| {
            let wt = root.join(name);
            for f in files {
                std::fs::create_dir_all(wt.join(f).parent().unwrap()).unwrap();
                std::fs::write(wt.join(f), "o").unwrap();
            }
            let entries: Vec<String> = entries.iter().map(|e| e.to_string()).collect();
            let mut budget = NESTED_SEARCH_MAX;
            let build = trash::build_output(&wt, &entries, &list, &mut budget);
            assert_eq!(build, entries);
            (wt, build)
        };
        let drop =
            |wt: &Path, build: &[String], hook: &dyn Fn(Step, &Path) -> std::io::Result<()>| {
                let mut budget = NESTED_SEARCH_MAX;
                let journal = wt.with_extension("journal");
                let dropped = trash::drop_build_output(
                    wt,
                    build,
                    "item",
                    &journal,
                    Vec::new(),
                    &mut budget,
                    hook,
                )
                .unwrap();
                // The journal keeps only what stays aside.
                assert_eq!(journal.exists(), !dropped.aside.is_empty());
                let aside: Vec<_> = std::fs::read_dir(wt)
                    .unwrap()
                    .flatten()
                    .filter(|e| {
                        e.file_name()
                            .to_string_lossy()
                            .starts_with(".twr-build-output")
                    })
                    .collect();
                assert_eq!(aside.is_empty(), dropped.aside.is_empty(), "{aside:?}");
                dropped
            };
        let none = |_: Step, _: &Path| Ok(());
        // `f` once an entry was moved aside and searched.
        fn searched(f: &dyn Fn(&Path)) -> impl Fn(Step, &Path) -> std::io::Result<()> + '_ {
            move |step, at| {
                if step == Step::BuildOutputSearched {
                    f(at)
                }
                Ok(())
            }
        }
        let outside_kept = || {
            for d in ["x/target", "target", "sub"] {
                assert!(outside.join(d).join("keep").exists(), "{d}");
            }
        };

        // An ancestor swapped for a symlink.
        let (wt, build) = tree("a", &["crates/x/target/o"], &["crates/x/target/"]);
        std::fs::rename(wt.join("crates"), wt.join("real")).unwrap();
        symlink(&outside, wt.join("crates")).unwrap();
        assert!(drop(&wt, &build, &none).deleted.is_empty());
        outside_kept();
        assert!(wt.join("real/x/target/o").exists());

        // The entry itself.
        let (wt, build) = tree("b", &["target/o"], &["target/"]);
        std::fs::rename(wt.join("target"), wt.join("real")).unwrap();
        symlink(outside.join("target"), wt.join("target")).unwrap();
        assert!(drop(&wt, &build, &none).deleted.is_empty());
        outside_kept();
        assert!(wt.join("target").symlink_metadata().unwrap().is_symlink());

        // A directory in it, once it was moved aside: the symlink goes, not what it leads to.
        let (wt, build) = tree("c", &["target/sub/o", "target/o"], &["target/"]);
        let swap = |at: &Path| {
            std::fs::remove_dir_all(at.join("sub")).unwrap();
            symlink(outside.join("sub"), at.join("sub")).unwrap();
        };
        assert_eq!(drop(&wt, &build, &searched(&swap)).deleted, build);
        outside_kept();
        assert!(!wt.join("target").exists());

        // A repository moved into it after the inventory: never moved aside.
        let (wt, build) = tree("d", &["target/o"], &["target/"]);
        init_repo(&wt.join("target/dep"));
        let unmoved = |step: Step, _: &Path| {
            assert_ne!(step, Step::BuildOutputOpened);
            Ok(())
        };
        assert!(drop(&wt, &build, &unmoved).deleted.is_empty());
        assert!(wt.join("target/dep/.git").is_dir() && wt.join("target/o").exists());

        // One made in it once it was searched, a work tree or a bare one: deleting stops there,
        // and what is left goes back.
        let made: [&[&str]; 2] = [
            &["sub/.git/HEAD"],
            &["sub/b.git/HEAD", "sub/b.git/objects/o"],
        ];
        for (i, files) in made.into_iter().enumerate() {
            let (wt, build) = tree(&format!("e{i}"), &["target/sub/o"], &["target/"]);
            let make = |at: &Path| {
                for f in files {
                    std::fs::create_dir_all(at.join(f).parent().unwrap()).unwrap();
                    std::fs::write(at.join(f), "x").unwrap();
                }
            };
            let d = drop(&wt, &build, &searched(&make));
            assert!(d.deleted.is_empty() && d.aside.is_empty(), "{files:?}");
            for f in files {
                assert!(wt.join("target").join(f).exists(), "{f}");
            }
        }

        // Read-only directories go, and one made unreadable once it was searched. One that
        // cannot be searched keeps all of it.
        let mode = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
        };
        let (wt, build) = tree("f", &["target/ro/o", "target/locked/o"], &["target/"]);
        mode(&wt.join("target/ro"), 0o555);
        let lock = |at: &Path| mode(&at.join("locked"), 0);
        assert_eq!(drop(&wt, &build, &searched(&lock)).deleted, build);
        assert!(!wt.join("target").exists());
        let (wt, build) = tree("g", &["target/locked/o"], &["target/"]);
        mode(&wt.join("target/locked"), 0);
        assert!(drop(&wt, &build, &none).deleted.is_empty());
        mode(&wt.join("target/locked"), 0o700);
        assert!(wt.join("target/locked/o").exists());

        // The entry swapped for a symlink once it was opened, and something made where it was
        // before what was moved aside goes back: nothing goes over it, and what was moved
        // stays aside, written down where it was. Put back once that place is free.
        let (wt, build) = tree("h", &["target/o"], &["target/"]);
        let race = |step: Step, at: &Path| match step {
            Step::BuildOutputOpened => {
                std::fs::rename(at, wt.join("real")).unwrap();
                symlink(outside.join("target"), at).unwrap();
            }
            Step::BuildOutputPuttingBack => std::fs::write(at, "new").unwrap(),
            _ => {}
        };
        let race = |step: Step, at: &Path| {
            race(step, at);
            Ok(())
        };
        let d = drop(&wt, &build, &race);
        assert!(d.deleted.is_empty());
        let kept = [twr_core::git::KeptAside {
            path: "target/".into(),
            at: ".twr-build-output.0/0".into(),
        }];
        assert_eq!(d.aside, kept);
        assert_eq!(std::fs::read_to_string(wt.join("target")).unwrap(), "new");
        let aside = wt.join(".twr-build-output.0");
        assert!(aside.join("0").symlink_metadata().unwrap().is_symlink());
        assert_eq!(
            std::fs::read_to_string(aside.join("0.from")).unwrap(),
            "target/"
        );
        assert_eq!(
            std::fs::read_to_string(aside.join("owner")).unwrap(),
            "item"
        );
        outside_kept();
        let notes = trash::put_back_aside(&wt, "item", &kept);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("something is there"), "{notes:?}");
        assert!(aside.join("0").symlink_metadata().is_ok());
        std::fs::remove_file(wt.join("target")).unwrap();
        assert!(trash::put_back_aside(&wt, "item", &kept).is_empty());
        assert!(wt.join("target").symlink_metadata().unwrap().is_symlink());
        assert!(!aside.exists());
    }

    /// The items of `g`'s trash.
    async fn list(g: &LocalGit) -> Vec<TrashItem> {
        g.trash_list().await.unwrap()
    }

    /// The refs under `refs/twr-trash/` of `repo`.
    fn trash_refs(repo: &Path) -> String {
        run(
            repo,
            &["for-each-ref", "--format=%(refname)", "refs/twr-trash/"],
        )
    }

    /// A purge removes the items it names, or those older than its cutoff, with their refs,
    /// and nothing else: not another item, not a directory of the trash without a manifest
    /// naming it, not one named by no valid id.
    #[tokio::test]
    async fn purging_removes_only_trash_items() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let root = dir.path().join("worktrees").join("proj");
        let mut ids = Vec::new();
        for (name, at) in [("a", 1_000), ("b", 2_000), ("c", 3_000)] {
            let wt = new_wt(&g, &repo, &root.join(name), &format!("twr/{name}")).await;
            std::fs::write(wt.join("f.txt"), name).unwrap();
            commit_all(&wt, name);
            let out = to_trash_at(&g, &repo, &wt, Some(&format!("twr/{name}")), at)
                .await
                .unwrap();
            ids.push(out.item.unwrap());
        }
        let trash = dir.path().join("trash");
        std::fs::create_dir(trash.join("junk")).unwrap();
        std::fs::write(trash.join("junk").join("keep"), "x").unwrap();
        let bare = twr_core::event::new_id();
        std::fs::create_dir(trash.join(&bare)).unwrap();
        std::fs::write(trash.join(&bare).join("keep"), "x").unwrap();
        // A manifest naming another item.
        let lying = twr_core::event::new_id();
        std::fs::create_dir(trash.join(&lying)).unwrap();
        let mut m = list(&g).await[0].manifest.clone();
        m.item = ids[0].clone();
        std::fs::write(
            trash.join(&lying).join("manifest.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        assert_eq!(
            list(&g)
                .await
                .iter()
                .map(|i| i.manifest.item.clone())
                .collect::<Vec<_>>(),
            ids
        );

        let out = g
            .trash_purge(TrashPurge {
                items: vec![ids[1].clone(), bare.clone(), "../repo".into()],
                before_ms: Some(1_500),
            })
            .await
            .unwrap();
        assert_eq!(out.purged, [ids[1].clone(), ids[0].clone()]);
        assert_eq!(out.unknown, [bare.clone(), "../repo".to_string()]);
        assert!(out.failed.is_empty(), "{out:?}");
        let refs = trash_refs(&repo);
        assert!(!refs.contains(&ids[0]) && !refs.contains(&ids[1]), "{refs}");
        assert!(refs.contains(&ids[2]), "{refs}");
        assert!(!trash.join(&ids[0]).exists() && !trash.join(&ids[1]).exists());
        assert!(trash.join(&ids[2]).exists());
        for kept in [
            "junk/keep".to_string(),
            format!("{bare}/keep"),
            format!("{lying}/manifest.json"),
        ] {
            assert!(trash.join(&kept).exists(), "{kept}");
        }
        assert!(!repo.join(".git/logs/refs/twr-trash").join(&ids[0]).exists());
        // An item whose repository is gone still goes.
        std::fs::remove_dir_all(&repo).unwrap();
        let out = g
            .trash_purge(TrashPurge {
                before_ms: Some(u64::MAX),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out.purged, [ids[2].clone()]);
        assert!(list(&g).await.is_empty());
    }

    /// A move into the trash that fails part way (the worktree's parent may not be written)
    /// deletes nothing: the worktree and branch stay where they were, the item keeps the refs
    /// it made, and the thread can be moved again once the cause is gone.
    #[tokio::test]
    async fn a_move_that_fails_leaves_everything_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let parent = dir.path().join("worktrees").join("proj");
        let wt = new_wt(&g, &repo, &parent.join("t"), "twr/t").await;
        std::fs::write(wt.join("f.txt"), "f").unwrap();
        commit_all(&wt, "work");
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        let Some(ro) = ReadOnly::new(&parent) else {
            return;
        };
        let e = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("moving the worktree into the trash failed"),
            "{e:#}"
        );
        assert!(wt.join("f.txt").exists() && worktrees(&repo).contains(&wt));
        assert!(branch_exists(&repo, "twr/t"));
        let items = list(&g).await;
        assert_eq!(items.len(), 1);
        assert!(!items[0].manifest.complete && !items[0].manifest.worktree_moved);
        assert!(kept_by_trash(&repo, &items[0].manifest.item, &tip));
        drop(ro);
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        assert!(out.worktree_moved && out.branch_deleted && !wt.exists());
    }

    /// A thread whose worktree directory is gone moves its record to the trash even when it
    /// is locked or its `HEAD` reflog holds commits nothing else reaches, and its branch;
    /// another stale record, locked, stays.
    #[tokio::test]
    async fn trash_takes_a_locked_or_reflog_holding_stale_record() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let root = dir.path().join("worktrees").join("proj");
        let locked = new_wt(&g, &repo, &root.join("locked"), "twr/locked").await;
        run(&repo, &["worktree", "lock", locked.to_str().unwrap()]);
        let holding = new_wt(&g, &repo, &root.join("holding"), "twr/holding").await;
        run(&holding, &["checkout", "-q", "--detach"]);
        std::fs::write(holding.join("experiment.txt"), "work").unwrap();
        commit_all(&holding, "detached experiment");
        let experiment = run(&holding, &["rev-parse", "HEAD"]);
        run(&holding, &["checkout", "-q", "twr/holding"]);
        let other = new_wt(&g, &repo, &root.join("other"), "twr/other").await;
        run(&repo, &["worktree", "lock", other.to_str().unwrap()]);
        for wt in [&locked, &holding, &other] {
            std::fs::remove_dir_all(wt).unwrap();
        }
        for (wt, branch) in [(&locked, "twr/locked"), (&holding, "twr/holding")] {
            let out = to_trash(&g, &repo, wt, Some(branch)).await.unwrap();
            assert!(out.branch_deleted && !out.worktree_moved, "{branch}");
            assert!(!worktrees(&repo).contains(wt), "{branch}");
            assert!(!branch_exists(&repo, branch), "{branch}");
            if branch == "twr/holding" {
                gc_now(&repo);
                assert!(has_object(&repo, &experiment));
            }
        }
        assert!(worktrees(&repo).contains(&other));
        assert!(branch_exists(&repo, "twr/other"));
    }

    /// Makes a directory read-only (`0o500`) until dropped, so a tempdir can be cleaned up.
    struct ReadOnly(PathBuf);

    impl ReadOnly {
        /// `dir` read-only, with a file in it; `None` when this user can write it all the
        /// same (root): what may not be changed does not reproduce.
        fn new(dir: &Path) -> Option<ReadOnly> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join("f"), "f").unwrap();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
            let ro = ReadOnly(dir.to_path_buf());
            if std::fs::write(dir.join("probe"), "").is_ok() {
                eprintln!(
                    "skipped: {} is writable all the same (root?)",
                    dir.display()
                );
                return None;
            }
            Some(ro)
        }
    }

    impl Drop for ReadOnly {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        }
    }

    /// What is left of a worktree that is no worktree any more (its `.git` gone with the record
    /// still there, or its record gone with its `.git` still naming it) moves to the trash too,
    /// read-only directories and all. Another repository, or a directory in the repository, at
    /// the path is refused, and the branch stays.
    #[tokio::test]
    async fn trash_takes_what_is_left_of_a_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let root = dir.path().join("worktrees").join("proj");
        let wt = new_wt(&g, &repo, &root.join("t1"), "twr/t1").await;
        let ro = ReadOnly::new(&wt.join("ro"));
        let out = to_trash(&g, &repo, &wt, Some("twr/t1")).await.unwrap();
        assert!(out.worktree_moved && out.branch_deleted && !wt.exists());
        // Still read-only in the trash (its guard looks at the old path): a purge removes it.
        drop(ro);
        let purged = g
            .trash_purge(TrashPurge {
                items: vec![out.item.unwrap()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(purged.purged.len(), 1, "{purged:?}");

        // Its `.git` gone, its record still there (prunable).
        let wt3 = new_wt(&g, &repo, &root.join("t3"), "twr/t3").await;
        std::fs::remove_file(wt3.join(".git")).unwrap();
        assert!(worktrees(&repo).contains(&wt3));
        let out = to_trash(&g, &repo, &wt3, Some("twr/t3")).await.unwrap();
        assert!(out.worktree_moved && !wt3.exists());
        assert!(!worktrees(&repo).contains(&wt3) && !branch_exists(&repo, "twr/t3"));
        assert!(out.dir.unwrap().join("worktree/README.md").exists());

        // Its record gone, its `.git` still naming it.
        let wt4 = new_wt(&g, &repo, &root.join("t4"), "twr/t4").await;
        let admin = run(&wt4, &["rev-parse", "--absolute-git-dir"]);
        std::fs::remove_dir_all(&admin).unwrap();
        let out = to_trash(&g, &repo, &wt4, Some("twr/t4")).await.unwrap();
        assert!(out.worktree_moved && !wt4.exists() && !branch_exists(&repo, "twr/t4"));

        // Another repository at the path, or a plain directory in the repository: refused,
        // the branch stays.
        let wt5 = new_wt(&g, &repo, &root.join("t5"), "twr/t5").await;
        std::fs::remove_dir_all(&wt5).unwrap();
        init_repo(&wt5);
        let e = to_trash(&g, &repo, &wt5, Some("twr/t5")).await.unwrap_err();
        assert!(format!("{e:#}").contains("not a worktree of"), "{e:#}");
        let inside = repo.join("plain");
        std::fs::create_dir(&inside).unwrap();
        let e = to_trash(&g, &repo, &inside, Some("twr/t5"))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("top level"), "{e:#}");
        // `.git` naming a record of another repository.
        let other = init_repo(&dir.path().join("other"));
        let wt6 = new_wt(&g, &other, &root.join("t6"), "twr/t6").await;
        let admin = run(&wt6, &["rev-parse", "--absolute-git-dir"]);
        std::fs::remove_dir_all(&admin).unwrap();
        assert!(to_trash(&g, &repo, &wt6, Some("twr/t6")).await.is_err());
        assert!(wt5.join(".git").exists() && inside.exists() && branch_exists(&repo, "twr/t5"));
    }

    /// A deletion or a park moves or removes a worktree's directory and record only
    /// when each names the other. Another checkout copied to a thread's path (its `.git`
    /// naming that checkout's record), a symlink at the path to another worktree, or a record
    /// on another thread's branch: the delete, the loss count, the inventory and the park
    /// refuse and touch nothing; the other worktree, its record and its branch stay working.
    #[tokio::test]
    async fn a_worktree_path_holding_another_checkout_is_never_taken_for_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wts = root.join("worktrees").join("proj");
        let x = new_wt(&g, &repo, &wts.join("x"), "twr/x").await;
        let y = new_wt(&g, &repo, &wts.join("y"), "twr/y").await;
        std::fs::write(y.join("y.txt"), "y").unwrap();
        commit_all(&y, "only on y");
        let y_tip = run(&y, &["rev-parse", "HEAD"]);
        std::fs::write(y.join("dirty.txt"), "dirty").unwrap();
        let y_admin = PathBuf::from(run(&y, &["rev-parse", "--absolute-git-dir"]));
        let y_intact = |when: &str| {
            assert!(y_admin.join("HEAD").exists(), "{when}: y's record");
            assert!(worktrees(&repo).contains(&y), "{when}: y's worktree");
            assert_eq!(run(&repo, &["rev-parse", "twr/y"]), y_tip, "{when}");
            assert!(
                run(&y, &["status", "--porcelain"]).contains("dirty.txt"),
                "{when}: git status in y"
            );
            assert_eq!(std::fs::read_to_string(y.join("y.txt")).unwrap(), "y");
        };
        let refused = |what: &str, e: &str, needle: &str| {
            assert!(e.contains(needle), "{what}: {e}");
        };
        // Parked (its worktree removed, its branch kept), then y's checkout copied to its path.
        assert_eq!(
            remove_async(&g, &repo, &x, "twr/x").await,
            RemoveOutcome::Removed
        );
        let copied = std::process::Command::new("cp")
            .arg("-a")
            .arg(&y)
            .arg(&x)
            .status()
            .unwrap();
        assert!(copied.success());
        let e = to_trash(&g, &repo, &x, Some("twr/x")).await.unwrap_err();
        refused("delete", &format!("{e:#}"), "now holds another checkout");
        refused("delete", &format!("{e:#}"), &y_admin.display().to_string());
        let counted = g
            .branch_only_commits(vec![target(&repo, "twr/x", Some(&x))])
            .await
            .unwrap()
            .remove(0);
        refused(
            "loss count",
            counted.unchecked.as_deref().unwrap_or_default(),
            "now holds another checkout",
        );
        let e = g
            .worktree_inventory(repo.clone(), x.clone())
            .await
            .unwrap_err();
        refused("inventory", &format!("{e:#}"), "now holds another checkout");
        let reason = kept_reason(remove_async(&g, &repo, &x, "twr/x").await);
        refused("park", &reason, "now holds another checkout");
        assert!(x.join("y.txt").exists() && branch_exists(&repo, "twr/x"));
        assert!(list(&g).await.is_empty());
        y_intact("copy");

        // The same with the thread's own record still there (its directory replaced).
        let z = new_wt(&g, &repo, &wts.join("z"), "twr/z").await;
        let z_admin = PathBuf::from(run(&z, &["rev-parse", "--absolute-git-dir"]));
        std::fs::remove_dir_all(&z).unwrap();
        let copied = std::process::Command::new("cp")
            .arg("-a")
            .arg(&y)
            .arg(&z)
            .status()
            .unwrap();
        assert!(copied.success());
        let e = to_trash(&g, &repo, &z, Some("twr/z")).await.unwrap_err();
        refused("delete, own record", &format!("{e:#}"), "another checkout");
        let reason = kept_reason(remove_async(&g, &repo, &z, "twr/z").await);
        refused("park, own record", &reason, "another checkout");
        assert!(z_admin.join("HEAD").exists() && branch_exists(&repo, "twr/z"));
        assert!(list(&g).await.is_empty());
        y_intact("copy over a live record");

        // A symlink at the path to y.
        std::fs::remove_dir_all(&x).unwrap();
        std::os::unix::fs::symlink(&y, &x).unwrap();
        let e = to_trash(&g, &repo, &x, Some("twr/x")).await.unwrap_err();
        refused("delete, symlink", &format!("{e:#}"), "is a symlink");
        let counted = g
            .branch_only_commits(vec![target(&repo, "twr/x", Some(&x))])
            .await
            .unwrap()
            .remove(0);
        refused(
            "loss count, symlink",
            counted.unchecked.as_deref().unwrap_or_default(),
            "is a symlink",
        );
        let reason = kept_reason(remove_async(&g, &repo, &x, "twr/x").await);
        refused("park, symlink", &reason, "is a symlink");
        assert!(std::fs::symlink_metadata(&x)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(list(&g).await.is_empty());
        y_intact("symlink");

        // A record whose `HEAD` is on another thread's branch (here x's, which nothing has
        // checked out since x was parked): refused as the hub names x's branch, and as an
        // older hub's request (no list: any other twr/ branch) is.
        let w = new_wt(&g, &repo, &wts.join("w"), "twr/w").await;
        run(&w, &["checkout", "-q", "twr/x"]);
        let e = to_trash_among(
            &g,
            &repo,
            &w,
            Some("twr/w"),
            1000,
            Some(&["twr/x", "twr/y"]),
        )
        .await
        .unwrap_err();
        refused(
            "delete, HEAD",
            &format!("{e:#}"),
            "another thread's checkout",
        );
        refused("delete, HEAD", &format!("{e:#}"), "on the branch twr/x");
        let e = to_trash(&g, &repo, &w, Some("twr/w")).await.unwrap_err();
        refused(
            "delete, HEAD, no list",
            &format!("{e:#}"),
            "another thread's checkout",
        );
        let mut t = target(&repo, "twr/w", Some(&w));
        t.other_branches = Some(others_of(&repo, &["twr/x"]));
        let counted = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        refused(
            "loss count, HEAD",
            counted.unchecked.as_deref().unwrap_or_default(),
            "another thread's checkout",
        );
        // A park keeps any checkout of another branch, whoever's it is.
        assert_eq!(
            remove_async(&g, &repo, &w, "twr/w").await,
            RemoveOutcome::KeptCheckout {
                head: "branch twr/x".into()
            }
        );
        assert!(w.join(".git").exists() && branch_exists(&repo, "twr/w"));
        assert!(list(&g).await.is_empty());
        // Back on its own branch, it goes.
        run(&w, &["checkout", "-q", "twr/w"]);
        let out = to_trash(&g, &repo, &w, Some("twr/w")).await.unwrap();
        assert!(out.worktree_moved && out.branch_deleted && !w.exists());
        y_intact("after");

        // A checkout git made elsewhere and moved to a parked thread's path (`git worktree
        // move`: its record keeps the name it was made with), on a branch no thread has.
        let p = new_wt(&g, &repo, &wts.join("p"), "twr/p").await;
        assert_eq!(
            remove_async(&g, &repo, &p, "twr/p").await,
            RemoveOutcome::Removed
        );
        let q = root.join("elsewhere").join("q");
        run(
            &repo,
            &["worktree", "add", "-q", "-b", "mine", q.to_str().unwrap()],
        );
        std::fs::write(q.join("mine.txt"), "mine").unwrap();
        run(
            &repo,
            &["worktree", "move", q.to_str().unwrap(), p.to_str().unwrap()],
        );
        let e = to_trash_among(&g, &repo, &p, Some("twr/p"), 1000, Some(&[]))
            .await
            .unwrap_err();
        refused(
            "delete, moved here",
            &format!("{e:#}"),
            "made for another path",
        );
        let mut t = target(&repo, "twr/p", Some(&p));
        t.other_branches = Some(Vec::new());
        let counted = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        refused(
            "loss count, moved here",
            counted.unchecked.as_deref().unwrap_or_default(),
            "made for another path",
        );
        let reason = kept_reason(remove_async(&g, &repo, &p, "twr/p").await);
        refused("park, moved here", &reason, "made for another path");
        assert!(p.join("mine.txt").exists() && branch_exists(&repo, "mine"));
        // Only w's item (above) is in the trash.
        assert!(branch_exists(&repo, "twr/p") && list(&g).await.len() == 1);
        y_intact("moved here");

        // The same with a checkout made elsewhere under the very name (its record is named
        // after the thread's path too), on a twr/ branch no thread records: git has no record
        // of it ever leaving the thread's branch.
        let m = new_wt(&g, &repo, &wts.join("m"), "twr/m").await;
        assert_eq!(
            remove_async(&g, &repo, &m, "twr/m").await,
            RemoveOutcome::Removed
        );
        let made = root.join("elsewhere").join("m");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "twr/manual",
                made.to_str().unwrap(),
            ],
        );
        std::fs::write(made.join("mine.txt"), "mine").unwrap();
        run(
            &repo,
            &[
                "worktree",
                "move",
                made.to_str().unwrap(),
                m.to_str().unwrap(),
            ],
        );
        let e = to_trash_among(&g, &repo, &m, Some("twr/m"), 1000, Some(&[]))
            .await
            .unwrap_err();
        refused(
            "delete, same name",
            &format!("{e:#}"),
            "no record of it leaving twr/m",
        );
        let mut t = target(&repo, "twr/m", Some(&m));
        t.other_branches = Some(Vec::new());
        let counted = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        refused(
            "loss count, same name",
            counted.unchecked.as_deref().unwrap_or_default(),
            "no record of it leaving",
        );
        assert!(m.join("mine.txt").exists() && branch_exists(&repo, "twr/manual"));
        assert!(branch_exists(&repo, "twr/m") && list(&g).await.len() == 1);
    }

    /// A thread's own worktree switched to other branches (a thread integrating its own work
    /// makes new ones as it goes) is its own: the loss count names the branch it is on and
    /// counts that branch's commits on no other branch; a park keeps it (its checkout is not
    /// the thread's branch); the deletion moves it and the thread's recorded branch to the
    /// trash and leaves the branches it switched to. A restore puts the worktree back on the
    /// branch it was on, as it was; once that branch has moved, on the thread's branch, and a
    /// note says why.
    #[tokio::test]
    async fn a_worktree_switched_to_another_branch_is_its_threads_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/s"), "twr/s").await;
        std::fs::write(wt.join("a.txt"), "a").unwrap();
        commit_all(&wt, "on its recorded branch");
        run(&wt, &["checkout", "-q", "-b", "twr/s-step"]);
        std::fs::write(wt.join("b.txt"), "b").unwrap();
        commit_all(&wt, "on the first branch it switched to");
        run(&wt, &["checkout", "-q", "-b", "twr/s-final"]);
        std::fs::write(wt.join("c.txt"), "c").unwrap();
        commit_all(&wt, "only on the branch it is on");
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        commit_all(&wt, "also only there");
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("a.txt"), "edited").unwrap();
        std::fs::write(wt.join("wip.txt"), "wip").unwrap();
        let others: &[&str] = &["twr/other"];

        let mut t = target(&repo, "twr/s", Some(&wt));
        t.other_branches = Some(others_of(&repo, &["twr/other"]));
        let o = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        assert_eq!(o.unchecked, None, "{o:?}");
        assert_eq!(
            (o.checkout.as_deref(), o.checkout_only, o.branch, o.head),
            (Some("twr/s-final"), Some(2), Some(0), Some(0)),
            "{o:?}"
        );
        // The step branch keeps nothing of its own: twr/s-final has all of it.
        assert!(o.commits.items.is_empty(), "{o:?}");
        // A thread of another repository whose branch has the same name is no other thread's
        // checkout here.
        let elsewhere = init_repo(&root.join("other-repo"));
        let mut t = target(&repo, "twr/s", Some(&wt));
        t.other_branches = Some(others_of(&elsewhere, &["twr/s-final"]));
        let o = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        assert_eq!((o.unchecked, o.checkout_only), (None, Some(2)));
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/s").await,
            RemoveOutcome::KeptDirty { dirty_files: 2 }
        );

        let out = to_trash_among(&g, &repo, &wt, Some("twr/s"), 1000, Some(others))
            .await
            .unwrap();
        assert!(out.worktree_moved && out.branch_deleted && !wt.exists());
        assert!(!branch_exists(&repo, "twr/s"));
        assert_eq!(run(&repo, &["rev-parse", "twr/s-final"]), tip);
        assert!(branch_exists(&repo, "twr/s-step"));
        let item = out.item.unwrap();
        let m = manifest(&g, &item).await;
        assert_eq!(m.head.as_deref(), Some("ref: refs/heads/twr/s-final"));

        // An older hub's restore (no list of the other threads' branches, and it reads no
        // `checkout`): on the thread's branch, as before.
        let older = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/s".into(),
                paths: vec![wt.clone()],
                other_branches: None,
            })
            .await
            .unwrap();
        assert_eq!(older.checkout, None);
        assert_eq!(run(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/twr/s");
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!wt.exists() && !branch_exists(&repo, "twr/s"));
        // The branch moved by another process while the restore runs (after its check): what
        // the restore made goes, and the item stays.
        let moving = g.clone().with_step_hook(|step, p| {
            if step == Step::Claimed {
                let repo = p.ancestors().nth(3).unwrap().join("repo");
                run(&repo, &["branch", "-f", "twr/s-final", "twr/s-step"]);
            }
            Ok(())
        });
        let e = moving
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/s".into(),
                paths: vec![wt.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("the branch twr/s-final moved"),
            "{e:#}"
        );
        assert!(!wt.exists() && !branch_exists(&repo, "twr/s"));
        run(&repo, &["branch", "-f", "twr/s-final", &tip]);
        // A branch another thread records now: back on the thread's own.
        let e = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/s".into(),
                paths: vec![wt.clone()],
                other_branches: Some(others_of(&repo, &["twr/s-final"])),
            })
            .await
            .unwrap();
        assert_eq!(e.checkout, None);
        assert!(
            e.notes.join("; ").contains("another thread's now"),
            "{:?}",
            e.notes
        );
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());

        // Back on the branch it was on, which the restore checks out without any ref update
        // (`reset` rereads `HEAD` once the files are written and sets the branch back over what
        // another process moved it to meanwhile; it always writes `ORIG_HEAD` on the way).
        let back = restore_at(&g, &item, "twr/s", &wt).await.unwrap();
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        assert!(
            !admin.join("ORIG_HEAD").exists(),
            "the restore ran git reset"
        );
        assert_eq!(
            (back.branch.as_str(), back.checkout.as_deref()),
            ("twr/s", Some("twr/s-final"))
        );
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        assert_eq!(
            run(&wt, &["symbolic-ref", "HEAD"]),
            "refs/heads/twr/s-final"
        );
        assert_eq!(run(&wt, &["rev-parse", "HEAD"]), tip);
        assert_eq!(run(&wt, &["status", "--porcelain"]), "M a.txt\n?? wip.txt");
        assert!(branch_exists(&repo, "twr/s"));

        // Deleted again, and the branch moved meanwhile: the worktree comes back on the
        // thread's branch, the saved files over it, and a note says why.
        let out = to_trash_among(&g, &repo, &wt, Some("twr/s"), 2000, Some(others))
            .await
            .unwrap();
        let again = out.item.unwrap();
        run(&repo, &["branch", "-f", "twr/s-final", "twr/s-step"]);
        let back = restore_at(&g, &again, "twr/s", &wt).await.unwrap();
        assert_eq!(back.checkout, None);
        assert_eq!(run(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/twr/s");
        let notes = back.notes.join("; ");
        assert!(
            notes.contains("its worktree was on the branch twr/s-final")
                && notes.contains("it is on twr/s now (the branch twr/s-final has moved since"),
            "{notes}"
        );
        assert_eq!(std::fs::read_to_string(wt.join("d.txt")).unwrap(), "d");
        assert_eq!(std::fs::read_to_string(wt.join("wip.txt")).unwrap(), "wip");
    }

    /// The item's directory as it is: every entry's relative path with its contents (a
    /// symlink's target), its kind and mode, and its modification time.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32, i64, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out = BTreeMap::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                let rel = p.strip_prefix(dir).unwrap().to_path_buf();
                let body = if m.is_dir() {
                    todo.push(p.clone());
                    Vec::new()
                } else if m.file_type().is_symlink() {
                    std::fs::read_link(&p).unwrap().into_os_string().into_vec()
                } else if m.is_file() {
                    std::fs::read(&p).unwrap()
                } else {
                    Vec::new()
                };
                out.insert(rel, (body, m.mode(), m.mtime(), m.mtime_nsec()));
            }
        }
        out
    }

    /// Restores trash item `item` of `g` as `branch` at `path`.
    async fn restore_at(
        g: &LocalGit,
        item: &str,
        branch: &str,
        path: &Path,
    ) -> anyhow::Result<twr_core::git::RestoreOutcome> {
        g.trash_restore(RestoreRequest {
            item: item.into(),
            branch: branch.into(),
            paths: vec![path.into()],
            other_branches: Some(Vec::new()),
        })
        .await
    }

    /// A restore copies the thread back and leaves the item as it was: a new
    /// worktree git made on the branch made again (`git worktree list` has it, `git status`
    /// works), uncommitted edits, untracked and ignored files, a nested repository, file modes,
    /// symlinks as symlinks (one to a directory outside is not followed), a deleted tracked
    /// file still deleted, a FIFO skipped and reported. The original path taken: the next one.
    /// The branch taken, or the item restored already: refused, nothing made. A purge then
    /// removes the item and leaves the restored worktree and branch.
    #[tokio::test]
    async fn a_restore_copies_the_worktree_back_and_leaves_the_item() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), "outside").unwrap();
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        std::fs::write(wt.join("gone.txt"), "tracked, then deleted").unwrap();
        std::fs::create_dir_all(wt.join("deep/er")).unwrap();
        std::fs::write(wt.join("deep/er/old.txt"), "tracked, then deleted").unwrap();
        std::fs::write(wt.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(wt.join("run.sh"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(wt.join(".gitignore"), "*.log\n").unwrap();
        commit_all(&wt, "work");
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("README.md"), "edited, not committed\n").unwrap();
        std::fs::remove_file(wt.join("gone.txt")).unwrap();
        std::fs::remove_dir_all(wt.join("deep")).unwrap();
        std::fs::write(wt.join("new.txt"), "untracked").unwrap();
        std::fs::write(wt.join("x.log"), "ignored").unwrap();
        std::os::unix::fs::symlink("README.md", wt.join("readme-link")).unwrap();
        std::os::unix::fs::symlink(&outside, wt.join("out-link")).unwrap();
        mkfifo(&wt.join("pipe"));
        let nested = init_repo(&wt.join("tools/n"));
        let nested_commit = run(&nested, &["rev-parse", "HEAD"]);
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        let item_dir = out.dir.unwrap();
        let before = snapshot(&item_dir);
        let outside_before = snapshot(&outside);

        // The branch is taken meanwhile: refused, nothing made.
        run(&repo, &["branch", "twr/t", "HEAD"]);
        let e = restore_at(&g, &item, "twr/t", &wt).await.unwrap_err();
        assert!(format!("{e:#}").contains("exists already"), "{e:#}");
        assert!(!wt.exists());
        run(&repo, &["branch", "-D", "twr/t"]);
        // The original path is taken: the next one.
        std::fs::create_dir_all(&wt).unwrap();
        let wt_b = root.join("worktrees/proj/t-2");
        let back = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone(), wt_b.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(
            (back.worktree.as_path(), back.branch.as_str()),
            (wt_b.as_path(), "twr/t")
        );
        assert_eq!(snapshot(&item_dir), before, "the item is untouched");
        assert_eq!(snapshot(&outside), outside_before);
        assert!(worktrees(&repo).contains(&wt_b));
        assert_eq!(run(&wt_b, &["symbolic-ref", "HEAD"]), "refs/heads/twr/t");
        assert_eq!(run(&wt_b, &["rev-parse", "HEAD"]), tip);
        let read = |p: &str| std::fs::read_to_string(wt_b.join(p)).unwrap();
        assert_eq!(read("README.md"), "edited, not committed\n");
        assert_eq!(
            (read("new.txt"), read("x.log")),
            ("untracked".into(), "ignored".into())
        );
        assert!(!wt_b.join("gone.txt").exists() && !wt_b.join("deep").exists());
        let mode = std::fs::metadata(wt_b.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        for (link, target) in [
            ("readme-link", PathBuf::from("README.md")),
            ("out-link", outside.clone()),
        ] {
            let m = std::fs::symlink_metadata(wt_b.join(link)).unwrap();
            assert!(m.file_type().is_symlink(), "{link}");
            assert_eq!(std::fs::read_link(wt_b.join(link)).unwrap(), target);
        }
        assert!(!wt_b.join("pipe").exists());
        assert_eq!(back.skipped, ["pipe"]);
        assert!(
            back.notes
                .iter()
                .any(|n| n.contains("FIFO") && n.contains("pipe")),
            "{:?}",
            back.notes
        );
        assert_eq!(
            run(&wt_b.join("tools/n"), &["rev-parse", "HEAD"]),
            nested_commit
        );
        let status = run(&wt_b, &["status", "--porcelain", "--ignored"]);
        for want in [
            "M README.md",
            " D gone.txt",
            " D deep/er/old.txt",
            "?? new.txt",
            "!! x.log",
        ] {
            assert!(status.contains(want), "{want}: {status}");
        }
        assert!(run(&repo, &["reflog", "twr/t"]).contains("restored from the trash"));
        // The item stays, marked restored by the new worktree's record.
        let items = list(&g).await;
        assert_eq!(items.len(), 1);
        let restored = items[0].restored.clone().unwrap();
        assert_eq!(
            (restored.worktree, restored.branch.as_str()),
            (wt_b.clone(), "twr/t")
        );
        assert!(!trash_refs(&repo).is_empty());
        // Not twice.
        let e = restore_at(&g, &item, "twr/u", &root.join("u"))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("restored already"), "{e:#}");
        assert!(!branch_exists(&repo, "twr/u") && !root.join("u").exists());
        assert_eq!(snapshot(&item_dir), before);
        // A purge removes the item and its refs; the restored worktree and branch stay.
        let p = g
            .trash_purge(TrashPurge {
                items: vec![item.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p.purged, std::slice::from_ref(&item), "{p:?}");
        assert!(!item_dir.exists() && trash_refs(&repo).is_empty());
        assert_eq!(read("README.md"), "edited, not committed\n");
        assert!(branch_exists(&repo, "twr/t") && worktrees(&repo).contains(&wt_b));
        run(&wt_b, &["status"]);
    }

    /// A restore that fails part way (a saved file it cannot read) removes what it
    /// made, the worktree, its record and the branch, and leaves the item as it was; once the
    /// cause is gone it restores. A restore the hub could not record is undone the same way
    /// (`trash_restore_undo`, found by the mark in the worktree's record), the item untouched,
    /// and can be made again.
    #[tokio::test]
    async fn what_a_restore_made_goes_and_the_item_stays() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/t"), "twr/t").await;
        std::fs::create_dir_all(wt.join("a/b")).unwrap();
        std::fs::write(wt.join("a/b/secret.txt"), "work").unwrap();
        std::fs::write(wt.join("z.txt"), "z").unwrap();
        commit_all(&wt, "work");
        std::fs::write(wt.join("a/b/secret.txt"), "more work").unwrap();
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        let item_dir = out.dir.unwrap();
        let records = || run(&repo, &["worktree", "list", "--porcelain"]);
        let records_before = records();
        let before = snapshot(&item_dir);
        if let Some(u) = Unreadable::new(&item_dir.join("worktree/a/b/secret.txt")) {
            let e = restore_at(&g, &item, "twr/t", &wt).await.unwrap_err();
            assert!(format!("{e:#}").contains("the item is as it was"), "{e:#}");
            drop(u);
            let mut after = snapshot(&item_dir);
            // (Only the test's own permission change.)
            for (k, v) in after.iter_mut() {
                if k.ends_with("secret.txt") {
                    v.1 = before[k].1;
                }
            }
            assert_eq!(after, before, "the item is untouched");
            assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
            assert_eq!(records(), records_before);
            assert!(!root.join("repo/.git/logs/refs/heads/twr/t").exists());
            assert!(list(&g).await[0].restored.is_none());
        }
        // Made, then undone as when the hub cannot record it.
        let before = snapshot(&item_dir);
        restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        assert_eq!(
            std::fs::read_to_string(wt.join("a/b/secret.txt")).unwrap(),
            "more work"
        );
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(records(), records_before);
        assert_eq!(snapshot(&item_dir), before);
        assert!(
            !g.trash_restore_undo(item.clone()).await.unwrap(),
            "nothing left"
        );
        // A symlink at its path (to the restored checkout, moved away) is not it:
        // nothing is removed; moved back, the undo removes it.
        restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        let moved = root.join("moved");
        std::fs::rename(&wt, &moved).unwrap();
        std::os::unix::fs::symlink(&moved, &wt).unwrap();
        let e = g.trash_restore_undo(item.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("nothing is removed"), "{e:#}");
        assert!(moved.join("z.txt").exists() && branch_exists(&repo, "twr/t"));
        std::fs::remove_file(&wt).unwrap();
        std::fs::rename(&moved, &wt).unwrap();
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(records(), records_before);
        // A branch moved since the restore stays.
        restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        run(&wt, &["commit", "-q", "--allow-empty", "-m", "on top"]);
        let e = g.trash_restore_undo(item.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("points at"), "{e:#}");
        assert!(branch_exists(&repo, "twr/t") && !wt.exists());
        assert_eq!(snapshot(&item_dir), before);
    }

    /// What is in the item or in the branch's checkout cannot make a restore write
    /// outside the new worktree: the saved tree's own `.git` (a symlink here) is skipped, a
    /// symlink of the checkout where the saved tree has a directory is replaced by the
    /// directory (not written through), a saved symlink where the checkout has a directory
    /// replaces it, and a symlinked `worktree/` in the item is refused.
    #[tokio::test]
    async fn a_restore_never_writes_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("sub")).unwrap();
        std::fs::write(outside.join("keep.txt"), "outside").unwrap();
        let outside_before = snapshot(&outside);
        let wt = new_wt(&g, &repo, &root.join("worktrees/t"), "twr/t").await;
        // Committed: `docs` a symlink to the outside, `lib/x.txt` a file.
        std::os::unix::fs::symlink(&outside, wt.join("docs")).unwrap();
        std::fs::create_dir_all(wt.join("lib")).unwrap();
        std::fs::write(wt.join("lib/x.txt"), "x").unwrap();
        commit_all(&wt, "links");
        // In the worktree: `docs` a directory with files, `lib` a symlink to the outside.
        std::fs::remove_file(wt.join("docs")).unwrap();
        std::fs::create_dir_all(wt.join("docs/sub")).unwrap();
        std::fs::write(wt.join("docs/keep.txt"), "mine").unwrap();
        std::fs::write(wt.join("docs/sub/new.txt"), "mine").unwrap();
        std::fs::remove_dir_all(wt.join("lib")).unwrap();
        std::os::unix::fs::symlink(outside.join("sub"), wt.join("lib")).unwrap();
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        let item_dir = out.dir.unwrap();
        // The saved `.git` a symlink to a directory outside.
        std::fs::remove_file(item_dir.join("worktree/.git")).unwrap();
        std::os::unix::fs::symlink(&outside, item_dir.join("worktree/.git")).unwrap();
        let back = restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        assert_eq!(
            snapshot(&outside),
            outside_before,
            "nothing written outside"
        );
        assert!(std::fs::symlink_metadata(wt.join(".git"))
            .unwrap()
            .is_file());
        assert_eq!(
            run(&wt, &["rev-parse", "--show-toplevel"]),
            wt.to_string_lossy()
        );
        assert!(std::fs::symlink_metadata(wt.join("docs")).unwrap().is_dir());
        assert_eq!(
            std::fs::read_to_string(wt.join("docs/keep.txt")).unwrap(),
            "mine"
        );
        assert_eq!(
            std::fs::read_to_string(wt.join("docs/sub/new.txt")).unwrap(),
            "mine"
        );
        assert_eq!(
            std::fs::read_link(wt.join("lib")).unwrap(),
            outside.join("sub")
        );
        assert!(back.skipped.is_empty(), "{back:?}");
        run(&wt, &["status"]);
        // A symlinked `worktree/` in the item: refused, nothing made.
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        let saved = item_dir.join("worktree");
        std::fs::rename(&saved, item_dir.join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(item_dir.join("elsewhere"), &saved).unwrap();
        let e = restore_at(&g, &item, "twr/t", &wt).await.unwrap_err();
        assert!(format!("{e:#}").contains("is not a directory"), "{e:#}");
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(snapshot(&outside), outside_before);
    }

    /// A trash item of `g` from a worktree of a new repository at `root/repo` (branch
    /// `twr/t`, one commit and an uncommitted edit): (repo, the worktree's old path, item).
    async fn trashed_thread(g: &LocalGit, root: &Path) -> (PathBuf, PathBuf, String) {
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(g, &repo, &root.join("worktrees/t"), "twr/t").await;
        std::fs::write(wt.join("work.txt"), "work").unwrap();
        commit_all(&wt, "work");
        std::fs::write(wt.join("work.txt"), "more work").unwrap();
        let out = to_trash(g, &repo, &wt, Some("twr/t")).await.unwrap();
        (repo, wt, out.item.unwrap())
    }

    /// A restore never deletes what another process puts in its way after its
    /// checks: a directory made at the chosen place before the restore makes it moves the
    /// restore to the next place (none left: refused, nothing made), and files written into
    /// the directory it made before git adds the worktree make git refuse; either way what
    /// the other process made stays as it was.
    #[tokio::test]
    async fn what_another_process_puts_in_a_restores_way_stays() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let (repo, wt, item) = trashed_thread(&g, &root).await;
        let records = || run(&repo, &["worktree", "list", "--porcelain"]);
        let records_before = records();
        let log = repo.join(".git/logs/refs/heads/twr/t");
        // Taken between the checks and the claim: the next place.
        let theirs = wt.clone();
        let taking = g.clone().with_step_hook(move |step, p| {
            if step == Step::Chosen && p == theirs {
                std::fs::create_dir_all(p)?;
                std::fs::write(p.join("theirs.txt"), "theirs")?;
            }
            Ok(())
        });
        let wt_b = root.join("worktrees/t-2");
        let back = taking
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone(), wt_b.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(back.worktree, wt_b);
        let foreign = snapshot(&wt);
        assert_eq!(foreign.len(), 1, "{foreign:?}");
        assert_eq!(
            std::fs::read_to_string(wt.join("theirs.txt")).unwrap(),
            "theirs"
        );
        assert_eq!(
            std::fs::read_to_string(wt_b.join("work.txt")).unwrap(),
            "more work"
        );
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!wt_b.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(snapshot(&wt), foreign);
        // No other place: refused, nothing made.
        std::fs::remove_dir_all(&wt).unwrap();
        let e = taking
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("is taken"), "{e:#}");
        assert_eq!(snapshot(&wt).len(), 1);
        assert_eq!(
            std::fs::read_to_string(wt.join("theirs.txt")).unwrap(),
            "theirs"
        );
        assert!(!branch_exists(&repo, "twr/t") && !log.exists());
        assert_eq!(records(), records_before);
        // Written into the claimed directory before the add: git refuses, the restore removes
        // its branch and reflog, and the directory (not empty) and the ones above it stay.
        let writing = g.clone().with_step_hook(|step, p| {
            if step == Step::Claimed {
                std::fs::write(p.join("theirs.txt"), "theirs")?;
            }
            Ok(())
        });
        let wt_c = root.join("fresh/a/t");
        let e = writing
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt_c.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("git worktree add"), "{e:#}");
        assert_eq!(
            std::fs::read_to_string(wt_c.join("theirs.txt")).unwrap(),
            "theirs"
        );
        assert_eq!(snapshot(&wt_c).len(), 1);
        assert!(!branch_exists(&repo, "twr/t") && !log.exists());
        assert_eq!(records(), records_before);
        assert!(list(&g).await[0].restored.is_none());
    }

    /// A restore clears no worktree record. A stale record at the old path (its
    /// directory gone, the repositories of its submodules with a commit nothing else has in
    /// its `modules/`) makes the restore take the next place, and stays as it was; with no
    /// other place the restore is refused.
    #[tokio::test]
    async fn a_restore_leaves_a_stale_record_at_its_old_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let (repo, wt, item) = trashed_thread(&g, &root).await;
        let other = new_wt(&g, &repo, &wt, "other").await;
        let admin = PathBuf::from(run(&other, &["rev-parse", "--absolute-git-dir"]));
        let sub = init_repo(&admin.join("modules/sub"));
        let unpublished = run(&sub, &["rev-parse", "HEAD"]);
        std::fs::remove_dir_all(&other).unwrap();
        assert!(run(&repo, &["worktree", "list", "--porcelain"]).contains("prunable"));
        let before = snapshot(&admin);
        let e = restore_at(&g, &item, "twr/t", &wt).await.unwrap_err();
        assert!(format!("{e:#}").contains("names it"), "{e:#}");
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(snapshot(&admin), before);
        let wt_b = root.join("worktrees/t-2");
        let back = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone(), wt_b.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(back.worktree, wt_b);
        assert!(!wt.exists());
        assert_eq!(snapshot(&admin), before, "the stale record is untouched");
        assert_eq!(run(&sub, &["rev-parse", "HEAD"]), unpublished);
        assert!(worktrees(&repo).contains(&wt));
        // Undone: the stale record still stays.
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert_eq!(snapshot(&admin), before);
    }

    /// A restore that fails once git added its worktree removes the worktree, the
    /// record git made for it, the branch and the reflog it wrote, and the directories above
    /// the worktree it made while they are empty; another worktree and its branch, and a
    /// file another process put in a directory the restore made, stay. An undo removes only
    /// a worktree whose `.git` names the marked record.
    #[tokio::test]
    async fn a_failed_restore_removes_only_what_it_made() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let (repo, wt, item) = trashed_thread(&g, &root).await;
        let keep = new_wt(&g, &repo, &root.join("worktrees/k"), "twr/k").await;
        std::fs::write(keep.join("mine.txt"), "mine").unwrap();
        let keep_admin = PathBuf::from(run(&keep, &["rev-parse", "--absolute-git-dir"]));
        let (keep_before, keep_admin_before) = (snapshot(&keep), snapshot(&keep_admin));
        let records = || run(&repo, &["worktree", "list", "--porcelain"]);
        let records_before = records();
        let log = repo.join(".git/logs/refs/heads/twr/t");
        let foreign = root.join("new1/foreign.txt");
        let failing = {
            let foreign = foreign.clone();
            g.clone().with_step_hook(move |step, p| match step {
                Step::Claimed if p.starts_with(foreign.parent().unwrap()) => {
                    std::fs::write(&foreign, "theirs")
                }
                Step::Added => Err(std::io::Error::other("injected")),
                _ => Ok(()),
            })
        };
        // A file of another process in a directory the restore made: that one stays.
        let wt_a = root.join("new1/new2/t");
        let e = restore_at(&failing, &item, "twr/t", &wt_a)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("injected"), "{e:#}");
        assert!(
            format!("{e:#}").contains("what it made is removed"),
            "{e:#}"
        );
        assert!(!root.join("new1/new2").exists());
        assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "theirs");
        assert_eq!(snapshot(&root.join("new1")).len(), 1);
        assert!(!branch_exists(&repo, "twr/t") && !log.exists());
        assert_eq!(records(), records_before);
        assert!(!repo.join(".git/worktrees/t").exists());
        // All empty: all of it goes.
        let wt_b = root.join("p1/p2/t");
        restore_at(&failing, &item, "twr/t", &wt_b)
            .await
            .unwrap_err();
        assert!(!root.join("p1").exists() && root.exists());
        assert!(!branch_exists(&repo, "twr/t") && !log.exists());
        assert_eq!(records(), records_before);
        assert_eq!(
            (snapshot(&keep), snapshot(&keep_admin)),
            (keep_before.clone(), keep_admin_before.clone())
        );
        assert!(branch_exists(&repo, "twr/k"));
        // Made; its directory then replaced by another one: the undo removes nothing of it.
        restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        std::fs::remove_dir_all(&wt).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(
            wt.join(".git"),
            format!("gitdir: {}\n", keep_admin.display()),
        )
        .unwrap();
        std::fs::write(wt.join("theirs.txt"), "theirs").unwrap();
        let theirs = snapshot(&wt);
        let e = g.trash_restore_undo(item.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("nothing is removed"), "{e:#}");
        assert_eq!(snapshot(&wt), theirs);
        assert!(admin.exists() && branch_exists(&repo, "twr/t"));
        assert_eq!(
            (snapshot(&keep), snapshot(&keep_admin)),
            (keep_before, keep_admin_before)
        );
    }

    /// An item that keeps no branch (the thread had none) gets its branch at its
    /// worktree's `HEAD`; a worktree that was on another branch, or detached, comes back on
    /// its own branch, and the result says where that `HEAD` was and which trash ref keeps
    /// its commit, also when that other branch is checked out elsewhere.
    #[tokio::test]
    async fn a_head_elsewhere_comes_back_on_the_threads_branch() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        // On another branch, checked out in the main checkout meanwhile.
        let wt = new_wt(&g, &repo, &root.join("worktrees/t"), "twr/t").await;
        run(&wt, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(wt.join("f.txt"), "f").unwrap();
        commit_all(&wt, "on feature");
        let feature = run(&wt, &["rev-parse", "HEAD"]);
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        run(&repo, &["checkout", "-q", "feature"]);
        let back = restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        assert_eq!(run(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/twr/t");
        let note = back.notes.join("\n");
        assert!(
            note.contains("on the branch feature")
                && note.contains(&feature[..12])
                && note.contains(&format!("refs/twr-trash/{item}/head")),
            "{note}"
        );
        assert!(kept_by_trash(&repo, &item, &feature));
        run(&repo, &["checkout", "-q", "main"]);
        // No branch of its own, detached.
        let wt2 = new_wt(&g, &repo, &root.join("worktrees/d"), "twr/d").await;
        run(&wt2, &["checkout", "-q", "--detach"]);
        std::fs::write(wt2.join("d.txt"), "d").unwrap();
        commit_all(&wt2, "detached");
        let detached = run(&wt2, &["rev-parse", "HEAD"]);
        let out = to_trash(&g, &repo, &wt2, None).await.unwrap();
        let item = out.item.unwrap();
        let back = restore_at(&g, &item, "twr/d2", &wt2).await.unwrap();
        assert_eq!(run(&repo, &["rev-parse", "twr/d2"]), detached);
        assert_eq!(run(&wt2, &["symbolic-ref", "HEAD"]), "refs/heads/twr/d2");
        let note = back.notes.join("\n");
        assert!(
            note.contains("no branch of its own") && note.contains("detached"),
            "{note}"
        );
        assert_eq!(std::fs::read_to_string(wt2.join("d.txt")).unwrap(), "d");
    }

    /// What deleting a thread loses: the commits only its branch has, and those only its
    /// worktree's `HEAD` or `HEAD` reflog reaches (made on a detached `HEAD`; removing the
    /// worktree deletes both), unless a tag or branch holds them.
    #[tokio::test]
    async fn only_commits_counts_the_worktree_head_too() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("h"), "twr/h").await;
        let count = |worktree: Option<&PathBuf>| {
            let f = g.branch_only_commits(vec![DeleteRefs {
                repo: repo.clone(),
                branch: Some("twr/h".into()),
                worktree: worktree.cloned(),
                other_branches: None,
            }]);
            async move {
                let o = f.await.unwrap().remove(0);
                (o.branch, o.head)
            }
        };
        assert_eq!(count(Some(&wt)).await, (Some(0), Some(0)));
        std::fs::write(wt.join("a.txt"), "a").unwrap();
        commit_all(&wt, "on the branch");
        run(&wt, &["checkout", "-q", "--detach"]);
        for f in ["b.txt", "c.txt"] {
            std::fs::write(wt.join(f), f).unwrap();
            commit_all(&wt, f);
        }
        let both = (Some(1), Some(2));
        assert_eq!(count(Some(&wt)).await, both);
        // Back on the branch: only the reflog remembers them.
        run(&wt, &["checkout", "-q", "twr/h"]);
        assert_eq!(count(Some(&wt)).await, both);
        // The worktree stays: its `HEAD` reflog keeps them all.
        assert_eq!(count(None).await, (Some(0), None));
        // HEAD@{1}: the detached tip, before the checkout.
        run(&wt, &["tag", "kept", "HEAD@{1}"]);
        assert_eq!(count(Some(&wt)).await.1, Some(0));
        // No worktree there, or no linked worktree of the repository: not counted.
        assert_eq!(count(Some(&dir.path().join("none"))).await.1, None);
        assert_eq!(count(Some(&repo)).await.1, None);
    }

    /// Branches deleted together keep none of each other's commits: a commit only two
    /// branches of the set have is lost, shared by both; with one of them kept, it is not.
    #[tokio::test]
    async fn only_commits_counts_the_whole_set_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt_a = new_wt(&g, &repo, &dir.path().join("worktrees").join("a"), "twr/a").await;
        std::fs::write(wt_a.join("a.txt"), "a").unwrap();
        commit_all(&wt_a, "shared");
        let wt_b = dir.path().join("worktrees").join("b");
        g.worktree_create(spec(&repo, &wt_b, "twr/b", Some("twr/a")))
            .await
            .unwrap();
        std::fs::write(wt_b.join("b.txt"), "b").unwrap();
        commit_all(&wt_b, "b's own");
        let refs = |b: &str, w: &Path| DeleteRefs {
            repo: repo.clone(),
            branch: Some(b.into()),
            worktree: Some(w.to_path_buf()),
            other_branches: None,
        };
        let out = g
            .branch_only_commits(vec![refs("twr/a", &wt_a), refs("twr/b", &wt_b)])
            .await
            .unwrap();
        assert_eq!(
            (out[0].branch, out[0].head, out[0].commits.total),
            (Some(1), Some(0), 1)
        );
        assert_eq!(
            (out[1].branch, out[1].head, out[1].commits.total),
            (Some(2), Some(0), 2)
        );
        assert_eq!(
            out[0].shared,
            [SharedCommits {
                with: 1,
                commits: 1
            }]
        );
        assert_eq!(
            out[1].shared,
            [SharedCommits {
                with: 0,
                commits: 1
            }]
        );
        assert_eq!(
            out[0].commits.items[0],
            run(&repo, &["rev-parse", "twr/a"]).trim()
        );
        // Alone, each counts the other as a surviving copy.
        let alone = g
            .branch_only_commits(vec![refs("twr/a", &wt_a)])
            .await
            .unwrap();
        assert_eq!((alone[0].branch, alone[0].shared.len()), (Some(0), 0));
        // A target without a branch or worktree: nothing.
        let none = g
            .branch_only_commits(vec![DeleteRefs {
                repo: repo.clone(),
                ..Default::default()
            }])
            .await
            .unwrap();
        assert_eq!(
            none,
            [OnlyCommits {
                common: Some(std::fs::canonicalize(repo.join(".git")).unwrap()),
                ..Default::default()
            }]
        );
        // Each names its repository's git common directory, whichever checkout it names.
        let through_wt = g
            .branch_only_commits(vec![DeleteRefs {
                repo: wt_b.clone(),
                ..Default::default()
            }])
            .await
            .unwrap();
        for o in [&out[0], &out[1], &alone[0], &through_wt[0]] {
            assert_eq!(o.common, none[0].common);
        }
        // Outside any repository: not checked, never "nothing lost".
        let outside = g
            .branch_only_commits(vec![DeleteRefs {
                repo: dir.path().join("nowhere"),
                branch: Some("twr/a".into()),
                worktree: None,
                other_branches: None,
            }])
            .await
            .unwrap();
        assert!(outside[0].unchecked.is_some() && outside[0].branch.is_none());
    }

    /// One thread of `repo` for [`GitOps::branch_only_commits`].
    fn target(repo: &Path, branch: &str, worktree: Option<&Path>) -> DeleteRefs {
        DeleteRefs {
            repo: repo.to_path_buf(),
            branch: Some(branch.into()),
            worktree: worktree.map(Path::to_path_buf),
            other_branches: None,
        }
    }

    /// `(branch, head)` lost counts of the one target `t`, and the lost commits.
    async fn lost_one(g: &LocalGit, t: DeleteRefs) -> ((Option<u64>, Option<u64>), Vec<String>) {
        let o = g.branch_only_commits(vec![t]).await.unwrap().remove(0);
        assert_eq!(o.unchecked, None, "{o:?}");
        ((o.branch, o.head), o.commits.items)
    }

    /// A worktree whose directory is gone still has its record, which the deletion clears:
    /// a commit only its `HEAD` reflog keeps is lost.
    #[tokio::test]
    async fn a_worktree_without_its_directory_still_counts_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(
            &g,
            &repo,
            &dir.path().join("worktrees").join("l"),
            "twr/lost",
        )
        .await;
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("x.txt"), "x").unwrap();
        commit_all(&wt, "detached");
        let detached = run(&wt, &["rev-parse", "HEAD"]).trim().to_string();
        run(&wt, &["checkout", "-q", "twr/lost"]);
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            lost_one(&g, target(&repo, "twr/lost", Some(&wt))).await,
            ((Some(0), Some(1)), vec![detached])
        );
        // Its `.git` file gone but the directory left: the same.
        let wt2 = new_wt(&g, &repo, &dir.path().join("worktrees").join("m"), "twr/m").await;
        run(&wt2, &["checkout", "-q", "--detach"]);
        std::fs::write(wt2.join("y.txt"), "y").unwrap();
        commit_all(&wt2, "detached too");
        run(&wt2, &["checkout", "-q", "twr/m"]);
        std::fs::remove_file(wt2.join(".git")).unwrap();
        assert_eq!(
            lost_one(&g, target(&repo, "twr/m", Some(&wt2))).await.0,
            (Some(0), Some(1))
        );
    }

    /// Threads created through two checkouts of one repository (its main checkout and a
    /// linked one the project was opened through) are one deletion set: a commit only their
    /// two branches have is lost, and shared.
    #[tokio::test]
    async fn two_checkouts_of_one_repository_are_one_set() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let alt = dir.path().join("alt");
        run(
            &repo,
            &["worktree", "add", "-q", "-b", "alt", alt.to_str().unwrap()],
        );
        let wt_a = new_wt(&g, &repo, &dir.path().join("worktrees").join("a"), "twr/a").await;
        std::fs::write(wt_a.join("a.txt"), "a").unwrap();
        commit_all(&wt_a, "shared");
        let wt_b = dir.path().join("worktrees").join("b");
        g.worktree_create(spec(&alt, &wt_b, "twr/b", Some("twr/a")))
            .await
            .unwrap();
        let out = g
            .branch_only_commits(vec![
                target(&repo, "twr/a", Some(&wt_a)),
                target(&alt, "twr/b", Some(&wt_b)),
            ])
            .await
            .unwrap();
        for (o, other) in out.iter().zip([1, 0]) {
            assert_eq!((o.branch, o.head, &o.unchecked), (Some(1), Some(0), &None));
            assert_eq!(
                o.shared,
                [SharedCommits {
                    with: other,
                    commits: 1
                }]
            );
        }
        // Threads of other repositories have nothing in common.
        let other = init_repo(&dir.path().join("other"));
        let out = g
            .branch_only_commits(vec![
                target(&repo, "twr/a", Some(&wt_a)),
                target(&other, "main", None),
            ])
            .await
            .unwrap();
        assert_eq!(
            (out[0].branch, out[0].shared.len(), out[1].shared.len()),
            (Some(0), 0, 0)
        );
    }

    /// `git branch -D` deletes the branch's reflog too: a commit only that reflog keeps (the
    /// branch was moved back) is lost.
    #[tokio::test]
    async fn a_commit_only_the_branch_reflog_keeps_is_lost() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("x"), "twr/x").await;
        std::fs::write(wt.join("x.txt"), "x").unwrap();
        commit_all(&wt, "unpublished");
        let work = run(&wt, &["rev-parse", "HEAD"]).trim().to_string();
        // Parked (worktree and record removed), then the branch moved back.
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/x".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
        run(&repo, &["branch", "-f", "twr/x", "twr/x~1"]);
        assert_eq!(
            lost_one(&g, target(&repo, "twr/x", Some(&wt))).await,
            ((Some(1), None), vec![work])
        );
    }

    /// What stays keeps its commits: `refs/stash`, another branch's reflog, another worktree's
    /// detached `HEAD`.
    #[tokio::test]
    async fn what_stays_keeps_its_commits() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("s"), "twr/s").await;
        std::fs::write(wt.join("s.txt"), "s").unwrap();
        commit_all(&wt, "unpublished");
        let t = || target(&repo, "twr/s", Some(&wt));
        assert_eq!(lost_one(&g, t()).await.0, (Some(1), Some(0)));
        // A stash made on it: its parent.
        std::fs::write(wt.join("s.txt"), "edited").unwrap();
        run(&wt, &["stash", "-q"]);
        assert_eq!(lost_one(&g, t()).await.0, (Some(0), Some(0)));
        run(&wt, &["stash", "drop", "-q"]);
        assert_eq!(lost_one(&g, t()).await.0, (Some(1), Some(0)));
        // Another branch that had it, and moved on: its reflog.
        run(&repo, &["branch", "keep", "twr/s"]);
        run(&repo, &["branch", "-f", "keep", "main"]);
        assert_eq!(lost_one(&g, t()).await.0, (Some(0), Some(0)));
        run(&repo, &["branch", "-D", "keep"]);
        assert_eq!(lost_one(&g, t()).await.0, (Some(1), Some(0)));
        // Another worktree, detached on it.
        let other = dir.path().join("other");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                other.to_str().unwrap(),
                "twr/s",
            ],
        );
        assert_eq!(lost_one(&g, t()).await.0, (Some(0), Some(0)));
    }

    /// A reflog, record or object of the set that cannot be read leaves that target not
    /// checked, never "nothing lost"; the other targets are counted.
    #[tokio::test]
    async fn what_cannot_be_read_is_unchecked() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt_a = new_wt(&g, &repo, &dir.path().join("worktrees").join("a"), "twr/a").await;
        let wt_b = new_wt(&g, &repo, &dir.path().join("worktrees").join("b"), "twr/b").await;
        let common = repo.join(".git");
        let log = common.join("logs/refs/heads/twr/a");
        let good = std::fs::read(&log).unwrap();
        let check = || {
            g.branch_only_commits(vec![
                target(&repo, "twr/a", Some(&wt_a)),
                target(&repo, "twr/b", Some(&wt_b)),
            ])
        };
        std::fs::write(&log, "garbage\n").unwrap();
        let out = check().await.unwrap();
        let why = out[0].unchecked.clone().unwrap();
        assert!(why.contains("reflog") && why.contains("twr/a"), "{why}");
        assert_eq!((out[0].branch, out[0].head), (None, None));
        assert_eq!((out[1].branch, &out[1].unchecked), (Some(0), &None));
        // A well-formed entry naming an object the repository does not have.
        let gone = "1".repeat(40);
        std::fs::write(
            &log,
            format!("{gone} {gone} A <a@b> 1 +0000\tcommit: gone\n"),
        )
        .unwrap();
        let why = check().await.unwrap()[0].unchecked.clone().unwrap();
        assert!(why.contains(&gone) && why.contains("missing"), "{why}");
        std::fs::write(&log, &good).unwrap();
        assert_eq!(check().await.unwrap()[0].unchecked, None);
        // Its worktree's record: a HEAD that holds neither a branch nor a commit.
        let admin = common.join("worktrees").join("a");
        std::fs::write(admin.join("HEAD"), "garbage\n").unwrap();
        let why = check().await.unwrap()[0].unchecked.clone().unwrap();
        assert!(why.contains("HEAD"), "{why}");
        // A record that cannot be told apart, for a worktree no record names for sure.
        std::fs::write(admin.join("HEAD"), "ref: refs/heads/twr/a\n").unwrap();
        std::fs::remove_dir_all(&wt_a).unwrap();
        std::fs::remove_file(admin.join("gitdir")).unwrap();
        let why = check().await.unwrap()[0].unchecked.clone().unwrap();
        assert!(why.contains("cannot tell"), "{why}");
    }

    /// The inventory of a work tree lists what `git status` shows (each untracked file), the
    /// tracked files whose edits it hides, the ignored files by top-level entry, and apart from
    /// those its build output (which a deletion deletes) with its size.
    #[tokio::test]
    async fn the_inventory_lists_hidden_edits_and_ignored_files() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitignore"), "target/\n.env\n*.o\n").unwrap();
        for f in ["config.local", "skip.txt", "same.txt", "gone.txt"] {
            std::fs::write(repo.join(f), "a=1\n").unwrap();
        }
        commit_all(&repo, "files");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/i").await;
        let clean = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        // Its size on disk is all it has.
        assert!(clean.bytes > 0);
        assert_eq!(
            WorktreeInventory { bytes: 0, ..clean },
            WorktreeInventory::default()
        );
        run(
            &wt,
            &[
                "update-index",
                "--assume-unchanged",
                "config.local",
                "same.txt",
            ],
        );
        run(
            &wt,
            &["update-index", "--skip-worktree", "skip.txt", "gone.txt"],
        );
        std::fs::write(wt.join("config.local"), "a=2 local edit\n").unwrap();
        std::fs::write(wt.join("skip.txt"), "edited\n").unwrap();
        std::fs::remove_file(wt.join("gone.txt")).unwrap();
        std::fs::write(wt.join(".env"), "KEY=secret").unwrap();
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        for f in ["target/debug/a", "target/debug/b", "target/c"] {
            std::fs::write(wt.join(f), "x").unwrap();
        }
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join("src/x.o"), "x").unwrap();
        std::fs::create_dir_all(wt.join("new/deep")).unwrap();
        std::fs::write(wt.join("new/deep/one.txt"), "x").unwrap();
        std::fs::write(wt.join("new/two.txt"), "x").unwrap();
        assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 1);
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(inv.dirty.items, ["new/deep/one.txt", "new/two.txt"]);
        assert_eq!(inv.hidden.items, ["config.local", "skip.txt"]);
        assert_eq!(inv.ignored.items, [".env", "src/x.o", "target/"]);
        let group = |p: &str, files: u64| IgnoredGroup {
            path: p.into(),
            files,
            more: false,
        };
        // Build output only on Linux and macOS (elsewhere it is ignored files like the others,
        // below).
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            assert_eq!(inv.ignored_groups, [group(".env", 1), group("src/", 1)]);
            assert_eq!(inv.ignored_groups_total, 2);
            assert_eq!(inv.build_output.items, ["target/"]);
            assert!(inv.build_output_bytes > 0);
        } else {
            assert!(inv.build_output.is_empty() && inv.build_output_bytes == 0);
        }
        // Without a list of build output, it is ignored files like the others.
        let all = g
            .clone()
            .with_park_disposable(Vec::new())
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(
            all.ignored_groups,
            [group(".env", 1), group("src/", 1), group("target/", 3)]
        );
        assert!(all.build_output.is_empty() && all.build_output_bytes == 0);
        assert_eq!(all.bytes, inv.bytes + inv.build_output_bytes);
        // A directory that is not a git work tree: an error, never an empty inventory.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(g.worktree_inventory(repo.clone(), plain).await.is_err());
        // A worktree whose directory is gone: nothing in it, only what its record holds.
        let gone = g
            .worktree_inventory(repo.clone(), dir.path().join("nowhere"))
            .await
            .unwrap();
        assert_eq!(
            gone,
            WorktreeInventory {
                gone: true,
                ..Default::default()
            }
        );
    }

    /// Runs git in `dir` with a committer, no hooks and local file transport allowed (a
    /// submodule cloned by `git submodule` has no config of its own).
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let mut all = vec![
            "-c",
            "user.name=twr test",
            "-c",
            "user.email=twr-test@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "protocol.file.allow=always",
        ];
        all.extend_from_slice(args);
        run(dir, &all)
    }

    /// Repository `name` under `dir` with submodule `deep` (the repository `sub`).
    fn repo_with_submodule(dir: &Path, name: &str, sub: &Path) -> PathBuf {
        let repo = init_repo(&dir.join(name));
        git_in(
            &repo,
            &["submodule", "add", "-q", sub.to_str().unwrap(), "deep"],
        );
        commit_all(&repo, "a submodule");
        repo
    }

    /// A round trip: a worktree with a submodule (a commit only its repository in
    /// the worktree's record has), a nested repository with a commit of its own, an untracked
    /// and an ignored file, a commit only the branch's reflog has and one only the worktree's
    /// `HEAD` reflog has goes to the trash, survives the hardest garbage collection, and comes
    /// back (copied): git lists the worktree, `git status` works in it and in its
    /// submodule (its repository copied into the new record), the branch's reflog names the
    /// reflog-only commit again, and the trash's refs keep every commit until the item is
    /// purged; then an ordinary `git gc` keeps what the restored reflog names.
    #[tokio::test]
    async fn a_trashed_worktree_comes_back_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let lib = init_repo(&root.join("lib"));
        let repo = init_repo(&root.join("repo"));
        git_in(
            &repo,
            &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
        );
        std::fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        commit_all(&repo, "a submodule");
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        git_in(&wt, &["submodule", "update", "--init", "-q"]);
        let x = wt.join("libs/x");
        git_in(&x, &["checkout", "-q", "-b", "mine"]);
        git_in(
            &x,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "only in the submodule",
            ],
        );
        let sub_commit = run(&x, &["rev-parse", "HEAD"]);
        let nested = init_repo(&wt.join("tools/n"));
        let nested_commit = run(&nested, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("w.txt"), "w").unwrap();
        commit_all(&wt, "work");
        let amended = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["commit", "-q", "--amend", "-m", "work, again"]);
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        commit_all(&wt, "detached");
        let detached = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["checkout", "-q", "twr/t"]);
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        std::fs::write(wt.join("new.txt"), "untracked").unwrap();
        std::fs::write(wt.join("x.log"), "ignored").unwrap();

        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        let item = out.item.clone().unwrap();
        let kept_log = run(
            &repo,
            &[
                "reflog",
                "--format=%H",
                &format!("refs/twr-trash/{item}/branch"),
            ],
        );
        assert!(kept_log.contains(&amended), "{kept_log}");
        let kept_head = run(
            &repo,
            &[
                "reflog",
                "--format=%H",
                &format!("refs/twr-trash/{item}/head"),
            ],
        );
        assert!(kept_head.contains(&detached), "{kept_head}");
        gc_now(&repo);
        for c in [&amended, &detached, &tip] {
            assert!(has_object(&repo, c), "{c}");
        }
        let back = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(back.worktree, wt);
        assert!(back.notes.is_empty(), "{back:?}");
        assert!(worktrees(&repo).contains(&wt));
        assert_eq!(run(&wt, &["rev-parse", "HEAD"]), tip);
        let status = run(&wt, &["status", "--porcelain", "--ignored"]);
        for want in ["?? new.txt", "!! x.log"] {
            assert!(status.contains(want), "{want}: {status}");
        }
        assert_eq!(run(&x, &["rev-parse", "HEAD"]), sub_commit);
        run(&x, &["status"]);
        assert_eq!(run(&nested, &["rev-parse", "HEAD"]), nested_commit);
        for c in [&amended, &detached] {
            assert!(has_object(&repo, c), "{c}");
        }
        // (The collection above expired every reflog entry of the repository; the item's
        // verbatim copy of the branch's came back.)
        let reflog = run(&repo, &["reflog", "--format=%H %gs", "twr/t"]);
        assert!(reflog.contains("restored from the trash"), "{reflog}");
        assert!(reflog.contains(&amended), "{reflog}");
        assert!(trash_refs(&repo).contains(&item) && list(&g).await.len() == 1);
        let p = g
            .trash_purge(TrashPurge {
                items: vec![item.clone()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(p.purged.len(), 1, "{p:?}");
        run(&repo, &["gc", "-q", "--prune=now"]);
        assert!(has_object(&repo, &amended));
        assert_eq!(run(&x, &["rev-parse", "HEAD"]), sub_commit);
    }

    /// A worktree with submodules (one nested in the other) restored where its record gets
    /// another name, at the next place offered (its own taken by another worktree of the
    /// repository, with submodules of its own) and under another name: each submodule works on
    /// its repository's copy in the new record, never on the other worktree's, with its
    /// commit only that copy has.
    #[tokio::test]
    async fn a_restore_elsewhere_connects_the_submodules_to_their_copies() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let leaf = init_repo(&root.join("leaf"));
        let lib = repo_with_submodule(&root, "lib", &leaf);
        let repo = init_repo(&root.join("repo"));
        git_in(
            &repo,
            &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
        );
        commit_all(&repo, "a submodule");
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        git_in(&wt, &["submodule", "update", "--init", "--recursive", "-q"]);
        let (x, deep) = (PathBuf::from("libs/x"), PathBuf::from("libs/x/deep"));
        let mine = |w: &Path, sub: &Path| {
            git_in(
                &w.join(sub),
                &["commit", "-q", "--allow-empty", "-m", "mine"],
            );
            run(&w.join(sub), &["rev-parse", "HEAD"])
        };
        let (x_commit, deep_commit) = (mine(&wt, &x), mine(&wt, &deep));
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        // Its place taken by another worktree, which git gives the old record's name.
        let other = new_wt(&g, &repo, &wt, "twr/other").await;
        git_in(
            &other,
            &["submodule", "update", "--init", "--recursive", "-q"],
        );
        let other_x = run(&other.join(&x), &["rev-parse", "HEAD"]);
        let wt_b = root.join("worktrees/proj/t-2");
        let back = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone(), wt_b.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(back.worktree, wt_b);
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        let connected = |w: &Path| {
            let record = PathBuf::from(run(w, &["rev-parse", "--absolute-git-dir"]));
            for (sub, commit) in [(&x, &x_commit), (&deep, &deep_commit)] {
                let at = w.join(sub);
                assert_eq!(
                    run(&at, &["rev-parse", "--show-toplevel"]),
                    at.to_str().unwrap()
                );
                let gitdir = PathBuf::from(run(&at, &["rev-parse", "--absolute-git-dir"]));
                assert!(gitdir.starts_with(record.join("modules")), "{gitdir:?}");
                assert_eq!(&run(&at, &["rev-parse", "HEAD"]), commit, "{sub:?}");
                run(&at, &["status"]);
            }
            run(w, &["status"]);
        };
        connected(&wt_b);
        // The other worktree's submodules are as they were.
        assert_eq!(run(&other.join(&x), &["rev-parse", "HEAD"]), other_x);
        assert_eq!(
            run(&other.join(&x), &["rev-parse", "--show-toplevel"]),
            other.join(&x).to_str().unwrap()
        );

        // Deleted again and restored under another name (`twr trash restore --as`).
        let out = to_trash(&g, &repo, &wt_b, Some("twr/t")).await.unwrap();
        let again = out.item.unwrap();
        let renamed = root.join("worktrees/proj/renamed");
        let back = restore_at(&g, &again, "twr/renamed", &renamed)
            .await
            .unwrap();
        assert!(back.notes.is_empty(), "{:?}", back.notes);
        connected(&renamed);
    }

    /// The submodule repositories of a record's `modules/` are all found (names with
    /// slashes, nested ones), or an error says why not: more than the cap, a folder that
    /// cannot be listed.
    #[test]
    fn module_repositories_are_all_found_or_none() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let modules = dir.path().join("modules");
        let repo = |rel: &str| {
            let d = modules.join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("HEAD"), "x").unwrap();
            std::fs::write(d.join("config"), "").unwrap();
        };
        repo("libs/x");
        repo("libs/x/modules/deep");
        repo("y");
        assert_eq!(
            trash::module_repositories(&modules).unwrap(),
            ["modules/libs/x", "modules/libs/x/modules/deep", "modules/y"].map(PathBuf::from)
        );
        let locked = modules.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_err() {
            assert!(trash::module_repositories(&modules).is_err());
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A marker or a folder that is a symlink, a repository with one marker of two.
        std::os::unix::fs::symlink(modules.join("y/HEAD"), modules.join("locked/HEAD")).unwrap();
        std::fs::write(modules.join("locked/config"), "").unwrap();
        assert!(trash::module_repositories(&modules).is_err());
        std::fs::remove_file(modules.join("locked/HEAD")).unwrap();
        assert!(trash::module_repositories(&modules).is_err());
        std::fs::remove_file(modules.join("locked/config")).unwrap();
        std::os::unix::fs::symlink(modules.join("y"), modules.join("locked/z")).unwrap();
        assert!(trash::module_repositories(&modules).is_err());
        std::fs::remove_file(modules.join("locked/z")).unwrap();
        assert_eq!(trash::module_repositories(&modules).unwrap().len(), 3);
        // Folders named `config` or `HEAD` in a submodule's name are no markers.
        repo("group/config/child");
        repo("ns/HEAD/y");
        let found = trash::module_repositories(&modules).unwrap();
        assert!(
            found.contains(&PathBuf::from("modules/group/config/child"))
                && found.contains(&PathBuf::from("modules/ns/HEAD/y")),
            "{found:?}"
        );
        std::fs::remove_dir_all(modules.join("group")).unwrap();
        std::fs::remove_dir_all(modules.join("ns")).unwrap();
        for i in 0..6 {
            repo(&format!("more/r{i}"));
        }
        let e = trash::module_repositories(&modules).unwrap_err();
        assert!(e.to_string().contains("more than 8"), "{e}");
    }

    /// Submodules a restore takes as they are, at the old path and at the next one: one
    /// deinitialized (its repository kept, without `core.worktree`, no checkout), and ones
    /// whose names hold a `config` or `HEAD` folder (`group/config/child`), connected.
    #[tokio::test]
    async fn a_restore_takes_deinitialized_submodules_and_any_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let leaf = init_repo(&root.join("leaf"));
        let lib = repo_with_submodule(&root, "lib", &leaf);
        let repo = init_repo(&root.join("repo"));
        let url = |r: &Path| r.to_str().unwrap().to_string();
        for (name, at, from) in [
            ("libs/x", "libs/x", url(&lib)),
            ("group/config/child", "odd", url(&leaf)),
            ("ns/HEAD/y", "odd2", url(&leaf)),
        ] {
            git_in(
                &repo,
                &["submodule", "add", "-q", "--name", name, &from, at],
            );
        }
        commit_all(&repo, "submodules");
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        git_in(&wt, &["submodule", "update", "--init", "--recursive", "-q"]);
        git_in(&wt, &["submodule", "deinit", "-q", "-f", "libs/x"]);
        let record = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        let kept = std::fs::read_to_string(record.join("modules/libs/x/config")).unwrap();
        assert!(!kept.contains("worktree"), "{kept}");
        let check = |w: &Path| {
            assert!(!w.join("libs/x/.git").exists());
            for at in ["odd", "odd2"] {
                let at = w.join(at);
                assert_eq!(
                    run(&at, &["rev-parse", "--show-toplevel"]),
                    at.to_str().unwrap()
                );
                let gitdir = PathBuf::from(run(&at, &["rev-parse", "--absolute-git-dir"]));
                let mine = PathBuf::from(run(w, &["rev-parse", "--absolute-git-dir"]));
                assert!(gitdir.starts_with(mine.join("modules")), "{gitdir:?}");
            }
            run(w, &["status"]);
        };
        // At the old path.
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let back = restore_at(&g, &out.item.unwrap(), "twr/t", &wt)
            .await
            .unwrap();
        assert_eq!(back.worktree, wt);
        check(&wt);
        // At the next one, the old taken by another worktree.
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let other = new_wt(&g, &repo, &wt, "twr/other").await;
        git_in(&other, &["submodule", "update", "--init", "-q", "odd"]);
        let wt_b = root.join("worktrees/proj/t-2");
        let back = g
            .trash_restore(RestoreRequest {
                item: out.item.unwrap(),
                branch: "twr/t".into(),
                paths: vec![wt.clone(), wt_b.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert_eq!(back.worktree, wt_b);
        check(&wt_b);
        assert_eq!(
            run(&other.join("odd"), &["rev-parse", "--show-toplevel"]),
            other.join("odd").to_str().unwrap()
        );
    }

    /// A worktree record's `HEAD` and refs, which its agent may write (Codex `auto`), are read
    /// capped by a move to the trash: a huge file among its refs fails the move, nothing
    /// moved, instead of being read whole into memory.
    #[tokio::test]
    async fn a_trash_move_reads_the_records_refs_capped() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        std::fs::create_dir_all(admin.join("refs/worktree")).unwrap();
        let mut body = run(&wt, &["rev-parse", "HEAD"]).into_bytes();
        body.resize(1 << 20, b'0');
        std::fs::write(admin.join("refs/worktree/big"), body).unwrap();
        let e = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap_err();
        assert!(format!("{e:#}").contains("over 65536 bytes"), "{e:#}");
        assert!(wt.exists());
        std::fs::remove_file(admin.join("refs/worktree/big")).unwrap();
        to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        assert!(!wt.exists());
    }

    /// The `.git` files a restore looks for in the worktree are read through their
    /// directories held, capped: an agent's huge file named `.git` deep in its worktree fails
    /// the restore instead of being read whole into memory; a symlink or a FIFO is not read.
    #[test]
    fn a_restore_reads_nested_git_files_capped() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("libs/x")).unwrap();
        std::fs::write(root.join("libs/x/.git"), "gitdir: ../../m/x\n").unwrap();
        std::fs::create_dir_all(root.join("aside")).unwrap();
        symlink(root.join("libs/x/.git"), root.join("aside/.git")).unwrap();
        assert_eq!(
            trash::gitfiles(root).unwrap(),
            [(PathBuf::from("libs/x"), PathBuf::from("../../m/x"))]
        );
        // Over 64 KiB, though it starts like a `.git` file.
        let big = root.join("deep/down");
        std::fs::create_dir_all(&big).unwrap();
        let mut body = b"gitdir: ../m".to_vec();
        body.resize(1 << 20, b'a');
        std::fs::write(big.join(".git"), body).unwrap();
        let e = trash::gitfiles(root).unwrap_err().to_string();
        assert!(e.contains("over 65536 bytes"), "{e}");
    }

    /// A restore that cannot connect every submodule to its repository's copy fails and
    /// removes what it made, the item untouched and the other worktree that took the old
    /// place as it was: more submodule repositories than it takes, one whose config git
    /// cannot write (a copied `config.lock`), a submodule directory swapped for a symlink
    /// while it is connected (nothing written through it). Then it restores.
    #[tokio::test]
    async fn a_restore_that_cannot_connect_every_submodule_makes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let leaf = init_repo(&root.join("leaf"));
        let lib = repo_with_submodule(&root, "lib", &leaf);
        let repo = init_repo(&root.join("repo"));
        git_in(
            &repo,
            &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
        );
        commit_all(&repo, "a submodule");
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        git_in(&wt, &["submodule", "update", "--init", "--recursive", "-q"]);
        let x = PathBuf::from("libs/x");
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let (item, item_dir) = (out.item.unwrap(), out.dir.unwrap());
        let other = new_wt(&g, &repo, &wt, "twr/other").await;
        git_in(
            &other,
            &["submodule", "update", "--init", "--recursive", "-q"],
        );
        let other_x = run(&other.join(&x), &["rev-parse", "HEAD"]);
        let wt_b = root.join("worktrees/proj/t-2");
        let restore = |g: LocalGit| {
            let (item, paths) = (item.clone(), vec![wt.clone(), wt_b.clone()]);
            async move {
                g.trash_restore(RestoreRequest {
                    item,
                    branch: "twr/t".into(),
                    paths,
                    other_branches: Some(Vec::new()),
                })
                .await
            }
        };
        let nothing_made = |why: &str| {
            assert!(!wt_b.exists(), "{why}");
            assert!(!branch_exists(&repo, "twr/t"), "{why}");
            assert!(!worktrees(&repo).contains(&wt_b), "{why}");
            assert_eq!(
                run(&other.join(&x), &["rev-parse", "HEAD"]),
                other_x,
                "{why}"
            );
            assert_eq!(
                run(&other.join(&x), &["rev-parse", "--show-toplevel"]),
                other.join(&x).to_str().unwrap(),
                "{why}"
            );
        };
        let modules = item_dir.join("record/modules");
        // More repositories than a restore connects (the cap is lower in tests).
        for i in 0..8 {
            let fake = modules.join(format!("fake{i}"));
            std::fs::create_dir_all(&fake).unwrap();
            std::fs::write(fake.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            std::fs::write(fake.join("config"), "").unwrap();
        }
        let before = snapshot(&item_dir);
        let e = restore(g.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("more than 8"), "{e:#}");
        nothing_made("too many");
        assert_eq!(snapshot(&item_dir), before);
        for i in 0..8 {
            std::fs::remove_dir_all(modules.join(format!("fake{i}"))).unwrap();
        }
        // A repository's config a symlink (to a copy of itself): not found as a repository,
        // which is an error, not a submodule left out.
        let config = modules.join("libs/x/config");
        let copy = root.join("config-copy");
        std::fs::rename(&config, &copy).unwrap();
        std::os::unix::fs::symlink(&copy, &config).unwrap();
        let e = restore(g.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("not a regular file"), "{e:#}");
        nothing_made("symlinked config");
        std::fs::remove_file(&config).unwrap();
        std::fs::rename(&copy, &config).unwrap();
        // A nested submodule's repository missing from the item: its `.git` file still names
        // the old record.
        let deep_repo = modules.join("libs/x/modules/deep");
        let aside = root.join("deep-aside");
        std::fs::rename(&deep_repo, &aside).unwrap();
        let e = restore(g.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("did not find"), "{e:#}");
        nothing_made("missing repository");
        std::fs::rename(&aside, &deep_repo).unwrap();
        // A config git cannot write.
        let lock = modules.join("libs/x/config.lock");
        std::fs::write(&lock, "").unwrap();
        let e = restore(g.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("core.worktree"), "{e:#}");
        nothing_made("config.lock");
        std::fs::remove_file(&lock).unwrap();
        // The submodule's directory swapped for a symlink to one with a `.git` file of its
        // own while it is connected.
        let decoy = root.join("decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join(".git"), "gitdir: /nowhere\n").unwrap();
        let moved = root.join("moved");
        let swapping = {
            let (decoy, moved) = (decoy.clone(), moved.clone());
            g.clone().with_step_hook(move |step, at| {
                if step == trash::Step::Connecting && at.ends_with("libs/x") {
                    std::fs::rename(at, &moved)?;
                    std::os::unix::fs::symlink(&decoy, at)?;
                }
                Ok(())
            })
        };
        let e = restore(swapping).await.unwrap_err();
        assert!(format!("{e:#}").contains("symbolic link"), "{e:#}");
        nothing_made("swapped");
        assert_eq!(
            std::fs::read_to_string(decoy.join(".git")).unwrap(),
            "gitdir: /nowhere\n"
        );
        // Nothing in the way: restored, connected.
        let back = restore(g.clone()).await.unwrap();
        assert_eq!(back.worktree, wt_b);
        let at = wt_b.join(&x);
        assert_eq!(
            run(&at, &["rev-parse", "--show-toplevel"]),
            at.to_str().unwrap()
        );
        run(&at.join("deep"), &["status"]);
    }

    /// A submodule's repository lives under the linked worktree's own
    /// git directory (`worktrees/<id>/modules/<name>`) and goes with it. Its commits on none
    /// of its remote-tracking branches are listed, also with the superproject clean (the
    /// gitlink checked out back) and when its work tree is gone; a submodule in a submodule
    /// likewise; a checked-out submodule's files count under its path.
    #[tokio::test]
    async fn the_inventory_counts_what_submodule_repositories_lose() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let leaf = init_repo(&root.join("leaf"));
        let lib = repo_with_submodule(&root, "lib", &leaf);
        let repo = init_repo(&root.join("repo"));
        git_in(
            &repo,
            &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
        );
        commit_all(&repo, "a submodule");
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/s").await;
        git_in(&wt, &["submodule", "update", "--init", "--recursive", "-q"]);
        let x = wt.join("libs/x");
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        assert!(admin.join("modules/libs/x/modules/deep/HEAD").is_file());
        assert_eq!(
            WorktreeInventory {
                bytes: 0,
                ..g.worktree_inventory(repo.clone(), wt.clone())
                    .await
                    .unwrap()
            },
            WorktreeInventory::default(),
            "freshly cloned submodules lose nothing"
        );

        // A commit on a local branch of the submodule, the gitlink checked out back.
        git_in(&x, &["checkout", "-q", "-b", "mine"]);
        git_in(&x, &["commit", "-q", "--allow-empty", "-m", "secret"]);
        let secret = run(&x, &["rev-parse", "HEAD"]);
        git_in(&x, &["checkout", "-q", "--detach", "HEAD~1"]);
        git_in(
            &x.join("deep"),
            &["commit", "-q", "--allow-empty", "-m", "deeper"],
        );
        let deeper = run(&x.join("deep"), &["rev-parse", "HEAD"]);
        git_in(&x.join("deep"), &["checkout", "-q", "--detach", "HEAD~1"]);
        assert_eq!(run(&wt, &["status", "--porcelain"]), "");
        std::fs::write(x.join("notes.txt"), "mine").unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let sub = |path: &str, commit: &str| RepoLoss {
            path: path.into(),
            commits: Listed::new([commit.to_string()]),
            ..Default::default()
        };
        assert_eq!(
            inv.repositories,
            [sub("libs/x", &secret), sub("libs/x/deep", &deeper)]
        );
        // The superproject shows the submodule changed; its file is listed under its path.
        assert_eq!(inv.dirty.items, ["libs/x", "libs/x/notes.txt"]);

        // Its work tree gone (not checked out): its repository is still there, and goes.
        std::fs::remove_dir_all(&x).unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let gitdir = |p: &Path| quote_path(p.as_os_str().as_bytes());
        assert_eq!(
            inv.repositories,
            [
                sub(&gitdir(&admin.join("modules/libs/x")), &secret),
                sub(&gitdir(&admin.join("modules/libs/x/modules/deep")), &deeper),
            ]
        );
        // A repository that cannot be read is not checked, never clean.
        std::fs::write(admin.join("modules/libs/x/HEAD"), "junk\n").unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let why = inv.repositories[0].unchecked.clone().unwrap();
        assert!(why.starts_with("its commits ("), "{why}");
        assert!(inv.repositories[0].path.ends_with("modules/libs/x"));

        // Moving the worktree to the trash takes them with it, their commits kept there.
        let out = to_trash(&with_trash(&g, dir.path()), &repo, &wt, Some("twr/s"))
            .await
            .unwrap();
        assert!(!admin.exists());
        let kept = out.dir.unwrap().join("record").join("modules");
        assert!(kept.exists(), "the submodule repositories are in the item");
    }

    /// Every repository the forced removal deletes counts, not only submodules:
    /// an untracked one (`? tools/x/`) and one deep in an ignored directory lose their commits
    /// (on no remote) and list their files under their paths. A linked worktree of a repository
    /// outside, nested in an ignored directory, is no repository that goes: its files count,
    /// and it is named as what refuses the deletion (it loses no commits, but its record
    /// outside would be left naming a missing checkout). Ignored
    /// directories past the search budget are not searched to the end: said, never "no
    /// repositories".
    #[tokio::test]
    async fn the_inventory_counts_what_nested_repositories_lose() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        std::fs::write(repo.join(".gitignore"), "vendor/\nbig/\n").unwrap();
        commit_all(&repo, "ignores");
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/n").await;
        let head = |d: &Path| run(d, &["rev-parse", "HEAD"]);
        let tools = init_repo(&wt.join("tools/x"));
        std::fs::write(tools.join("wip.txt"), "wip").unwrap();
        let deep = init_repo(&wt.join("vendor/a/b"));
        std::fs::write(deep.join("notes.txt"), "mine").unwrap();
        let outside = init_repo(&root.join("outside"));
        let lw = wt.join("vendor/lw");
        run(
            &outside,
            &["worktree", "add", "-q", "-b", "lw", lw.to_str().unwrap()],
        );
        std::fs::write(lw.join("x.txt"), "x").unwrap();
        commit_all(&lw, "on the outside repository");
        let outside_commit = head(&lw);
        std::fs::write(lw.join("scratch.txt"), "scratch").unwrap();
        std::fs::create_dir_all(wt.join("big")).unwrap();
        for f in ["1", "2", "3"] {
            std::fs::write(wt.join("big").join(f), "x").unwrap();
        }

        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let nested = |path: &str, commit: String| RepoLoss {
            path: path.into(),
            nested: true,
            commits: Listed::new([commit]),
            unchecked: None,
        };
        assert_eq!(
            inv.repositories,
            [
                nested("tools/x", head(&tools)),
                nested("vendor/a/b", head(&deep))
            ]
        );
        assert!(inv.unsearched.is_empty(), "{inv:?}");
        assert_eq!(
            inv.dirty.items,
            [
                "tools/x/",
                "tools/x/wip.txt",
                "vendor/a/b/notes.txt",
                "vendor/lw/scratch.txt"
            ]
        );
        assert_eq!(inv.linked.len(), 1, "{inv:?}");
        assert_eq!(inv.linked[0].path, lw.display().to_string());

        // Past the budget: the work tree is said not searched to the end; the untracked
        // repository (from `git status`) still counts.
        let small = LocalGit::default().with_search_max(1);
        let inv = small
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(inv.unsearched, ["./"]);
        assert_eq!(inv.repositories, [nested("tools/x", head(&tools))]);

        // The trash refuses while the linked worktree is there; moved away, it takes the
        // repositories, their commits with them, and the outside repository keeps its commit.
        let (tools_head, deep_head) = (head(&tools), head(&deep));
        let trash = with_trash(&g, dir.path());
        let e = to_trash(&trash, &repo, &wt, Some("twr/n"))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("vendor/lw"), "{e:#}");
        run(
            &outside,
            &[
                "worktree",
                "move",
                lw.to_str().unwrap(),
                root.join("lw").to_str().unwrap(),
            ],
        );
        let out = to_trash(&trash, &repo, &wt, Some("twr/n")).await.unwrap();
        assert!(!wt.exists());
        let item = out.dir.unwrap().join("worktree");
        assert_eq!(head(&item.join("tools/x")), tools_head);
        assert_eq!(head(&item.join("vendor/a/b")), deep_head);
        assert_eq!(
            run(&outside, &["rev-parse", "lw"]),
            outside_commit,
            "a linked worktree's objects stay with its repository"
        );
    }

    /// `git status` shows no repository inside a tracked directory (nor does it
    /// show the directory changed), yet removing the worktree deletes it: the walk of the
    /// whole directory finds it, and a deeper one, with their commits on no remote. A bare
    /// repository whose files are all tracked and unchanged (a test fixture) loses nothing;
    /// one file more in it, and it counts. A park keeps the worktree; the forced deletion
    /// takes it.
    #[tokio::test]
    async fn the_inventory_finds_repositories_in_tracked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        // Tracked files, the same `README.md` as the repositories made there below write.
        std::fs::create_dir_all(repo.join("tracked/deep")).unwrap();
        for f in ["tracked/README.md", "tracked/deep/README.md"] {
            std::fs::write(repo.join(f), "test repo\n").unwrap();
        }
        std::fs::write(repo.join("tracked/f"), "f").unwrap();
        let src = init_repo(&root.join("src"));
        let fixture = repo.join("fixtures/x.git");
        std::fs::create_dir_all(fixture.parent().unwrap()).unwrap();
        run(
            &root,
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                fixture.to_str().unwrap(),
            ],
        );
        // Git tracks no empty directory: kept so that the fixture is a git directory.
        for d in ["refs/heads", "refs/tags"] {
            std::fs::write(fixture.join(d).join(".keep"), "").unwrap();
        }
        commit_all(&repo, "tracked files and a fixture");
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/t").await;
        assert!(is_git_dir(&wt.join("fixtures/x.git")));
        let head = |d: &Path| run(d, &["rev-parse", "HEAD"]);
        let outer = init_repo(&wt.join("tracked"));
        let inner = init_repo(&wt.join("tracked/deep"));
        assert_eq!(run(&wt, &["status", "--porcelain", "--ignored"]), "");

        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let nested = |path: &str, commit: String| RepoLoss {
            path: path.into(),
            nested: true,
            commits: Listed::new([commit]),
            unchecked: None,
        };
        assert_eq!(
            inv.repositories,
            [
                nested("tracked", head(&outer)),
                nested("tracked/deep", head(&inner))
            ]
        );
        assert!(inv.unsearched.is_empty(), "{inv:?}");
        // A park would delete them: it keeps the worktree.
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t".into()))
                .await
                .unwrap(),
        );
        assert!(
            reason.contains("1 git repository in it that git status does not show")
                && reason.contains("tracked/"),
            "{reason}"
        );
        assert!(outer.join(".git").exists());

        // The fixture with a file git does not track: its commits count.
        std::fs::write(wt.join("fixtures/x.git/notes.txt"), "mine").unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert!(
            inv.repositories
                .iter()
                .any(|r| r.path == "fixtures/x.git" && r.nested && r.commits.items == [head(&src)]),
            "{inv:?}"
        );

        to_trash(&with_trash(&g, dir.path()), &repo, &wt, Some("twr/t"))
            .await
            .unwrap();
        assert!(!wt.exists());
    }

    /// A park counts what goes with the worktree's record as a deletion does: a
    /// commit only one of its own refs (`refs/worktree/...`) reaches, its `HEAD` reflog off and
    /// empty, keeps the worktree, and its stale record once the directory is gone.
    #[tokio::test]
    async fn a_worktree_ref_keeps_the_worktree_and_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("worktrees/p/t1");
        let wtc = new_wt(&g, &repo, &wt, "twr/t1").await;
        let lost = lost_on_a_detached_head_without_reflog(&wt);
        run(&wt, &["update-ref", "refs/worktree/keep", &lost]);
        run(&wt, &["checkout", "-q", "twr/t1"]);
        // Git logs to a reflog that exists, whatever the config: removed.
        let log = wt.join(run(&wt, &["rev-parse", "--git-path", "logs/HEAD"]));
        let _ = std::fs::remove_file(log);
        assert!(run(&wt, &["rev-list", "--reflog", "--not", "--branches"]).is_empty());
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
                .await
                .unwrap(),
        );
        assert!(reason.contains(&lost[..12]), "{reason}");
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
        // Once a branch reaches it, the record goes.
        run(&repo, &["branch", "keep", &lost]);
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(!worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
    }

    /// A repository is any git directory the deletion removes, bare
    /// ones too: an ignored one deep in an ignored directory, an untracked one (whose files
    /// `git status` lists one by one), and one put in the worktree's record (outside its
    /// `modules/` too) lose their commits on no remote; a bare one's files are its own. With
    /// the directory gone, the record's repositories still count; the deletion removes them.
    #[tokio::test]
    async fn the_inventory_counts_what_bare_repositories_lose() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        std::fs::write(
            repo.join(".gitignore"),
            "vendor/
",
        )
        .unwrap();
        commit_all(&repo, "ignores");
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/b").await;
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        let src = init_repo(&root.join("src"));
        let private = run(&src, &["rev-parse", "HEAD"]);
        // A bare clone has no remote-tracking branches: its commit is on none of them.
        let bare = |to: &Path| {
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            run(
                &root,
                &[
                    "clone",
                    "-q",
                    "--bare",
                    src.to_str().unwrap(),
                    to.to_str().unwrap(),
                ],
            );
            assert!(is_git_dir(to), "{}", to.display());
        };
        bare(&wt.join("vendor/old/backup.git"));
        bare(&wt.join("spare.git"));
        bare(&admin.join("modules/vault.git"));
        bare(&admin.join("stash/kept.git"));
        assert!(!is_git_dir(&wt.join("vendor")) && !is_git_dir(&wt));

        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        let lost = |path: &str, nested: bool| RepoLoss {
            path: path.into(),
            nested,
            commits: Listed::new([private.clone()]),
            unchecked: None,
        };
        let gitdir = |p: &Path| quote_path(p.as_os_str().as_bytes());
        let mut want = vec![
            lost(&gitdir(&admin.join("modules/vault.git")), false),
            lost(&gitdir(&admin.join("stash/kept.git")), false),
            lost("spare.git", true),
            lost("vendor/old/backup.git", true),
        ];
        want.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(inv.repositories, want);
        assert!(!inv.gone);
        assert!(
            inv.dirty.items.iter().any(|f| f == "spare.git/HEAD"),
            "{:?}",
            inv.dirty
        );

        // The directory gone: what its record holds still goes, and counts.
        std::fs::remove_dir_all(&wt).unwrap();
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert!(inv.gone && inv.dirty.is_empty(), "{inv:?}");
        assert_eq!(
            inv.repositories,
            [
                lost(&gitdir(&admin.join("modules/vault.git")), false),
                lost(&gitdir(&admin.join("stash/kept.git")), false),
            ]
        );
        let out = to_trash(&with_trash(&g, dir.path()), &repo, &wt, Some("twr/b"))
            .await
            .unwrap();
        assert!(!admin.exists());
        assert!(out.dir.unwrap().join("record/modules/vault.git").exists());
    }

    /// What tells a git directory: `HEAD` naming a ref or holding an object id, `objects/`
    /// and `refs/`; a work tree, a plain directory or a half of one is none.
    #[test]
    fn git_directories_are_told_by_their_shape() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let make = |name: &str, head: Option<&str>, objects: bool, refs: bool| {
            let g = d.join(name);
            std::fs::create_dir_all(&g).unwrap();
            if let Some(h) = head {
                std::fs::write(g.join("HEAD"), h).unwrap();
            }
            if objects {
                std::fs::create_dir(g.join("objects")).unwrap();
            }
            if refs {
                std::fs::create_dir(g.join("refs")).unwrap();
            }
            g
        };
        let id = "0123456789abcdef0123456789abcdef01234567\n";
        assert!(is_git_dir(&make(
            "a",
            Some("ref: refs/heads/main\n"),
            true,
            true
        )));
        assert!(is_git_dir(&make("b", Some(id), true, true)));
        assert!(is_git_dir(&make(
            "c",
            Some("ref:  refs/heads/x"),
            true,
            true
        )));
        assert!(!is_git_dir(&make("d", Some("ref: HEAD\n"), true, true)));
        assert!(!is_git_dir(&make("e", Some("junk\n"), true, true)));
        assert!(!is_git_dir(&make("f", Some(id), false, true)));
        assert!(!is_git_dir(&make("g", Some(id), true, false)));
        assert!(!is_git_dir(&make("h", None, true, true)));
        let worktree = init_repo(&d.join("w"));
        assert!(!is_git_dir(&worktree) && is_git_dir(&worktree.join(".git")));
        // What is left of one (an object store with a HEAD or refs) still holds a repository's
        // objects: it counts, its commits not checked. Without an object store, nothing does.
        for (name, counts) in [("e", true), ("g", true), ("h", true), ("f", false)] {
            assert_eq!(repository_at(&d.join(name)), Ok(counts), "{name}");
        }
        assert_eq!(repository_at(&d.join("w")), Ok(false));
        // A symlinked `objects` (a shared object store) is taken for the directory git takes
        // it for, not followed: also one that leads nowhere.
        let shared = make("s", Some(id), false, true);
        std::os::unix::fs::symlink(d.join("a/objects"), shared.join("objects")).unwrap();
        assert!(is_git_dir(&shared));
        let dangling = make("t", Some(id), false, false);
        std::os::unix::fs::symlink(d.join("nowhere"), dangling.join("objects")).unwrap();
        assert_eq!(repository_at(&dangling), Ok(true));
    }

    /// File names that are not UTF-8 stay apart (none is replaced),
    /// a literal backslash is not taken for an escaped byte, and an ignored directory with
    /// such a name is counted.
    #[tokio::test]
    async fn the_inventory_keeps_names_that_are_not_utf8_apart() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitignore"), "build-*/\n").unwrap();
        commit_all(&repo, "ignores");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/n").await;
        let name = |b: &[u8]| wt.join(OsStr::from_bytes(b));
        if let Err(e) = std::fs::write(name(b"notes-\x80"), "a") {
            eprintln!("skipped: this file system refuses non-UTF-8 names ({e})");
            return;
        }
        std::fs::write(name(b"notes-\x81"), "b").unwrap();
        std::fs::write(name(b"notes-\\x80"), "c").unwrap();
        std::fs::create_dir(name(b"build-\xff")).unwrap();
        for f in [b"build-\xff/a".as_slice(), b"build-\xff/b"] {
            std::fs::write(name(f), "x").unwrap();
        }
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(
            inv.dirty.items,
            ["notes-\\\\x80", "notes-\\x80", "notes-\\x81"]
        );
        assert_eq!(inv.dirty.total, 3);
        assert_eq!(inv.ignored.items, ["build-\\xff/"]);
        assert_eq!(
            inv.ignored_groups,
            [IgnoredGroup {
                path: "build-\\xff/".into(),
                files: 2,
                more: false,
            }]
        );
        // The card from before the second name sees it as new.
        let before = Listed::new(["notes-\\x80".to_string()]);
        assert!(inv.dirty.added_since(&before));
    }

    /// The lost commits `branch` of `repo` reaches when it and its worktree `wt` are deleted.
    async fn branch_only(g: &LocalGit, repo: &Path, branch: &str, wt: &Path) -> Option<u64> {
        g.branch_only_commits(vec![target(repo, branch, Some(wt))])
            .await
            .unwrap()
            .remove(0)
            .branch
    }

    #[tokio::test]
    async fn create_snapshot_remove_and_recreate() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("worktrees").join("proj").join("t1");

        let made = g
            .worktree_create(spec(&repo, &wt, "twr/t1", None))
            .await
            .unwrap();
        assert!(made.new_branch);
        assert_eq!(made.head, run(&repo, &["rev-parse", "HEAD"]));
        assert!(g.worktree_exists(wt.clone()).await.unwrap());
        assert!(branch_exists(&repo, "twr/t1"));
        let wt = std::fs::canonicalize(&wt).unwrap();
        assert!(worktrees(&repo).contains(&wt));

        let s = g.snapshot(wt.clone(), None).await.unwrap();
        assert_eq!(s.branch.as_deref(), Some("twr/t1"));
        assert_eq!((s.dirty_files, s.ahead, s.behind), (0, 0, 0));

        // Dirty: an untracked file and a modified one. Kept, never forced.
        std::fs::write(wt.join("new.txt"), "x").unwrap();
        std::fs::write(wt.join("README.md"), "changed\n").unwrap();
        assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 2);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
                .await
                .unwrap(),
            RemoveOutcome::KeptDirty { dirty_files: 2 }
        );
        assert!(wt.join("new.txt").exists());

        // Committed on the branch: clean again, removed, branch kept with the commit.
        commit_all(&wt, "work");
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
        assert!(!wt.exists());
        assert!(!worktrees(&repo).contains(&wt));
        assert!(branch_exists(&repo, "twr/t1"));
        // Removing again finds nothing.
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
                .await
                .unwrap(),
            RemoveOutcome::Missing
        );

        // Recreated at the same path from the existing branch.
        let again = g
            .worktree_create(spec(&repo, &wt, "twr/t1", None))
            .await
            .unwrap();
        assert!(!again.new_branch);
        assert_eq!(again.head, tip);
        assert!(wt.join("new.txt").exists());
        // A base only applies to new branches.
        let err = g
            .worktree_create(spec(
                &repo,
                &dir.path().join("other"),
                "twr/t1",
                Some("main"),
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
    }

    #[tokio::test]
    async fn a_worktree_with_another_checkout_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("wt");
        g.worktree_create(spec(&repo, &wt, "twr/det", None))
            .await
            .unwrap();
        // Detach, commit there: only the worktree's HEAD reflog knows that commit.
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("detached.txt"), "work").unwrap();
        commit_all(&wt, "on a detached head");
        let detached = run(&wt, &["rev-parse", "--short", "HEAD"]);
        let out = g
            .worktree_remove(repo.clone(), wt.clone(), Some("twr/det".into()))
            .await
            .unwrap();
        assert_eq!(
            out,
            RemoveOutcome::KeptCheckout {
                head: format!("detached at {detached}")
            }
        );
        assert!(wt.join("detached.txt").exists());
        // Another branch checked out: kept too.
        run(&wt, &["checkout", "-q", "-b", "side"]);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/det".into()))
                .await
                .unwrap(),
            RemoveOutcome::KeptCheckout {
                head: "branch side".into()
            }
        );
        // Back on the thread's branch (clean): removed; without a branch nothing is checked.
        run(&wt, &["checkout", "-q", "twr/det"]);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/det".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
        let wt2 = dir.path().join("wt2");
        g.worktree_create(spec(&repo, &wt2, "twr/det2", None))
            .await
            .unwrap();
        run(&wt2, &["checkout", "-q", "--detach"]);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt2.clone(), None)
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
    }

    #[tokio::test]
    async fn a_deleted_worktree_directory_is_pruned_and_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("wt");
        g.worktree_create(spec(&repo, &wt, "twr/gone", None))
            .await
            .unwrap();
        std::fs::remove_dir_all(&wt).unwrap();
        assert!(!g.worktree_exists(wt.clone()).await.unwrap());
        // Still registered (prunable) until pruned; create prunes first.
        g.worktree_create(spec(&repo, &wt, "twr/gone", None))
            .await
            .unwrap();
        assert!(wt.join("README.md").exists());
        std::fs::remove_dir_all(&wt).unwrap();
        // Prune clears only records under the managed directory.
        g.worktree_prune(repo.clone()).await.unwrap();
        assert_eq!(worktrees(&repo).len(), 2);
        let managed = LocalGit::default().with_managed_root(dir.path().join("elsewhere"));
        managed.worktree_prune(repo.clone()).await.unwrap();
        assert_eq!(worktrees(&repo).len(), 2);
        let managed = LocalGit::default().with_managed_root(dir.path());
        managed.worktree_prune(repo.clone()).await.unwrap();
        assert_eq!(worktrees(&repo), vec![repo.clone()]);
    }

    #[tokio::test]
    async fn bases_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let first = run(&repo, &["rev-parse", "HEAD"]);
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        commit_all(&repo, "second");

        let wt = dir.path().join("based");
        let made = g
            .worktree_create(spec(&repo, &wt, "twr/based", Some(&first)))
            .await
            .unwrap();
        assert_eq!(made.head, first);
        assert!(!wt.join("b.txt").exists());

        for bad in ["no-such-ref", "--upload-pack=x", ""] {
            let e = g
                .worktree_create(spec(&repo, &dir.path().join("x"), "twr/x", Some(bad)))
                .await
                .unwrap_err();
            assert!(
                e.to_string().contains("not a commit") || e.to_string().contains("invalid"),
                "{bad:?}: {e:#}"
            );
        }
        assert!(!branch_exists(&repo, "twr/x"));
        let e = g
            .worktree_create(spec(&repo, &dir.path().join("y"), "twr/.bad", None))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("invalid branch"), "{e:#}");
    }

    #[tokio::test]
    async fn snapshots_track_ahead_behind_and_detached_heads() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("wt");
        g.worktree_create(spec(&repo, &wt, "twr/ab", None))
            .await
            .unwrap();
        run(&wt, &["branch", "--set-upstream-to=main"]);
        std::fs::write(wt.join("mine.txt"), "1").unwrap();
        commit_all(&wt, "ahead");
        std::fs::write(repo.join("theirs.txt"), "2").unwrap();
        commit_all(&repo, "behind 1");
        std::fs::write(repo.join("theirs2.txt"), "3").unwrap();
        commit_all(&repo, "behind 2");
        let s = g.snapshot(wt.clone(), None).await.unwrap();
        assert_eq!((s.ahead, s.behind), (1, 2));

        run(&wt, &["checkout", "-q", "--detach"]);
        let s = g.snapshot(wt.clone(), None).await.unwrap();
        assert_eq!(s.branch, None);

        // Not a repository at all.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(g.snapshot(plain, None).await.is_err());
    }

    async fn new_wt(g: &LocalGit, repo: &Path, wt: &Path, branch: &str) -> PathBuf {
        g.worktree_create(spec(repo, wt, branch, None))
            .await
            .unwrap();
        std::fs::canonicalize(wt).unwrap()
    }

    fn kept_reason(o: RemoveOutcome) -> String {
        match o {
            RemoveOutcome::Kept { reason } => reason,
            other => panic!("expected Kept, got {other:?}"),
        }
    }

    /// A user's `status.showUntrackedFiles=no` must not make untracked files look
    /// clean (removal would delete them).
    #[tokio::test]
    async fn untracked_files_count_whatever_the_user_config() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        run(
            &repo,
            &["config", "--local", "status.showUntrackedFiles", "no"],
        );
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/u").await;
        std::fs::write(wt.join("notes.txt"), "mine").unwrap();
        assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 1);
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/u").await,
            RemoveOutcome::KeptDirty { dirty_files: 1 }
        );
        assert!(wt.join("notes.txt").exists());
    }

    /// Tracked files hidden from `git status` (assume-unchanged, skip-worktree) keep
    /// the worktree; a sparse checkout's absent skip-worktree files do not.
    #[tokio::test]
    async fn hidden_tracked_files_keep_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join("config.local"), "a=1\n").unwrap();
        std::fs::write(repo.join("gone.txt"), "x\n").unwrap();
        commit_all(&repo, "more files");
        for flag in ["--assume-unchanged", "--skip-worktree"] {
            let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/h").await;
            run(&wt, &["update-index", flag, "config.local"]);
            std::fs::write(wt.join("config.local"), "a=2 local edit\n").unwrap();
            assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 0);
            let out = g
                .worktree_remove(repo.clone(), wt.clone(), Some("twr/h".into()))
                .await
                .unwrap();
            let reason = kept_reason(out);
            assert!(reason.contains("config.local"), "{flag}: {reason}");
            assert_eq!(
                std::fs::read_to_string(wt.join("config.local")).unwrap(),
                "a=2 local edit\n"
            );
            run(
                &wt,
                &["update-index", &flag.replace("--", "--no-"), "config.local"],
            );
            run(&wt, &["checkout", "--", "config.local"]);
            assert_eq!(
                g.worktree_remove(repo.clone(), wt.clone(), Some("twr/h".into()))
                    .await
                    .unwrap(),
                RemoveOutcome::Removed
            );
        }
        // Skip-worktree and absent (sparse): nothing to lose.
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/h").await;
        run(&wt, &["update-index", "--skip-worktree", "gone.txt"]);
        std::fs::remove_file(wt.join("gone.txt")).unwrap();
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/h".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
    }

    /// `git <args>` in `dir` with raw (possibly non-UTF-8) arguments.
    fn run_os(dir: &Path, args: &[&OsStr]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A hidden (skip-worktree / assume-unchanged) file whose name is not UTF-8
    /// is looked up by its real name; a replaced name would look absent and removal would
    /// delete its edits.
    #[tokio::test]
    async fn hidden_files_with_non_utf8_names_keep_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let name = OsStr::from_bytes(b"file-\xff.txt");
        if let Err(e) = std::fs::write(repo.join(name), "original\n") {
            // macOS (APFS) refuses names that aren't valid UTF-8, so the case can't arise there.
            eprintln!("skipped: this file system refuses non-UTF-8 names ({e})");
            return;
        }
        commit_all(&repo, "a non-UTF-8 name");
        for flag in ["--skip-worktree", "--assume-unchanged"] {
            let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/b").await;
            run_os(&wt, &[OsStr::new("update-index"), OsStr::new(flag), name]);
            std::fs::write(wt.join(name), "my edit\n").unwrap();
            assert_eq!(g.snapshot(wt.clone(), None).await.unwrap().dirty_files, 0);
            let reason = kept_reason(
                g.worktree_remove(repo.clone(), wt.clone(), Some("twr/b".into()))
                    .await
                    .unwrap(),
            );
            assert!(reason.contains("file-"), "{flag}: {reason}");
            assert_eq!(std::fs::read(wt.join(name)).unwrap(), b"my edit\n");
            let undo = flag.replace("--", "--no-");
            run_os(&wt, &[OsStr::new("update-index"), OsStr::new(&undo), name]);
            run_os(&wt, &[OsStr::new("checkout"), OsStr::new("--"), name]);
            assert_eq!(
                g.worktree_remove(repo.clone(), wt.clone(), Some("twr/b".into()))
                    .await
                    .unwrap(),
                RemoveOutcome::Removed
            );
        }
    }

    /// A stale record whose HEAD reflog has a non-UTF-8 message (a
    /// latin-1 commit subject) still has that reflog read: its unreachable commit keeps it.
    #[tokio::test]
    async fn a_stale_record_with_a_non_utf8_reflog_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("worktrees/p/t1");
        let wtc = new_wt(&g, &repo, &wt, "twr/t1").await;
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        run(&wt, &["add", "-A"]);
        let msg = dir.path().join("msg");
        std::fs::write(&msg, b"caf\xe9 latin-1\n").unwrap();
        run(&wt, &["commit", "-q", "-F", msg.to_str().unwrap()]);
        run(&wt, &["checkout", "-q", "twr/t1"]);
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
    }

    /// Commits made on a detached HEAD, then abandoned by checking the thread's branch
    /// out again, are remembered only by the worktree's HEAD reflog.
    #[tokio::test]
    async fn commits_only_the_reflog_reaches_keep_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/r").await;
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("experiment.txt"), "work").unwrap();
        commit_all(&wt, "detached experiment");
        let lost = run(&wt, &["rev-parse", "HEAD"]);
        run(&wt, &["checkout", "-q", "twr/r"]);
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/r".into()))
                .await
                .unwrap(),
        );
        assert!(reason.contains(&lost[..12]), "{reason}");
        assert!(wt.exists());
        // Once a branch reaches it, removal is safe.
        run(&repo, &["branch", "keep-experiment", &lost]);
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/r".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
    }

    /// Commits `wt` on a detached `HEAD` that stays checked out, with the worktree's `HEAD`
    /// reflog disabled (`core.logAllRefUpdates=false`) and emptied: only the worktree's own
    /// `HEAD` file names the returned commit.
    fn lost_on_a_detached_head_without_reflog(wt: &Path) -> String {
        run(wt, &["config", "core.logAllRefUpdates", "false"]);
        run(wt, &["checkout", "-q", "--detach"]);
        // Content unique to the worktree: the same commit made elsewhere in the same second
        // would share its id.
        std::fs::write(wt.join("experiment.txt"), wt.as_os_str().as_bytes()).unwrap();
        commit_all(wt, "detached experiment");
        let lost = run(wt, &["rev-parse", "HEAD"]);
        let log = wt.join(run(wt, &["rev-parse", "--git-path", "logs/HEAD"]));
        // Emptied (as by an expiry) where it exists; absent is just as empty.
        if log.exists() {
            std::fs::write(&log, "").unwrap();
        }
        assert!(
            run(wt, &["rev-list", "--reflog", "--not", "--branches"]).is_empty(),
            "the reflog must not reach it"
        );
        lost
    }

    /// A stale record whose detached `HEAD` is the only thing reaching a commit
    /// (reflog disabled and empty) is kept by removal and by the resume path (create), and the
    /// commit stays reachable; so is a live worktree with no branch to compare against.
    #[tokio::test]
    async fn a_detached_head_without_a_reflog_keeps_its_record() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = dir.path().join("worktrees/p/t1");
        let wtc = new_wt(&g, &repo, &wt, "twr/t1").await;
        let lost = lost_on_a_detached_head_without_reflog(&wt);
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
        // Resume recreates the worktree: the record still stands, so nothing is created.
        assert!(g
            .worktree_create(spec(&repo, &wt, "twr/t1", None))
            .await
            .is_err());
        assert!(worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
        assert!(run(&repo, &["rev-list", "--all"]).contains(&lost));
        // Once a branch reaches it, the stale record goes and the worktree comes back.
        run(&repo, &["branch", "keep", &lost]);
        g.worktree_create(spec(&repo, &wt, "twr/t1", None))
            .await
            .unwrap();
        assert!(wt.join("README.md").exists());

        // Live, and removed without a branch to compare the checkout with.
        let wt2 = new_wt(&g, &repo, &dir.path().join("worktrees/p/t2"), "twr/t2").await;
        let lost2 = lost_on_a_detached_head_without_reflog(&wt2);
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt2.clone(), None)
                .await
                .unwrap(),
        );
        assert!(reason.contains(&lost2[..12]), "{reason}");
        assert!(wt2.join("experiment.txt").exists());
    }

    #[test]
    fn head_tips_are_read_or_refused() {
        let a = "a".repeat(40);
        assert_eq!(head_tip(format!("{a}\n").as_bytes()).unwrap(), Some(a));
        let b = "b".repeat(64);
        assert_eq!(head_tip(b.as_bytes()).unwrap(), Some(b));
        assert_eq!(head_tip(b"ref: refs/heads/twr/x\n").unwrap(), None);
        for bad in [
            &b"ref: refs/heads/.invalid\n"[..],
            b"ref: refs/worktree/x\n",
            b"ref: refs/heads/\n",
            b"",
            b"garbage\n",
            "0".repeat(40).as_bytes(),
            "a".repeat(39).as_bytes(),
        ] {
            assert!(head_tip(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Commits `wt` on a detached `HEAD`, checks `branch` out again, then expires every
    /// `HEAD` reflog entry but the newest (`<lost> <tip>`): only that entry's old id still
    /// names the lost commit, which is returned.
    fn lost_behind_a_partly_expired_reflog(wt: &Path, branch: &str) -> String {
        run(wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("experiment.txt"), "work").unwrap();
        commit_all(wt, "detached experiment");
        let lost = run(wt, &["rev-parse", "HEAD"]);
        run(wt, &["checkout", "-q", branch]);
        let n: usize = run(wt, &["reflog", "show", "--format=%H", "HEAD"])
            .lines()
            .count();
        for k in (1..n).rev() {
            // Oldest first; without --rewrite the newest keeps its old id.
            run(wt, &["reflog", "delete", &format!("HEAD@{{{k}}}")]);
        }
        let file = wt.join(run(wt, &["rev-parse", "--git-path", "logs/HEAD"]));
        let log = std::fs::read_to_string(&file).unwrap();
        assert_eq!(log.lines().count(), 1, "{log}");
        assert!(log.starts_with(&format!("{lost} ")), "{log}");
        // Git still counts the commit as reachable from the reflog.
        assert!(
            run(wt, &["rev-list", "--reflog", "--not", "--branches"]).contains(&lost),
            "{log}"
        );
        lost
    }

    /// After a partial reflog expiry a detached commit is named only as the old
    /// id of the oldest entry left. Removal keeps the worktree, and a stale record of it stays.
    #[tokio::test]
    async fn a_commit_only_an_expired_reflogs_old_id_reaches_keeps_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        let lost = lost_behind_a_partly_expired_reflog(&wt, "twr/x");
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/x".into()))
                .await
                .unwrap(),
        );
        assert!(reason.contains(&lost[..12]), "{reason}");
        assert!(wt.join(".git").exists());

        // The stale-record path: the directory is gone, the record stays.
        let wt2 = dir.path().join("worktrees/p/t2");
        let wt2c = new_wt(&g, &repo, &wt2, "twr/y").await;
        lost_behind_a_partly_expired_reflog(&wt2, "twr/y");
        std::fs::remove_dir_all(&wt2).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt2, "twr/y").await,
            RemoveOutcome::Missing
        );
        assert!(worktrees(&repo).contains(&wt2c), "{:?}", worktrees(&repo));
    }

    #[test]
    fn reflog_ids_reads_both_ids_and_rejects_malformed_entries() {
        let a = "a".repeat(40);
        let b = "b".repeat(40);
        let z = "0".repeat(40);
        // A latin-1 message is fine.
        let mut log =
            format!("{z} {a} T <t@x> 1 +0000\tadd\n{a} {b} T <t@x> 2 +0000\tcaf").into_bytes();
        log.extend_from_slice(b"\xe9\n");
        assert_eq!(reflog_ids(&log).unwrap(), format!("{a}\n{a}\n{b}\n"));
        let mut raw = log.clone();
        raw.extend_from_slice(b"\xff\xfe\n");
        assert!(reflog_ids(&raw).is_err());
        assert!(reflog_ids(format!("{a} xyz T 1\tm\n").as_bytes()).is_err());
        assert!(reflog_ids(format!("{a}\n").as_bytes()).is_err());
        assert_eq!(reflog_ids(b"").unwrap(), "");
        let sha256 = "c".repeat(64);
        assert_eq!(
            reflog_ids(format!("{z}{} {sha256} T 1\tm", &z[..24]).as_bytes()).unwrap(),
            format!("{sha256}\n")
        );
    }

    /// Ignored files outside the regenerable directories (`.env`) keep the worktree.
    #[tokio::test]
    async fn ignored_files_keep_the_worktree_unless_disposable() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(
            repo.join(".gitignore"),
            "target/\nnode_modules\n.env\n*.log\n",
        )
        .unwrap();
        commit_all(&repo, "ignores");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/i").await;
        // Build output and dependencies only: removed.
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/bin"), "x").unwrap();
        std::fs::create_dir_all(wt.join("web/node_modules/dep")).unwrap();
        std::fs::write(wt.join("web/node_modules/dep/i.js"), "x").unwrap();
        std::fs::create_dir_all(wt.join("build")).unwrap();
        std::fs::write(wt.join("build/run.log"), "x").unwrap();
        assert_eq!(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/i".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
        // A secret: kept.
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/i").await;
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join(".env"), "API_KEY=secret").unwrap();
        std::fs::write(wt.join("debug.log"), "x").unwrap();
        let reason = kept_reason(
            g.worktree_remove(repo.clone(), wt.clone(), Some("twr/i".into()))
                .await
                .unwrap(),
        );
        assert!(
            reason.contains(".env") && reason.contains("debug.log"),
            "{reason}"
        );
        assert!(!reason.contains("target"), "{reason}");
        assert!(wt.join(".env").exists());
        // Configured as disposable: removed.
        let g2 = LocalGit::default().with_park_disposable(vec![
            "target/".into(),
            ".env".into(),
            "debug.log".into(),
        ]);
        assert_eq!(
            g2.worktree_remove(repo.clone(), wt.clone(), Some("twr/i".into()))
                .await
                .unwrap(),
            RemoveOutcome::Removed
        );
    }

    #[test]
    fn disposable_paths() {
        let list: Vec<String> = DEFAULT_PARK_DISPOSABLE
            .iter()
            .map(|s| s.to_string())
            .collect();
        for yes in [
            "target/",
            "crates/x/target/",
            "web/node_modules/",
            "build/out.log",
            "a/__pycache__/m.pyc",
            ".DS_Store",
            "sub/.DS_Store",
        ] {
            assert!(disposable(yes, &list), "{yes}");
        }
        for no in [
            ".env",
            "target",
            "a/node_modules\\xff/",
            "src/build.rs",
            "notes/x.txt",
            "config/local.toml",
        ] {
            assert!(!disposable(no, &list), "{no}");
        }
        // Names are compared raw: a byte that is not UTF-8 never matches an entry spelled with
        // U+FFFD, nor an escape a literal backslash; the other components still count.
        let odd = vec!["\u{FFFD}/".to_string(), "x\\xff".to_string()];
        assert!(!disposable("\\xff/", &odd));
        assert!(disposable("\u{FFFD}/", &odd));
        assert!(!disposable("a/x\\xff", &odd));
        assert!(disposable("a/x\\\\xff", &odd));
        assert!(disposable("\\xff/target/", &list));
    }

    /// An idle trim limited to its own list deletes only what one component names in both
    /// lists: an ignored `build/` inside a `node_modules/` only the idle list names stays.
    #[test]
    fn idle_trim_needs_one_component_in_both_lists() {
        let park: Vec<String> = vec!["build/".into(), "target/".into()];
        let idle: Vec<String> = vec!["node_modules/".into(), "target/".into()];
        let path = "node_modules/pkg/build/";
        // Each list alone names a component of it, not the same one.
        assert!(disposable(path, &park) && disposable(path, &idle));
        assert!(!disposable_in_both(path, &park, &idle));
        for yes in ["target/", "a/target/debug/", "node_modules/x/target/"] {
            assert!(disposable_in_both(yes, &park, &idle), "{yes}");
        }
        for no in ["build/", "node_modules/", "src/target"] {
            assert!(!disposable_in_both(no, &park, &idle), "{no}");
        }
    }

    /// Removing or recreating twr's worktree never clears the records of the user's
    /// own worktrees whose directories are missing (a disk that is not mounted).
    #[tokio::test]
    async fn only_twr_stale_records_are_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        // The user's worktree on a "USB disk", unmounted.
        let usb = dir.path().join("usb/feature");
        run(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                usb.to_str().unwrap(),
            ],
        );
        let usb = std::fs::canonicalize(&usb).unwrap();
        std::fs::rename(dir.path().join("usb"), dir.path().join("usb-unmounted")).unwrap();
        let wt = dir.path().join("worktrees/p/t1");
        let wtc = new_wt(&g, &repo, &wt, "twr/t1").await;
        assert!(worktrees(&repo).contains(&usb));
        // Removed, then removed again (missing), then deleted and recreated.
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Removed
        );
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        new_wt(&g, &repo, &wt, "twr/t1").await;
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(!worktrees(&repo).contains(&wtc));
        new_wt(&g, &repo, &wt, "twr/t1").await;
        std::fs::remove_dir_all(&wt).unwrap();
        new_wt(&g, &repo, &wt, "twr/t1").await;
        // The user's record survived all of it; the disk comes back and it works.
        assert!(worktrees(&repo).contains(&usb), "{:?}", worktrees(&repo));
        std::fs::rename(dir.path().join("usb-unmounted"), dir.path().join("usb")).unwrap();
        assert_eq!(run(&usb, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature");

        // A stale twr record whose reflog holds unreachable commits stays too.
        run(&wt, &["checkout", "-q", "--detach"]);
        std::fs::write(wt.join("d.txt"), "d").unwrap();
        commit_all(&wt, "detached");
        run(&wt, &["checkout", "-q", "twr/t1"]);
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::Missing
        );
        assert!(worktrees(&repo).contains(&wtc));
    }

    /// Restores a directory's mode when dropped (a failed assertion included), so the
    /// temporary directory can still be deleted.
    struct Chmod(PathBuf, u32);

    impl Chmod {
        fn set(dir: &Path, mode: u32) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let old = std::fs::metadata(dir).unwrap().permissions().mode();
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
            Chmod(dir.to_path_buf(), old)
        }
    }

    impl Drop for Chmod {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
        }
    }

    /// A worktree whose parent this user cannot search looks absent to every
    /// check that reads errors as absence. Neither park (remove) nor prune may then delete its
    /// administrative directory (its index holds staged-only edits): both keep everything
    /// and report an error.
    #[tokio::test]
    async fn an_unsearchable_worktree_is_never_treated_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default().with_managed_root(dir.path().join("worktrees"));
        let repo = init_repo(&dir.path().join("repo"));
        let parent = dir.path().join("worktrees/p");
        let wt = parent.join("t1");
        let wtc = new_wt(&g, &repo, &wt, "twr/t1").await;
        std::fs::write(wt.join("staged.txt"), "only in the index").unwrap();
        run(&wt, &["add", "staged.txt"]);
        let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
        assert!(admin.join("index").is_file());

        {
            let _locked = Chmod::set(&parent, 0o000);
            if std::fs::symlink_metadata(&wt).is_ok() {
                eprintln!("skipping: permissions are not enforced for this user");
                return;
            }
            // Park: the initial presence check already refuses (it does not report the
            // worktree missing).
            let e = g
                .worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
                .await
                .unwrap_err();
            assert!(
                format!("{e:#}").contains("cannot tell whether the worktree"),
                "{e:#}"
            );
            assert!(admin.join("index").is_file(), "park deleted the index");
            // Prune (and create) go straight to the stale-record check.
            let e = g.worktree_prune(repo.clone()).await.unwrap_err();
            assert!(format!("{e:#}").contains("cannot tell"), "{e:#}");
            assert!(admin.join("index").is_file(), "prune deleted the index");
        }
        assert!(worktrees(&repo).contains(&wtc), "{:?}", worktrees(&repo));
        assert_eq!(
            run(&wt, &["diff", "--cached", "--name-only"]).trim(),
            "staged.txt"
        );
        // Accessible again: kept as dirty, as usual.
        assert!(matches!(
            remove_async(&g, &repo, &wt, "twr/t1").await,
            RemoveOutcome::KeptDirty { .. }
        ));
    }

    /// A runtime whose working directory reaches the worktree through a symlink and `..`
    /// (`alias -> <wt>/sub`, cwd `alias/..`, which the kernel resolves to the worktree itself)
    /// keeps the worktree: its reservation names the directory the path resolves to, not the
    /// lexical parent of `alias`.
    #[tokio::test]
    async fn a_runtime_in_the_worktree_through_a_symlink_and_dotdot_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/keep.txt"), "x\n").unwrap();
        commit_all(&repo, "sub");
        let g = LocalGit::default();
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/s").await;
        let links = dir.path().join("links");
        std::fs::create_dir(&links).unwrap();
        std::os::unix::fs::symlink(wt.join("sub"), links.join("alias")).unwrap();
        let cwd = links.join("alias").join("..");
        assert_eq!(std::fs::canonicalize(&cwd).unwrap(), wt);
        let used = g.workdirs().begin_use(&cwd, "t9").unwrap();
        let reason = kept_reason(remove_async(&g, &repo, &wt, "twr/s").await);
        assert!(reason.contains("still runs"), "{reason}");
        assert!(wt.join("sub/keep.txt").exists());
        drop(used);
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/s").await,
            RemoveOutcome::Removed
        );
    }

    async fn remove_async(g: &LocalGit, repo: &Path, wt: &Path, branch: &str) -> RemoveOutcome {
        g.worktree_remove(repo.into(), wt.into(), Some(branch.into()))
            .await
            .unwrap()
    }

    /// Config an agent can write into the repository (fsmonitor, hooks) never runs
    /// code through twr's own git commands.
    #[tokio::test]
    async fn repository_config_cannot_run_code_through_twr() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        let marker = dir.path().join("pwned");
        let script = dir.path().join("evil.sh");
        let evil = format!(
            "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
            marker.display()
        );
        install(&script, &evil);
        let hooks = dir.path().join("evil-hooks");
        std::fs::create_dir(&hooks).unwrap();
        for h in [
            "post-checkout",
            "reference-transaction",
            "post-index-change",
        ] {
            install(&hooks.join(h), &evil);
        }
        run(
            &repo,
            &[
                "config",
                "--local",
                "core.fsmonitor",
                script.to_str().unwrap(),
            ],
        );
        run(
            &repo,
            &[
                "config",
                "--local",
                "core.hooksPath",
                hooks.to_str().unwrap(),
            ],
        );
        // The planted config does run code for a plain git (the test means something).
        run(&repo, &["status", "--porcelain"]);
        assert!(
            marker.exists(),
            "fsmonitor did not run for a plain git status"
        );
        std::fs::remove_file(&marker).unwrap();

        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/e").await;
        g.snapshot(wt.clone(), None).await.unwrap();
        g.repo_root(wt.clone()).await.unwrap();
        assert_eq!(
            remove_async(&g, &repo, &wt, "twr/e").await,
            RemoveOutcome::Removed
        );
        assert!(
            !marker.exists(),
            "twr's git ran planted code: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
    }

    /// No git twr runs reaches outside the machine, whatever the repository asks for: in a
    /// partial clone whose promisor remote is an `ext::` command, which its own
    /// `protocol.ext.allow` allows, missing a blob of `HEAD`'s tree and a commit a branch
    /// names, the snapshot, a worktree made from it, a `reset --hard` in it and the
    /// inventory's reads (`rev-list`, `cat-file`) run no transport and fetch nothing.
    #[tokio::test]
    async fn transports_and_lazy_fetches_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let canary = dir.path().join("fetched");
        // `ext::` takes a space as `% ` and `%` as `%%`: none in the path.
        assert!(!canary.to_str().unwrap().contains([' ', '%']));
        // Plain git, with nothing in the environment turning transports or lazy fetches off.
        let plain = |at: &Path, args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .env_remove("GIT_ALLOW_PROTOCOL")
                .env_remove("GIT_NO_LAZY_FETCH")
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap()
        };
        let src = init_repo(&dir.path().join("src"));
        std::fs::write(src.join("a.txt"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        commit_all(&src, "a");
        run(&src, &["config", "uploadpack.allowFilter", "true"]);
        let url = format!("file://{}", src.display());
        let cloned = plain(
            dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "clone",
                "-q",
                "--no-checkout",
                "--filter=blob:none",
                &url,
                "pc",
            ],
        );
        assert!(cloned.status.success(), "{cloned:?}");
        let pc = std::fs::canonicalize(dir.path().join("pc")).unwrap();
        let blob = run(&pc, &["rev-parse", "HEAD:a.txt"]);
        // A commit the clone does not have.
        std::fs::write(src.join("b.txt"), "b\n").unwrap();
        commit_all(&src, "b");
        let ghost = run(&src, &["rev-parse", "HEAD"]);
        // Its promisor remote runs a command, which the repository allows.
        let ext = format!("ext::sh -c touch% {}", canary.display());
        run(&pc, &["config", "remote.origin.url", &ext]);
        run(&pc, &["config", "protocol.ext.allow", "always"]);
        // `HEAD`'s files in the index, with a rename staged: `git status` reads the missing
        // blob to tell it. Written without reading any blob.
        run(&pc, &["read-tree", "HEAD"]);
        let similar = pc.join("similar");
        std::fs::write(&similar, "one\ntwo\nthree\nfour\nsix\n").unwrap();
        let new = run(&pc, &["hash-object", "-w", similar.to_str().unwrap()]);
        run(&pc, &["update-index", "--force-remove", "a.txt"]);
        let info = format!("100644,{new},a2.txt");
        run(&pc, &["update-index", "--add", "--cacheinfo", &info]);
        assert!(!canary.exists(), "the setup fetched");
        // Plain git runs the remote's command for the missing blob (the test means something).
        plain(&pc, &["cat-file", "-t", &blob]);
        assert!(canary.exists(), "plain git did not fetch the missing blob");
        std::fs::remove_file(&canary).unwrap();
        plain(&pc, &["status", "--porcelain=v2"]);
        assert!(
            canary.exists(),
            "plain git status did not fetch the missing blob"
        );
        std::fs::remove_file(&canary).unwrap();
        // Whether git ran the remote's command since last asked.
        let fetched = || {
            let ran = canary.exists();
            let _ = std::fs::remove_file(&canary);
            ran
        };

        // The snapshot: its status needs the blob, which it can't have.
        let snap = g.snapshot(pc.clone(), None).await;
        assert!(!fetched(), "the snapshot fetched");
        assert!(snap.is_err(), "{snap:?}");
        // A worktree: its checkout (`read-tree -u`) needs the blob too.
        let made = g
            .worktree_create(spec(&pc, &dir.path().join("wt"), "twr/x", None))
            .await;
        assert!(!fetched(), "the worktree's checkout fetched");
        assert!(made.is_err(), "{made:?}");
        // `reset --hard` (a restore from the trash whose branch moved).
        let args: Vec<OsString> = ["reset", "--hard", "-q"].map(OsString::from).to_vec();
        let reset = g
            .run(&pc, args, Mode::Change(g.timeouts.worktree), None)
            .await;
        assert!(!fetched(), "reset --hard fetched");
        assert!(!reset.is_ok_and(|o| o.ok));
        // The inventory, with a branch at the commit the clone does not have.
        std::fs::write(pc.join(".git/refs/heads/ghost"), format!("{ghost}\n")).unwrap();
        let common = pc.join(".git");
        let unpublished = g.unpublished_commits(&common).await;
        assert!(!fetched(), "rev-list --all fetched");
        assert!(unpublished.is_err(), "{unpublished:?}");
        let lost = g.lost_commits(&common, format!("{ghost}\n"), "").await;
        assert!(!fetched(), "rev-list --stdin fetched");
        assert!(lost.is_err(), "{lost:?}");
        let ids = BTreeSet::from([blob.as_str(), ghost.as_str()]);
        let missing = g.missing_objects(&common, &ids).await;
        assert!(!fetched(), "cat-file --batch-check fetched");
        // Git refuses (it may not fetch them), or says they are missing.
        if let Ok(m) = &missing {
            assert_eq!(*m, HashSet::from([blob.clone(), ghost.clone()]));
        }
        let kind = g.git(&pc, ["cat-file", "-t", &blob]).await.unwrap();
        assert!(!fetched(), "cat-file -t fetched");
        assert!(!kind.ok);
        // Plain git fetches for the inventory too.
        plain(&pc, &["rev-list", "--all"]);
        assert!(
            canary.exists(),
            "plain git rev-list did not fetch the missing commit"
        );
    }

    /// The commands overrides run do what they must as git runs them, `<command> "$@"` with
    /// arguments, in every shell this machine has: bash refuses `exit 0` with arguments.
    #[test]
    fn override_commands_take_any_arguments_in_any_shell() {
        let shells: Vec<&str> = ["sh", "dash", "bash", "zsh"]
            .into_iter()
            .filter(|sh| {
                std::process::Command::new(sh)
                    .args(["-c", ":"])
                    .status()
                    .is_ok_and(|s| s.success())
            })
            .collect();
        assert!(shells.contains(&"sh"), "{shells:?}");
        for sh in shells {
            for (command, ok) in [(SUCCEEDS, true), (FAILS, false)] {
                assert!(
                    command.contains(' '),
                    "{command}: git would run it through PATH"
                );
                let status = std::process::Command::new(sh)
                    .args(["-c", &format!("{command} \"$@\""), command])
                    .args([
                        "/tmp/a b",
                        "--",
                        "-",
                        "",
                        "-n",
                        "*",
                        "[",
                        "$(touch /tmp/no)",
                        "100644",
                    ])
                    .status()
                    .unwrap();
                assert_eq!(status.success(), ok, "{sh} -c '{command} \"$@\"'");
            }
        }
    }

    /// A diff, or a commit shown or logged with its patch, runs neither the program
    /// `diff.external` names nor a diff driver's `textconv`, should twr ever run one.
    #[tokio::test]
    async fn diffs_run_no_program_the_repository_names() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt diff=evil\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::write(repo.join("b.md"), "a\n").unwrap();
        commit_all(&repo, "files");
        let marker = dir.path().join("ran");
        let touch = |what: &str| format!("echo {what} >> '{}'; cat", marker.display());
        run(&repo, &["config", "diff.evil.textconv", &touch("textconv")]);
        // (`b.md` has no diff driver of its own: only `diff.external` is run for it.)
        std::fs::write(repo.join("a.txt"), "b\n").unwrap();
        std::fs::write(repo.join("b.md"), "b\n").unwrap();
        let commands: [&[&str]; 3] = [&["show", "HEAD"], &["log", "-p", "-1"], &["diff"]];
        // (Failing: a `textconv` turned off fails.)
        for args in commands {
            g.git(&repo, args).await.unwrap();
        }
        let external = format!("echo external >> '{}'; true", marker.display());
        run(&repo, &["config", "diff.external", &external]);
        let diff = g.git(&repo, ["diff"]).await.unwrap();
        assert!(diff.ok, "{diff:?}");
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        // Plain git runs both (the test means something).
        run(&repo, &["diff"]);
        run(&repo, &["show", "HEAD"]);
        let ran = std::fs::read_to_string(&marker).unwrap();
        assert!(
            ran.contains("external") && ran.contains("textconv"),
            "{ran}"
        );
    }

    /// The drivers a command's config names are listed before it runs, in another git: a
    /// driver something names in between (a sandboxed agent writing its worktree's
    /// `config.worktree`) runs, and twr says so, failing the call. The next command turns it
    /// off as any other. A config changed in any other way meanwhile is no race.
    #[tokio::test]
    async fn a_driver_named_while_git_runs_fails_the_call() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=late\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        run(&repo, &["config", "extensions.worktreeConfig", "true"]);
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/race").await;
        // The same size, a later time: `git status` reads it again, through its clean filter.
        let a = wt.join("a.txt");
        std::fs::write(&a, "b\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let marker = dir.path().join("ran");
        let between = |config: Vec<String>| {
            let at = wt.clone();
            let hook: Between = Box::new(move || {
                let args: Vec<&str> = config.iter().map(String::as_str).collect();
                run(&at, &args);
            });
            BETWEEN_LIST_AND_RUN
                .lock()
                .unwrap()
                .push((wt.clone(), hook));
        };
        let clean = format!("echo clean >> '{}'; cat", marker.display());
        between(
            ["config", "--worktree", "filter.late.clean", &clean]
                .map(String::from)
                .to_vec(),
        );
        let e = g.porcelain(&wt, false, false).await.unwrap_err();
        let e = format!("{e:#}");
        assert!(
            e.contains("filter.late") && e.contains("may have run it"),
            "{e}"
        );
        // It did run: the race is real.
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
        // Named before the listing: turned off.
        let st = g.porcelain(&wt, false, false).await.unwrap();
        assert_eq!(st.snapshot().dirty_files, 1);
        assert!(!marker.exists());
        // Another key of a driver turned off already, or config that names no program: no race.
        for config in [
            ["config", "--worktree", "filter.late.smudge", &clean],
            ["config", "--worktree", "core.abbrev", "12"],
        ] {
            between(config.map(String::from).to_vec());
            let st = g.porcelain(&wt, false, false).await.unwrap();
            assert_eq!(st.snapshot().dirty_files, 1);
        }
        assert!(!marker.exists());
        let left = BETWEEN_LIST_AND_RUN.lock().unwrap();
        assert!(!left.iter().any(|(under, _)| *under == wt));
    }

    /// The merge readiness probe (`git merge-tree`, run from a directory of its own) is checked
    /// again once it ran, as any command: a merge driver named while it ran fails its result,
    /// which is not kept; the next probe turns the driver off.
    #[tokio::test]
    async fn a_driver_named_while_readiness_merges_fails_it() {
        if !twr_core::testkit::git::merge_tree_writes() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt merge=late\n").unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        commit_all(&repo, "files");
        let on = run(&repo, &["symbolic-ref", "HEAD"]);
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/x").await;
        std::fs::write(wt.join("a.txt"), "two\n").unwrap();
        commit_all(&wt, "two");
        std::fs::write(repo.join("a.txt"), "three\n").unwrap();
        commit_all(&repo, "three");
        let marker = dir.path().join("ran");
        // It would merge the file cleanly (keeping this side).
        let driver = format!("echo merge >> '{}'; true", marker.display());
        let at = wt.clone();
        let hook: Between = Box::new(move || {
            run(&at, &["config", "merge.late.driver", &driver]);
        });
        BETWEEN_LIST_AND_RUN
            .lock()
            .unwrap()
            .push((wt.clone(), hook));
        let e = format!("{:#}", g.readiness(&wt, &on).await.err().unwrap());
        assert!(
            e.contains("merge.late") && e.contains("may have run it"),
            "{e}"
        );
        // It did run: the race is real.
        assert!(marker.exists());
        std::fs::remove_file(&marker).unwrap();
        // Not kept: probed again, with the driver turned off, the file conflicts.
        let r = g.readiness(&wt, &on).await.unwrap();
        assert_eq!(r.conflicts, Some(vec!["a.txt".to_string()]));
        assert!(!marker.exists());
    }

    /// A caller that gives up while git runs does not stop the check made after it: a driver
    /// named meanwhile is still told, once git is killed and reaped.
    #[tokio::test]
    async fn a_driver_named_while_git_runs_is_told_when_the_caller_gave_up() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=slow\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/gone").await;
        // The same size, a later time: `git status` reads it again, through its clean filter.
        let a = wt.join("a.txt");
        std::fs::write(&a, "b\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let started = dir.path().join("started");
        let clean = format!("touch '{}'; sleep 30; cat", started.display());
        let at = wt.clone();
        let hook: Between = Box::new(move || {
            run(&at, &["config", "filter.slow.clean", &clean]);
        });
        BETWEEN_LIST_AND_RUN
            .lock()
            .unwrap()
            .push((wt.clone(), hook));
        let status = g.porcelain(&wt, false, false);
        tokio::select! {
            r = status => panic!("git ended while its filter sleeps: {}", r.is_ok()),
            _ = wait_until("the driver to start", || started.exists()) => {}
        }
        // Given up (dropped): git is killed and reaped, then checked.
        let told = || {
            RACES
                .lock()
                .unwrap()
                .iter()
                .any(|(d, drivers)| *d == wt && drivers.contains("filter.slow"))
        };
        wait_until("the race told", told).await;
    }

    /// The config is read again where the repository is, not only where git ran: a command run
    /// in a subdirectory that a driver named meanwhile removes is still checked, and the race
    /// still told.
    #[tokio::test]
    async fn a_driver_that_removes_where_git_ran_is_still_told() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=late\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/keep.md"), "k\n").unwrap();
        commit_all(&repo, "files");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/sub").await;
        let sub = wt.join("sub");
        // The same size, a later time: `git status` reads it again, through its clean filter.
        let a = wt.join("a.txt");
        std::fs::write(&a, "b\n").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let clean = format!("rm -rf '{}'; cat", sub.display());
        let at = wt.clone();
        let hook: Between = Box::new(move || {
            run(&at, &["config", "filter.late.clean", &clean]);
        });
        BETWEEN_LIST_AND_RUN
            .lock()
            .unwrap()
            .push((sub.clone(), hook));
        let st = g.porcelain(&sub, false, false).await;
        // It did run, and is told.
        assert!(!sub.exists());
        assert!(st.is_err());
        let races = RACES.lock().unwrap();
        assert!(
            races
                .iter()
                .any(|(d, drivers)| *d == sub && drivers.contains("filter.late")),
            "{races:?}"
        );
    }

    /// The config is read again where git ran, as git found its repository there: a `.git`
    /// planted in the subdirectory git runs in, between the listing and the command, makes
    /// git use another repository, whose drivers the listing never saw. The call is a race,
    /// whether that repository names a driver (read again there) or not (another repository
    /// is found).
    #[tokio::test]
    async fn a_repository_planted_where_git_runs_is_a_race() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::create_dir(repo.join("sub")).unwrap();
        std::fs::write(repo.join("sub/a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        let marker = dir.path().join("ran");
        // Another repository, with a clean filter for `a.txt`, and one with no driver.
        let evil = init_repo(&dir.path().join("evil"));
        std::fs::write(evil.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(evil.join("a.txt"), "x\n").unwrap();
        commit_all(&evil, "files");
        let clean = format!("echo clean >> '{}'; cat", marker.display());
        run(&evil, &["config", "filter.evil.clean", &clean]);
        let plain = init_repo(&dir.path().join("plain"));
        std::fs::write(plain.join("a.txt"), "x\n").unwrap();
        commit_all(&plain, "files");
        let sub = repo.join("sub");
        for (other, told) in [(&evil, "filter.evil"), (&plain, "another repository")] {
            let gitfile = sub.join(".git");
            let line = format!("gitdir: {}\n", other.join(".git").display());
            let hook: Between = Box::new(move || std::fs::write(&gitfile, line).unwrap());
            BETWEEN_LIST_AND_RUN
                .lock()
                .unwrap()
                .push((sub.clone(), hook));
            let e = format!("{:#}", g.porcelain(&sub, false, false).await.unwrap_err());
            assert!(e.contains("may have run"), "{e}");
            assert!(
                RACES
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(d, what)| *d == sub && what.contains(told)),
                "{told}: {e}"
            );
            std::fs::remove_file(sub.join(".git")).unwrap();
        }
        // The planted repository's filter did run: the race is real.
        assert!(marker.exists());
    }

    /// Git LFS (`git-lfs` on `PATH`), with your global config's filter for it in `global`, for
    /// tests: the `LocalGit` that runs git with that global config, and a plain git with it.
    /// `None` (said) without Git LFS.
    fn with_lfs(dir: &Path) -> Option<(LocalGit, PlainGit)> {
        let found = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|d| d.join("git-lfs"))
                .find(|p| p.is_file())
        });
        let Some(lfs) = found else {
            eprintln!("skipped: no git-lfs on PATH");
            return None;
        };
        let global = dir.join("global-config");
        let l = lfs.display();
        std::fs::write(
            &global,
            format!(
                "[filter \"lfs\"]\n\tclean = '{l}' clean -- %f\n\tsmudge = '{l}' smudge -- %f\n\
                 \tprocess = '{l}' filter-process\n\trequired = true\n"
            ),
        )
        .unwrap();
        let wrapper = dir.join("git-with-global");
        install(
            &wrapper,
            format!(
                "#!/bin/sh\nGIT_CONFIG_GLOBAL='{}' exec git \"$@\"\n",
                global.display()
            ),
        );
        let plain = move |at: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(at)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", &global)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .output()
                .unwrap();
            format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        };
        Some((LocalGit::with_bin(&wrapper), Box::new(plain)))
    }

    /// A plain git run in a directory, printing what it printed ([`with_lfs`]).
    type PlainGit = Box<dyn Fn(&Path, &[&str]) -> String>;

    /// A repository with a file Git LFS keeps (`a.bin`), whose own copy of its content is
    /// gone (a checkout of it fetches it), and a canary script at `canary.sh` that writes
    /// `ran` when run.
    fn lfs_repo(dir: &Path, plain: &PlainGit) -> PathBuf {
        let repo = init_repo(&dir.join("repo"));
        std::fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        std::fs::write(repo.join("a.bin"), "kept by lfs\n").unwrap();
        plain(&repo, &["add", "-A"]);
        plain(&repo, &["commit", "-qm", "lfs"]);
        assert!(
            run(&repo, &["cat-file", "-p", "HEAD:a.bin"]).contains("git-lfs"),
            "not a pointer"
        );
        run(
            &repo,
            &["remote", "add", "origin", "https://example.invalid/x.git"],
        );
        let script = dir.join("canary.sh");
        install(
            &script,
            format!(
                "#!/bin/sh\necho ran >> '{}'\nexit 1\n",
                dir.join("ran").display()
            ),
        );
        repo
    }

    /// Git LFS keeps working for twr's git (your filter, which you trust), but where the
    /// repository's config names a program for it to run (a custom transfer agent here), it is
    /// turned off: twr's checkouts hold the pointer, and nothing runs.
    #[tokio::test]
    async fn lfs_runs_no_program_the_repository_names() {
        let dir = tempfile::tempdir().unwrap();
        let Some((g, plain)) = with_lfs(dir.path()) else {
            return;
        };
        let repo = lfs_repo(dir.path(), &plain);
        let canary = dir.path().join("ran");
        // Ordinary LFS: its filter runs, and checks the file out.
        let wt = new_wt(&g, &repo, &dir.path().join("wt1"), "twr/lfs1").await;
        assert_eq!(
            std::fs::read_to_string(wt.join("a.bin")).unwrap(),
            "kept by lfs\n"
        );
        // The content must be fetched now, through the repository's agent.
        std::fs::remove_dir_all(repo.join(".git/lfs/objects")).unwrap();
        let script = dir.path().join("canary.sh");
        run(&repo, &["config", "lfs.standalonetransferagent", "evil"]);
        run(
            &repo,
            &[
                "config",
                "lfs.customtransfer.evil.path",
                script.to_str().unwrap(),
            ],
        );
        let wt = new_wt(&g, &repo, &dir.path().join("wt2"), "twr/lfs2").await;
        assert!(std::fs::read_to_string(wt.join("a.bin"))
            .unwrap()
            .starts_with("version https://git-lfs"));
        g.snapshot(wt.clone(), None).await.unwrap();
        let args: Vec<OsString> = ["reset", "--hard", "-q"].map(OsString::from).to_vec();
        let reset = g
            .run(&wt, args, Mode::Change(g.timeouts.worktree), None)
            .await
            .unwrap();
        assert!(reset.ok, "{}", reset.stderr);
        assert!(!canary.exists(), "twr's git ran the repository's LFS agent");
        // Plain git runs it (the test means something).
        let wt3 = dir.path().join("wt3");
        plain(&repo, &["worktree", "add", "-q", wt3.to_str().unwrap()]);
        assert!(canary.exists(), "plain git did not run the agent");
    }

    /// A program named for Git LFS while its filter was on (between the listing and the
    /// command) is a race like a driver named then: it ran, and is told.
    #[tokio::test]
    async fn an_lfs_program_named_while_git_runs_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let Some((g, plain)) = with_lfs(dir.path()) else {
            return;
        };
        let repo = lfs_repo(dir.path(), &plain);
        let canary = dir.path().join("ran");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/lfs").await;
        // To be checked out again, its content fetched.
        std::fs::remove_dir_all(repo.join(".git/lfs/objects")).unwrap();
        std::fs::remove_file(wt.join("a.bin")).unwrap();
        let script = dir.path().join("canary.sh");
        let at = repo.clone();
        let hook: Between = Box::new(move || {
            run(&at, &["config", "lfs.standalonetransferagent", "x"]);
            run(
                &at,
                &[
                    "config",
                    "lfs.customtransfer.x.path",
                    script.to_str().unwrap(),
                ],
            );
        });
        BETWEEN_LIST_AND_RUN
            .lock()
            .unwrap()
            .push((wt.clone(), hook));
        let args: Vec<OsString> = ["read-tree", "--reset", "-u", "HEAD"]
            .map(OsString::from)
            .to_vec();
        let _ = g
            .run(&wt, args, Mode::Change(g.timeouts.worktree), None)
            .await;
        assert!(canary.exists(), "the race did not run the agent");
        let races = RACES.lock().unwrap();
        assert!(
            races
                .iter()
                .any(|(d, drivers)| *d == wt && drivers.contains("filter.lfs")),
            "{races:?}"
        );
    }

    /// A change git made stays made when a driver was named while it ran: its own result is
    /// returned (its caller records what it made, and failing it would undo nothing), and the
    /// race is logged.
    #[tokio::test]
    async fn a_change_raced_by_a_driver_keeps_its_result() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&dir.path().join("repo"));
        std::fs::write(repo.join(".gitattributes"), "*.txt filter=late\n").unwrap();
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        commit_all(&repo, "files");
        let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/change").await;
        // Changed: checked out again.
        std::fs::write(wt.join("a.txt"), "b\n").unwrap();
        let marker = dir.path().join("ran");
        let smudge = format!("echo smudge >> '{}'; cat", marker.display());
        let at = wt.clone();
        let hook: Between = Box::new(move || {
            run(&at, &["config", "filter.late.smudge", &smudge]);
        });
        BETWEEN_LIST_AND_RUN
            .lock()
            .unwrap()
            .push((wt.clone(), hook));
        let args: Vec<OsString> = ["read-tree", "--reset", "-u", "HEAD"]
            .map(OsString::from)
            .to_vec();
        let out = g
            .run(&wt, args, Mode::Change(g.timeouts.worktree), None)
            .await
            .unwrap();
        assert!(out.ok, "{}", out.stderr);
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "a\n");
        // The driver ran (the race is real), and it is told.
        assert!(marker.exists());
        let races = RACES.lock().unwrap();
        assert!(
            races
                .iter()
                .any(|(d, drivers)| *d == wt && drivers.contains("filter.late")),
            "{races:?}"
        );
    }

    /// A diff of a submodule's change is the commits' names, never a diff of its files, which
    /// would run in the submodule, by its own config's drivers: not even where the
    /// repository's config asks for it (`diff.submodule=diff`).
    #[tokio::test]
    async fn diffs_never_go_into_submodules() {
        let dir = tempfile::tempdir().unwrap();
        let g = LocalGit::default();
        let sub = init_repo(&dir.path().join("sub"));
        std::fs::write(sub.join(".gitattributes"), "*.txt diff=x\n").unwrap();
        std::fs::write(sub.join("a.txt"), "a\n").unwrap();
        commit_all(&sub, "files");
        let outer = init_repo(&dir.path().join("outer"));
        run(
            &outer,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                sub.to_str().unwrap(),
                "sub",
            ],
        );
        commit_all(&outer, "sub");
        let wsub = outer.join("sub");
        std::fs::write(wsub.join("a.txt"), "b\n").unwrap();
        let commit = [
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qam",
            "b",
        ];
        run(&wsub, &commit);
        let marker = dir.path().join("ran");
        let textconv = format!("echo textconv >> '{}'; cat", marker.display());
        run(&wsub, &["config", "diff.x.textconv", &textconv]);
        run(&outer, &["config", "diff.submodule", "diff"]);
        // The submodule's new commit, in the work tree, then committed.
        g.git(&outer, ["diff"]).await.unwrap();
        commit_all(&outer, "bump");
        g.git(&outer, ["log", "-p", "-1"]).await.unwrap();
        g.git(&outer, ["show"]).await.unwrap();
        assert!(
            !marker.exists(),
            "{}",
            std::fs::read_to_string(&marker).unwrap()
        );
        // Plain git runs it (the test means something).
        run(&outer, &["show"]);
        assert!(marker.exists());
    }

    /// A fake git that runs `body` (a shell snippet), written by [`install`] (ETXTBSY).
    fn fake_git(dir: &Path, body: &str) -> LocalGit {
        let p = dir.join("fake-git");
        // Before every command twr lists the drivers the config defines (`config --list
        // --name-only`), and before some the submodules (`ls-files --stage`): answered at
        // once, with none. Where git finds its repository (`rev-parse --absolute-git-dir`),
        // read before and after each such command too, is git's own answer.
        let lists = "for a in \"$@\"; do case \"$a\" in --name-only|--stage) exit 0;; \
                     --absolute-git-dir) exec git \"$@\";; esac; done";
        install(&p, format!("#!/bin/sh\n{lists}\n{body}\n"));
        LocalGit::with_bin(p).with_timeouts(
            Duration::from_millis(300),
            Duration::from_millis(300),
            Duration::from_millis(300),
        )
    }

    /// Every git twr runs is told to run no transport and no lazy fetch.
    #[tokio::test]
    async fn every_git_is_told_not_to_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let g = fake_git(
            dir.path(),
            "printf '%s|%s' \"${GIT_ALLOW_PROTOCOL-unset}\" \"$GIT_NO_LAZY_FETCH\"",
        );
        for args in [["rev-parse", "HEAD"], ["read-tree", "HEAD"]] {
            let out = g.git(dir.path(), args).await.unwrap();
            assert_eq!(out.text(), "|1", "{args:?}");
        }
    }

    fn pid_alive(pid: u32) -> bool {
        crate::reap::process_alive(pid)
    }

    async fn wait_until(what: &str, cond: impl Fn() -> bool) {
        for _ in 0..500 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// A hung read-only git is killed with everything it started; so is one whose
    /// caller gives up.
    #[tokio::test]
    async fn hung_reads_are_killed_with_their_group() {
        let dir = tempfile::tempdir().unwrap();
        let pids = dir.path().join("pids");
        let g = fake_git(
            dir.path(),
            &format!("sleep 300 & echo $! >> '{}'\nsleep 300", pids.display()),
        );
        let started = std::time::Instant::now();
        let e = g.snapshot(dir.path().into(), None).await.unwrap_err();
        assert!(e.to_string().contains("timed out"), "{e:#}");
        assert!(started.elapsed() < Duration::from_secs(5));
        let first: u32 = std::fs::read_to_string(&pids)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_until("the background child killed", || !pid_alive(first)).await;

        // The caller gives up (the future is dropped).
        std::fs::remove_file(&pids).unwrap();
        let g = g.with_timeouts(
            Duration::from_secs(300),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let _ =
            tokio::time::timeout(Duration::from_millis(300), g.repo_root(dir.path().into())).await;
        wait_until("the pid file", || pids.exists()).await;
        let second: u32 = std::fs::read_to_string(&pids)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_until("the abandoned child killed", || !pid_alive(second)).await;
    }

    /// The pids a fake git wrote to `file`, one per line.
    fn pids_in(file: &Path) -> Vec<u32> {
        std::fs::read_to_string(file)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// A shell snippet for a fake git: notes its pid and a child's in `<dir>/pids`, touches
    /// `<dir>/started`, then (both) sleep `secs` and touch `<dir>/marker` (what a git that
    /// kept running past its timeout would still have changed).
    fn slow_change(dir: &Path, secs: u32) -> String {
        let d = dir.display();
        format!(
            "echo $$ >> '{d}/pids'\n(sleep {secs}; touch '{d}/marker') &\necho $! >> \
             '{d}/pids'\ntouch '{d}/started'\nsleep {secs}\ntouch '{d}/marker'\nexit 0"
        )
    }

    /// Watches the reservations of `g` for `path` once `<dir>/started` exists: returns whether
    /// any pid in `<dir>/pids` was still alive at the moment a runtime could first start in
    /// `path` again (the reservation of the change gone).
    fn reservation_probe(
        g: &LocalGit,
        path: &Path,
        dir: &Path,
    ) -> tokio::task::JoinHandle<Vec<u32>> {
        let (workdirs, path, dir) = (
            Arc::clone(g.workdirs()),
            path.to_path_buf(),
            dir.to_path_buf(),
        );
        tokio::spawn(async move {
            loop {
                if dir.join("started").exists() {
                    if let Ok(u) = workdirs.begin_use(&path, "probe") {
                        let alive = pids_in(&dir.join("pids"))
                            .into_iter()
                            .filter(|p| pid_alive(*p))
                            .collect();
                        drop(u);
                        return alive;
                    }
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
    }

    /// A change that does not finish in time is killed with everything it started,
    /// and the call returns only once all of it is gone: nothing it started runs on, the
    /// marker it would have written never appears, and the reservation it holds goes only
    /// then. The same when the caller gives up.
    #[tokio::test]
    async fn timed_out_changes_are_killed_and_reaped() {
        let dir = tempfile::tempdir().unwrap();
        let g = fake_git(dir.path(), &slow_change(dir.path(), 2));
        let wt = dir.path().join("wt");
        let probe = reservation_probe(&g, &wt, dir.path());
        let hold = g
            .workdirs()
            .begin_change(&wt, ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap();
        let started = std::time::Instant::now();
        let e = g
            .run_holding(
                dir.path(),
                vec!["worktree".into(), "remove".into()],
                Mode::Change(Duration::from_millis(300)),
                None,
                Some(hold),
            )
            .await
            .unwrap_err();
        assert!(was_killed(&e), "{e:#}");
        assert!(format!("{e:#}").contains("unknown state"), "{e:#}");
        assert!(started.elapsed() < Duration::from_secs(2));
        let pids = pids_in(&dir.path().join("pids"));
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(pids.iter().all(|p| !pid_alive(*p)), "{pids:?}");
        assert!(g.workdirs().begin_use(&wt, "t").is_ok());
        assert_eq!(probe.await.unwrap(), Vec::<u32>::new());
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!dir.path().join("marker").exists());

        // The caller gives up: killed at once, the reservation kept until all of it is gone.
        for f in ["pids", "started"] {
            std::fs::remove_file(dir.path().join(f)).unwrap();
        }
        let g = g.with_timeouts(
            Duration::from_secs(300),
            Duration::from_secs(300),
            Duration::from_secs(300),
        );
        let probe = reservation_probe(&g, &wt, dir.path());
        let hold = g
            .workdirs()
            .begin_change(&wt, ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap();
        let gave_up = tokio::time::timeout(
            Duration::from_millis(300),
            g.run_holding(
                dir.path(),
                vec!["worktree".into(), "remove".into()],
                Mode::Change(Duration::from_secs(300)),
                None,
                Some(hold),
            ),
        )
        .await;
        assert!(gave_up.is_err());
        assert_eq!(probe.await.unwrap(), Vec::<u32>::new());
        let pids = pids_in(&dir.path().join("pids"));
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(pids.iter().all(|p| !pid_alive(*p)), "{pids:?}");
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!dir.path().join("marker").exists());
    }

    /// A `git` that runs the real one, except for a command whose arguments contain `words`
    /// (space-separated, in order): that one runs `body` instead.
    fn git_except(dir: &Path, words: &str, body: &str) -> LocalGit {
        fake_git(
            dir,
            &format!("case \" $* \" in\n*\" {words} \"*)\n{body}\n;;\nesac\nexec git \"$@\""),
        )
        .with_timeouts(STATUS_TIMEOUT, READ_TIMEOUT, Duration::from_secs(2))
    }

    /// Output is capped.
    #[tokio::test]
    async fn output_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let g = fake_git(
            dir.path(),
            &format!("head -c {} /dev/zero", MAX_STDOUT + 10),
        )
        .with_timeouts(
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(30),
        );
        let e = g.snapshot(dir.path().into(), None).await.unwrap_err();
        assert!(e.to_string().contains("more than"), "{e:#}");
        // Under the cap is fine, stderr beyond its cap is cut.
        let g = fake_git(
            dir.path(),
            &format!("head -c {} /dev/zero | tr '\\0' x >&2; echo ok", 1 << 20),
        );
        let out = g.git(dir.path(), ["x"]).await.unwrap();
        assert_eq!(out.text().trim(), "ok");
        assert_eq!(out.stderr.len(), MAX_STDERR);
    }

    /// Runs `git <args>` in `dir` with `input` on stdin; panics when it fails.
    fn git_stdin(dir: &Path, args: &[&str], input: &str) -> String {
        use std::io::Write as _;
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// The manifest of `g`'s trash item `item`.
    async fn manifest(g: &LocalGit, item: &str) -> twr_core::git::TrashManifest {
        list(g)
            .await
            .into_iter()
            .find(|i| i.manifest.item == item)
            .unwrap_or_else(|| panic!("no item {item}"))
            .manifest
    }

    /// What the worktree's index staged (a new file, a change, the three stages of
    /// a conflict) is in the item's pack, so the hardest `git gc` while the record is out of
    /// the repository loses none of it. A restore does not bring the index back:
    /// the staged changes come back as unstaged ones (the worktree's files as they were), the
    /// result says so, and the staged versions stay in the item's pack.
    #[tokio::test]
    async fn staged_content_survives_gc_in_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("t"), "twr/t").await;
        std::fs::write(wt.join("staged.txt"), "only in the index\n").unwrap();
        std::fs::write(wt.join("README.md"), "changed and staged\n").unwrap();
        run(&wt, &["add", "staged.txt", "README.md"]);
        std::fs::write(wt.join("README.md"), "changed again, not staged\n").unwrap();
        let blob = |text: &str| git_stdin(&wt, &["hash-object", "-w", "--stdin"], text);
        let stages = [blob("base\n"), blob("ours\n"), blob("theirs\n")];
        git_stdin(
            &wt,
            &["update-index", "--index-info"],
            &stages
                .iter()
                .enumerate()
                .map(|(n, b)| format!("100644 {b} {}\tconflict.txt\n", n + 1))
                .collect::<String>(),
        );
        let staged = run(&wt, &["rev-parse", ":staged.txt"]);
        let readme = run(&wt, &["rev-parse", ":README.md"]);
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        let item_dir = out.dir.unwrap();
        let m = manifest(&g, &item).await;
        let pack = m
            .index_pack
            .clone()
            .expect("the index's objects are packed");
        let idx = item_dir
            .join("index-objects")
            .join(format!("pack-{pack}.idx"));
        assert!(idx.is_file());
        gc_now(&repo);
        // The repository itself dropped them: only the item has them.
        for b in stages.iter().chain([&staged, &readme]) {
            assert!(!has_object(&repo, b), "{b}");
        }
        let back = g
            .trash_restore(RestoreRequest {
                item: item.clone(),
                branch: "twr/t".into(),
                paths: vec![wt.clone()],
                other_branches: Some(Vec::new()),
            })
            .await
            .unwrap();
        assert!(
            back.notes.iter().any(|n| n.contains("staged")),
            "{:?}",
            back.notes
        );
        assert_eq!(run(&wt, &["diff", "--cached", "--name-only"]), "");
        assert_eq!(
            std::fs::read_to_string(wt.join("README.md")).unwrap(),
            "changed again, not staged\n"
        );
        let status = run(&wt, &["status", "--porcelain"]);
        assert!(
            status.contains("M README.md") && status.contains("?? staged.txt"),
            "{status}"
        );
        let packed = run(&repo, &["verify-pack", "-v", idx.to_str().unwrap()]);
        for b in stages.iter().chain([&staged, &readme]) {
            assert!(packed.contains(b.as_str()), "{b}: {packed}");
        }
    }

    /// A commit only the branch's reflog had, whose reflog entries `git gc`
    /// expired while it was in the trash (the item's `reflog/<id>` ref kept it): the restore
    /// writes the new branch's reflog from the item's verbatim copy, so once the item is
    /// purged the restored reflog keeps it (an ordinary `git gc` keeps it too). A
    /// branch name whose reflog is there without the branch is refused (removing what a
    /// restore made would delete that reflog with the branch), and the trash's refs keep the
    /// commit until the purge.
    #[tokio::test]
    async fn a_restore_writes_the_branch_reflog_from_the_items_copy() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("t"), "twr/t").await;
        std::fs::write(wt.join("f.txt"), "f").unwrap();
        commit_all(&wt, "work");
        let tip = run(&wt, &["rev-parse", "HEAD"]);
        // The branch moved to a commit and back from the main checkout: only the branch's
        // reflog names it (not the worktree's HEAD reflog).
        let tree = run(&repo, &["rev-parse", &format!("{tip}^{{tree}}")]);
        let side = run(&repo, &["commit-tree", &tree, "-p", &tip, "-m", "side"]);
        run(
            &repo,
            &["update-ref", "-m", "side", "refs/heads/twr/t", &side],
        );
        run(
            &repo,
            &["update-ref", "-m", "back", "refs/heads/twr/t", &tip],
        );
        assert!(!run(&wt, &["reflog", "--format=%H", "HEAD"]).contains(&side));
        let out = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        let item = out.item.unwrap();
        assert!(manifest(&g, &item).await.branch_log_saved);
        run(
            &repo,
            &[
                "reflog",
                "expire",
                "--expire=now",
                "--expire-unreachable=now",
                "--all",
            ],
        );
        gc_now(&repo);
        assert!(has_object(&repo, &side));
        assert!(
            run(
                &repo,
                &[
                    "reflog",
                    "--format=%H",
                    &format!("refs/twr-trash/{item}/branch")
                ]
            )
            .is_empty(),
            "the trash ref's reflog expired"
        );
        // A name whose reflog is there without its branch: refused, the reflog as it was.
        let log = repo.join(".git/logs/refs/heads/twr/other");
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let mine = format!(
            "{} {tip} me <me@example.invalid> 1 +0000\tmine\n",
            "0".repeat(40)
        );
        std::fs::write(&log, &mine).unwrap();
        let e = restore_at(&g, &item, "twr/other", &wt).await.unwrap_err();
        assert!(format!("{e:#}").contains("without the branch"), "{e:#}");
        assert_eq!(std::fs::read_to_string(&log).unwrap(), mine);
        assert!(!wt.exists() && !branch_exists(&repo, "twr/other"));
        assert!(kept_by_trash(&repo, &item, &side));
        // Under its own name: the item's copy.
        restore_at(&g, &item, "twr/t", &wt).await.unwrap();
        let restored = run(&repo, &["reflog", "--format=%H %gs", "twr/t"]);
        assert!(restored.contains(&side), "{restored}");
        g.trash_purge(TrashPurge {
            items: vec![item.clone()],
            ..Default::default()
        })
        .await
        .unwrap();
        assert!(trash_refs(&repo).is_empty());
        run(&repo, &["gc", "-q", "--prune=now"]);
        assert!(has_object(&repo, &side));
    }

    /// Makes `path` unreadable until dropped; `None` when this user reads it all the same.
    struct Unreadable(PathBuf);

    impl Unreadable {
        fn new(path: &Path) -> Option<Unreadable> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
            let u = Unreadable(path.to_path_buf());
            if std::fs::read(path).is_ok() {
                eprintln!(
                    "skipped: {} is readable all the same (root?)",
                    path.display()
                );
                return None;
            }
            Some(u)
        }
    }

    impl Drop for Unreadable {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o644));
        }
    }

    fn mkfifo(path: &Path) {
        assert!(std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success());
    }

    /// Every file of `dir` (relative path, contents), symlinks as their targets.
    fn files_of(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut out = BTreeMap::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let p = e.path();
                let t = e.file_type().unwrap();
                let rel = p.strip_prefix(dir).unwrap().to_path_buf();
                if t.is_dir() {
                    todo.push(p);
                } else if t.is_symlink() {
                    out.insert(
                        rel,
                        std::fs::read_link(&p).unwrap().into_os_string().into_vec(),
                    );
                } else if t.is_file() {
                    out.insert(rel, std::fs::read(&p).unwrap());
                }
            }
        }
        out
    }

    /// A move across file systems copies everything first and removes nothing at
    /// the source until the whole copy is there: a file that cannot be read or a failure
    /// noting the copy leaves the source whole and no partial copy; a special file stays at
    /// the source, the rest moving.
    #[test]
    fn a_copy_removes_nothing_until_all_of_it_arrived() {
        let dir = tempfile::tempdir().unwrap();
        let make = |name: &str| {
            let src = dir.path().join(name);
            std::fs::create_dir_all(src.join("sub").join("deeper")).unwrap();
            std::fs::write(src.join("a.txt"), "a").unwrap();
            std::fs::write(src.join("sub").join("b.bin"), vec![9u8; 100_000]).unwrap();
            std::fs::write(src.join("sub").join("deeper").join("c.txt"), "c").unwrap();
            std::os::unix::fs::symlink("a.txt", src.join("link")).unwrap();
            src
        };
        let noop = &mut || Ok(());
        // A FIFO stays; the rest moves.
        let src = make("fifo");
        mkfifo(&src.join("sub").join("pipe"));
        let before = files_of(&src);
        let dst = dir.path().join("fifo-dst");
        let moved = trash::move_tree(&src, &dst, true, noop).unwrap();
        assert_eq!(moved, trash::Moved::Copied { left: true });
        assert_eq!(files_of(&dst), before);
        assert!(src.join("sub").join("pipe").exists());
        assert!(files_of(&src).is_empty());
        // A file that cannot be read, part way: nothing removed, no copy left.
        let src = make("unreadable");
        let before = files_of(&src);
        let dst = dir.path().join("unreadable-dst");
        if let Some(u) = Unreadable::new(&src.join("sub").join("deeper").join("c.txt")) {
            assert!(trash::move_tree(&src, &dst, true, noop).is_err());
            drop(u);
            assert_eq!(files_of(&src), before);
            assert!(!dst.exists());
        }
        // Noting the copy fails: nothing removed, no copy left.
        let e = trash::move_tree(&src, &dst, true, &mut || {
            Err(std::io::Error::other("cannot note it"))
        })
        .unwrap_err();
        assert!(e.to_string().contains("cannot note it"));
        assert_eq!(files_of(&src), before);
        assert!(!dst.exists());
        // A whole copy: everything arrives, nothing stays.
        let moved = trash::move_tree(&src, &dst, true, noop).unwrap();
        assert_eq!(moved, trash::Moved::Copied { left: false });
        assert_eq!(files_of(&dst), before);
        assert!(!src.exists());
    }

    /// Through the trash: moving in across file systems (copies) takes everything
    /// but a FIFO, which stays at the old path; the item is complete.
    #[tokio::test]
    async fn moving_across_file_systems_leaves_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let copying = g.clone().with_copy_moves();
        let repo = init_repo(&dir.path().join("repo"));
        let wt = new_wt(&g, &repo, &dir.path().join("worktrees").join("a"), "twr/a").await;
        std::fs::create_dir_all(wt.join("deep")).unwrap();
        std::fs::write(wt.join("deep").join("f.txt"), "f").unwrap();
        mkfifo(&wt.join("pipe"));
        let out = to_trash(&copying, &repo, &wt, Some("twr/a")).await.unwrap();
        assert!(out.worktree_moved && out.worktree_left, "{out:?}");
        let item_dir = out.dir.unwrap();
        assert!(item_dir.join("worktree/deep/f.txt").is_file());
        assert!(item_dir.join("record/HEAD").is_file());
        assert!(wt.join("pipe").exists() && !wt.join("deep").exists());
        assert!(manifest(&g, out.item.as_ref().unwrap()).await.complete);
    }

    /// Sets the time `item`'s manifest last changed `ago` back.
    fn age_manifest(g: &LocalGit, item: &str, ago: Duration) {
        let file = g.trash_root().unwrap().join(item).join("manifest.json");
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - ago)
            .unwrap();
    }

    /// A purge never takes an item another operation holds (its lock), or an
    /// incomplete one that holds what its deletion moved (the worktree's files, here) however
    /// old; an incomplete one that holds nothing goes once abandoned (untouched for an hour),
    /// not before. A restored item is purged like any other.
    #[tokio::test]
    async fn a_purge_never_takes_what_an_item_still_holds() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let root = dir.path().join("worktrees").join("proj");
        let purge_all = || {
            g.trash_purge(TrashPurge {
                before_ms: Some(u64::MAX),
                ..Default::default()
            })
        };
        // Held by another operation.
        let wt = new_wt(&g, &repo, &root.join("held"), "twr/held").await;
        let out = to_trash(&g, &repo, &wt, Some("twr/held")).await.unwrap();
        let held_item = out.item.unwrap();
        let lock = std::fs::File::options()
            .write(true)
            .open(out.dir.as_ref().unwrap().join("lock"))
            .unwrap();
        // Blocking: a process started meanwhile may hold a copy of the item's lock a moment.
        lock.lock().unwrap();
        let p = purge_all().await.unwrap();
        assert!(p.purged.is_empty(), "{p:?}");
        assert!(p.failed[0].error.contains("busy"), "{p:?}");
        drop(lock);
        // Free again (and restored meanwhile): purged, the restored worktree left.
        restore_at(&g, &held_item, "twr/held", &wt).await.unwrap();
        let p = purge_all().await.unwrap();
        assert_eq!(p.purged, std::slice::from_ref(&held_item), "{p:?}");
        assert!(wt.exists() && branch_exists(&repo, "twr/held"));

        // Incomplete, holding the worktree's files (its record could not move: the records'
        // directory may not be written).
        let wt = new_wt(&g, &repo, &root.join("half"), "twr/half").await;
        std::fs::write(wt.join("work.txt"), "unique").unwrap();
        let records = repo.join(".git").join("worktrees");
        let ro = ReadOnly::new(&records);
        if ro.is_some() {
            let e = to_trash(&g, &repo, &wt, Some("twr/half"))
                .await
                .unwrap_err();
            assert!(format!("{e:#}").contains("record"), "{e:#}");
            drop(ro);
            let _ = std::fs::remove_file(records.join("f"));
            let half = list(&g).await.pop().unwrap().manifest;
            assert!(!half.complete && half.worktree_moved && !half.record_moved);
            age_manifest(&g, &half.item, Duration::from_secs(7200));
            let p = purge_all().await.unwrap();
            assert!(p.purged.is_empty(), "{p:?}");
            assert!(p.failed[0].error.contains("the worktree's files"), "{p:?}");
            let kept = g.trash_root().unwrap().join(&half.item);
            assert_eq!(
                std::fs::read_to_string(kept.join("worktree/work.txt")).unwrap(),
                "unique"
            );
            remove_item_by_hand(&g, &repo, &half.item);
        } else {
            drop(ro);
        }

        // Incomplete, holding nothing (the worktree could not move): recent, then abandoned.
        let parent = root.join("stuck");
        let wt = new_wt(&g, &repo, &parent.join("t"), "twr/stuck").await;
        let Some(ro) = ReadOnly::new(&parent) else {
            return;
        };
        to_trash(&g, &repo, &wt, Some("twr/stuck"))
            .await
            .unwrap_err();
        drop(ro);
        let stuck = list(&g).await.pop().unwrap().manifest;
        assert!(!stuck.complete && !stuck.worktree_moved);
        let p = purge_all().await.unwrap();
        assert!(p.failed[0].error.contains("recent"), "{p:?}");
        age_manifest(&g, &stuck.item, Duration::from_secs(7200));
        let p = purge_all().await.unwrap();
        assert_eq!(p.purged, std::slice::from_ref(&stuck.item), "{p:?}");
        assert!(trash_refs(&repo).is_empty());
        assert!(wt.exists() && branch_exists(&repo, "twr/stuck"));
    }

    /// An incomplete item whose branch has a namesake made again since (from
    /// `HEAD`, without the item's commits) still holds the only copy of those commits: a
    /// purge keeps it however old (the list says why), and they survive the hardest gc. Once
    /// something else keeps them (the branch moved to the item's tip), it goes.
    #[tokio::test]
    async fn an_incomplete_item_keeps_the_commits_only_it_has() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let tree = run(&repo, &["rev-parse", "HEAD^{tree}"]);
        let unique = run(&repo, &["commit-tree", &tree, "-p", "HEAD", "-m", "unique"]);
        run(&repo, &["branch", "twr/t", &unique]);
        // A thread whose worktree is gone: the item holds its branch alone.
        let gone = dir.path().join("worktrees").join("proj").join("t");
        let out = to_trash(&g, &repo, &gone, Some("twr/t")).await.unwrap();
        assert!(out.branch_deleted && !out.worktree_moved, "{out:?}");
        let item = out.item.unwrap();
        // Its deletion failed part way after the branch went (a crash before the last write).
        let file = g.trash_root().unwrap().join(&item).join("manifest.json");
        let mut m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        m["complete"] = false.into();
        std::fs::write(&file, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
        // The thread resumed from `HEAD` meanwhile: a branch of the same name, without `unique`.
        run(&repo, &["branch", "twr/t"]);
        age_manifest(&g, &item, Duration::from_secs(7200));
        let purge_all = || {
            g.trash_purge(TrashPurge {
                before_ms: Some(u64::MAX),
                ..Default::default()
            })
        };
        let p = purge_all().await.unwrap();
        assert!(p.purged.is_empty(), "{p:?}");
        assert!(
            p.failed[0].error.contains("the only copy of 1 commit"),
            "{p:?}"
        );
        let listed = list(&g).await;
        let i = listed.iter().find(|i| i.manifest.item == item).unwrap();
        assert_eq!(i.kept.as_deref(), Some("holds the only copy of 1 commit"));
        gc_now(&repo);
        assert!(has_object(&repo, &unique));
        assert!(kept_by_trash(&repo, &item, &unique));
        // Kept elsewhere now: the item goes, the commit stays.
        run(&repo, &["branch", "-f", "twr/t", &unique]);
        age_manifest(&g, &item, Duration::from_secs(7200));
        assert_eq!(list(&g).await[0].kept, None);
        let p = purge_all().await.unwrap();
        assert_eq!(p.purged, std::slice::from_ref(&item), "{p:?}");
        assert!(trash_refs(&repo).is_empty());
        gc_now(&repo);
        assert!(has_object(&repo, &unique));
    }

    /// Makes `tip` look like a child of `parent` in `repo` through a graft: a replacement
    /// (`git replace --graft`), or a line of the legacy graft file (`info/grafts`) when
    /// `legacy`. Returns what [`ungraft`] takes to remove it.
    fn graft(repo: &Path, tip: &str, parent: &str, legacy: bool) -> PathBuf {
        let file = repo.join(run(repo, &["rev-parse", "--git-path", "info/grafts"]));
        if legacy {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, format!("{tip} {parent}\n")).unwrap();
        } else {
            run(repo, &["replace", "--graft", tip, parent]);
        }
        assert!(
            run(
                repo,
                &["-c", "advice.graftFileDeprecated=false", "rev-list", tip]
            )
            .contains(parent),
            "the graft must make {parent} look reachable from {tip}"
        );
        file
    }

    /// Removes the graft [`graft`] made of `tip`.
    fn ungraft(repo: &Path, tip: &str, file: &Path, legacy: bool) {
        if legacy {
            std::fs::remove_file(file).unwrap();
        } else {
            run(repo, &["replace", "-d", tip]);
        }
    }

    /// A commit only the branch's reflog names, which a graft (a replacement, or
    /// a legacy `info/grafts` line) makes look like an ancestor of the branch's tip, still
    /// gets a ref of its own in the trash: git as twr runs it sees the literal object graph.
    /// Once the graft is gone it survives the hardest gc.
    #[tokio::test]
    async fn a_graft_never_stands_in_for_a_kept_commit() {
        for legacy in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let g = with_trash(&LocalGit::default(), dir.path());
            let repo = init_repo(&dir.path().join("repo"));
            let tree = run(&repo, &["rev-parse", "HEAD^{tree}"]);
            let lost = run(
                &repo,
                &["commit-tree", &tree, "-p", "HEAD", "-m", "only reflog"],
            );
            run(&repo, &["branch", "twr/t", &lost]);
            let tip = run(&repo, &["commit-tree", &tree, "-p", "HEAD", "-m", "tip"]);
            run(&repo, &["branch", "-f", "twr/t", &tip]);
            let file = graft(&repo, &tip, &lost, legacy);
            let gone = dir.path().join("worktrees").join("proj").join("t");
            let out = to_trash(&g, &repo, &gone, Some("twr/t")).await.unwrap();
            assert!(out.branch_deleted, "{out:?}");
            let item = out.item.unwrap();
            assert!(
                manifest(&g, &item)
                    .await
                    .refs
                    .iter()
                    .any(|r| r.kind == "reflog" && r.target == lost),
                "legacy: {legacy}"
            );
            ungraft(&repo, &tip, &file, legacy);
            gc_now(&repo);
            assert!(has_object(&repo, &lost), "legacy: {legacy}");
            assert!(kept_by_trash(&repo, &item, &lost), "legacy: {legacy}");
        }
    }

    /// Removal never takes a graft for a branch reaching a commit only the
    /// worktree's reflog has: a replacement or a legacy graft making it look like an
    /// ancestor of the thread's branch keeps the worktree all the same.
    #[tokio::test]
    async fn a_graft_never_lets_removal_take_a_commit_only_the_worktree_has() {
        for legacy in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let g = LocalGit::default();
            let repo = init_repo(&dir.path().join("repo"));
            let wt = new_wt(&g, &repo, &dir.path().join("wt"), "twr/r").await;
            run(&wt, &["checkout", "-q", "--detach"]);
            std::fs::write(wt.join("experiment.txt"), "work").unwrap();
            commit_all(&wt, "detached experiment");
            let lost = run(&wt, &["rev-parse", "HEAD"]);
            run(&wt, &["checkout", "-q", "twr/r"]);
            let tip = run(&repo, &["rev-parse", "twr/r"]);
            let file = graft(&repo, &tip, &lost, legacy);
            let reason = kept_reason(
                g.worktree_remove(repo.clone(), wt.clone(), Some("twr/r".into()))
                    .await
                    .unwrap(),
            );
            assert!(reason.contains(&lost[..12]), "legacy: {legacy}: {reason}");
            assert!(wt.exists(), "legacy: {legacy}");
            ungraft(&repo, &tip, &file, legacy);
        }
    }

    /// Two purges at once of abandoned incomplete items of one repository that
    /// share a commit nothing else keeps: each would count the other's refs as the copy that
    /// survives it. They run one at a time (the repository's purge lock), the second checks
    /// again once the first is done: one item goes, the other keeps the only copy, and the
    /// commit survives the hardest gc.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_purges_never_both_take_a_shared_only_copy() {
        let dir = tempfile::tempdir().unwrap();
        let g = with_trash(&LocalGit::default(), dir.path());
        let repo = init_repo(&dir.path().join("repo"));
        let tree = run(&repo, &["rev-parse", "HEAD^{tree}"]);
        let unique = run(&repo, &["commit-tree", &tree, "-p", "HEAD", "-m", "unique"]);
        let mut items = Vec::new();
        for name in ["a", "b"] {
            let branch = format!("twr/{name}");
            run(&repo, &["branch", &branch, &unique]);
            let gone = dir.path().join("worktrees").join("proj").join(name);
            let out = to_trash(&g, &repo, &gone, Some(&branch)).await.unwrap();
            assert!(out.branch_deleted && !out.worktree_moved, "{out:?}");
            let item = out.item.unwrap();
            // Its deletion failed part way after the branch went, long ago.
            let file = g.trash_root().unwrap().join(&item).join("manifest.json");
            let mut m: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
            m["complete"] = false.into();
            std::fs::write(&file, serde_json::to_vec_pretty(&m).unwrap()).unwrap();
            age_manifest(&g, &item, Duration::from_secs(7200));
            items.push(item);
        }
        assert_eq!(
            list(&g).await.iter().filter(|i| i.kept.is_none()).count(),
            2
        );
        // Each purge, once it passed its check, waits a while for the other to pass its own.
        let checked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = Arc::clone(&checked);
        let racing = g.clone().with_step_hook(move |step, _| {
            if step == Step::PurgeChecked {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let until = std::time::Instant::now() + Duration::from_secs(2);
                while c.load(std::sync::atomic::Ordering::SeqCst) < 2
                    && std::time::Instant::now() < until
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            Ok(())
        });
        let purge = |item: &String| {
            racing.trash_purge(TrashPurge {
                items: vec![item.clone()],
                before_ms: None,
            })
        };
        let (a, b) = tokio::join!(purge(&items[0]), purge(&items[1]));
        let (a, b) = (a.unwrap(), b.unwrap());
        assert_eq!(a.purged.len() + b.purged.len(), 1, "{a:?} {b:?}");
        assert_eq!(checked.load(std::sync::atomic::Ordering::SeqCst), 1);
        let failed = a.failed.iter().chain(&b.failed).collect::<Vec<_>>();
        assert_eq!(failed.len(), 1, "{a:?} {b:?}");
        assert!(
            failed[0].error.contains("the only copy of 1 commit"),
            "{failed:?}"
        );
        gc_now(&repo);
        assert!(has_object(&repo, &unique));
        assert!(kept_by_trash(&repo, &failed[0].item, &unique));
    }

    /// A restore whose `git reset --hard` does not finish in time: git and what it
    /// started are gone before the restore returns, and before the reservation of the new
    /// worktree goes; what the restore made is left as it is (the error says so), never
    /// removed on a guess, and the item is as it was. An undo then removes what it made.
    #[tokio::test]
    async fn a_restore_whose_git_times_out_leaves_what_it_made() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        std::fs::write(wt.join("work.txt"), "uncommitted").unwrap();
        let item = to_trash(&g, &repo, &wt, Some("twr/t"))
            .await
            .unwrap()
            .item
            .unwrap();
        let before = manifest(&g, &item).await;
        let fake = root.join("fake");
        std::fs::create_dir(&fake).unwrap();
        let slow = with_trash(
            &git_except(&fake, "reset --hard", &slow_change(&fake, 4)),
            &root,
        );
        let probe = reservation_probe(&slow, &wt, &fake);
        let e = restore_at(&slow, &item, "twr/t", &wt).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(was_killed(&e), "{msg}");
        assert!(msg.contains("left as it is"), "{msg}");
        let pids = pids_in(&fake.join("pids"));
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(pids.iter().all(|p| !pid_alive(*p)), "{pids:?}");
        assert_eq!(probe.await.unwrap(), Vec::<u32>::new());
        // Left as it is: the new worktree (its record marked) and the branch.
        assert!(wt.join(".git").exists() && branch_exists(&repo, "twr/t"));
        let listed = list(&g).await;
        let i = listed.iter().find(|i| i.manifest.item == item).unwrap();
        assert!(i.restored.is_some(), "{i:?}");
        assert_eq!(i.manifest, before);
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(!fake.join("marker").exists());
        // An undo removes it (git reaped: nothing changes it meanwhile).
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!wt.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(manifest(&g, &item).await, before);
    }

    /// Parking's removal of a worktree whose `git worktree remove` does not
    /// finish in time: git and what it started are gone before the removal fails, and before
    /// the path's reservation goes; the worktree stays.
    #[tokio::test]
    async fn a_removal_whose_git_times_out_is_reaped_before_its_reservation_goes() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let fake = root.join("fake");
        std::fs::create_dir(&fake).unwrap();
        let g = git_except(&fake, "worktree remove", &slow_change(&fake, 4));
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        let probe = reservation_probe(&g, &wt, &fake);
        let e = g
            .worktree_remove(repo.clone(), wt.clone(), Some("twr/t".into()))
            .await
            .unwrap_err();
        assert!(was_killed(&e), "{e:#}");
        let pids = pids_in(&fake.join("pids"));
        assert_eq!(pids.len(), 2, "{pids:?}");
        assert!(pids.iter().all(|p| !pid_alive(*p)), "{pids:?}");
        assert_eq!(probe.await.unwrap(), Vec::<u32>::new());
        assert!(wt.exists() && worktrees(&repo).contains(&wt));
        tokio::time::sleep(Duration::from_millis(4500)).await;
        assert!(!fake.join("marker").exists());
    }

    /// Flags a step hook raises: the blocking job began, and it ended.
    #[derive(Clone, Default)]
    struct JobFlags {
        began: Arc<std::sync::atomic::AtomicBool>,
        ended: Arc<std::sync::atomic::AtomicBool>,
    }

    impl JobFlags {
        fn began(&self) -> bool {
            self.began.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn ended(&self) -> bool {
            self.ended.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// What a hook runs at its step: notes the beginning, sleeps `ms`, notes the end, then
        /// panics when `panic`.
        fn slow(&self, ms: u64, panic: bool) {
            use std::sync::atomic::Ordering::SeqCst;
            self.began.store(true, SeqCst);
            std::thread::sleep(Duration::from_millis(ms));
            self.ended.store(true, SeqCst);
            assert!(!panic, "injected panic in a blocking job");
        }
    }

    /// Waits until no operation of `g` that changes the box runs any more.
    async fn changes_over(g: &LocalGit) {
        assert!(
            g.changes_done(Duration::from_secs(60)).await,
            "a change still runs"
        );
    }

    /// A restore whose caller went away while its blocking copy runs (the hub's
    /// connection aborted) runs on to its end as its own task, holding the item's lock and the
    /// place's reservation until the copy is over: meanwhile an undo and a purge of the item
    /// are refused (busy), a runtime cannot start in the place, and a worktree made at the same
    /// place waits until the restore is done (and then finds the place taken). The restore
    /// completes: its worktree has the saved files, and an undo then removes it.
    #[tokio::test]
    async fn a_restore_whose_caller_went_away_holds_everything_until_its_copy_ended() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let (repo, _wt, item) = trashed_thread(&g, &root).await;
        let flags = JobFlags::default();
        let f = flags.clone();
        let slow = g.clone().with_step_hook(move |step, _| {
            if step == Step::Copying {
                f.slow(1500, false);
            }
            Ok(())
        });
        let back = root.join("worktrees/t-back");
        let caller = tokio::spawn(slow.trash_restore(RestoreRequest {
            item: item.clone(),
            branch: "twr/t".into(),
            paths: vec![back.clone()],
            other_branches: Some(Vec::new()),
        }));
        wait_until("the copy began", || flags.began()).await;
        caller.abort();
        let _ = caller.await;
        assert!(g.changes_running() > 0);
        // Its reservation of the place stays: no runtime starts there.
        assert!(g.workdirs().begin_use(&back, "rt").is_err());
        // Its lock stays: an undo and a purge are refused while the copy runs.
        let e = g.trash_restore_undo(item.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("busy"), "{e:#}");
        assert!(!flags.ended(), "the copy ended before the undo was refused");
        let purged = g
            .trash_purge(TrashPurge {
                items: vec![item.clone()],
                before_ms: None,
            })
            .await
            .unwrap();
        assert!(purged.purged.is_empty(), "{purged:?}");
        assert!(purged.failed[0].error.contains("busy"), "{purged:?}");
        assert!(
            !flags.ended(),
            "the copy ended before the purge was refused"
        );
        // A worktree made at the same place waits until the restore is done.
        let e = g
            .worktree_create(spec(&repo, &back, "twr/other", None))
            .await
            .unwrap_err();
        assert!(
            flags.ended(),
            "a create at the place ran while the copy did"
        );
        assert!(format!("{e:#}").contains("git worktree add"), "{e:#}");
        changes_over(&g).await;
        // The restore went through: the saved files are there, the item says so.
        assert_eq!(
            std::fs::read_to_string(back.join("work.txt")).unwrap(),
            "more work"
        );
        assert!(branch_exists(&repo, "twr/t"));
        let listed = list(&g).await;
        assert!(listed[0].restored.is_some(), "{listed:?}");
        assert!(g.workdirs().begin_use(&back, "rt").is_ok());
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!back.exists() && !branch_exists(&repo, "twr/t"));
    }

    /// A move to the trash whose caller went away while its blocking move of the
    /// worktree runs goes on to its end, holding the worktree's reservation and the item's
    /// lock until then: no runtime starts in the worktree, a purge of the item is refused, and
    /// a worktree made at the same path waits until the move is done (then makes a new one
    /// there, which nothing of the move touches). The item is complete.
    #[tokio::test]
    async fn a_trash_move_whose_caller_went_away_holds_everything_until_it_ended() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root).with_copy_moves();
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        std::fs::write(wt.join("work.txt"), "work").unwrap();
        let flags = JobFlags::default();
        let (f, moved) = (flags.clone(), wt.clone());
        let slow = g.clone().with_step_hook(move |step, p| {
            if step == Step::Moving && p == moved {
                f.slow(1500, false);
            }
            Ok(())
        });
        let item = twr_core::event::new_id();
        let caller = tokio::spawn(slow.worktree_trash(TrashRequest {
            item: item.clone(),
            repo: repo.clone(),
            worktree: wt.clone(),
            branch: Some("twr/t".into()),
            other_branches: None,
            info: twr_core::git::TrashInfo {
                thread: "t-id".into(),
                thread_name: "t".into(),
                box_name: "local".into(),
                deleted_at_ms: 1000,
                ..Default::default()
            },
        }));
        wait_until("the move began", || flags.began()).await;
        caller.abort();
        let _ = caller.await;
        let e = g.workdirs().begin_use(&wt, "rt").unwrap_err();
        assert!(e.contains("being removed"), "{e}");
        let purged = g
            .trash_purge(TrashPurge {
                items: vec![item.clone()],
                before_ms: None,
            })
            .await
            .unwrap();
        assert!(purged.failed[0].error.contains("busy"), "{purged:?}");
        assert!(
            !flags.ended(),
            "the move ended before the purge was refused"
        );
        // The same path made again waits for the move, then is a worktree of its own.
        let made = g
            .worktree_create(spec(&repo, &wt, "twr/again", None))
            .await
            .unwrap();
        assert!(flags.ended(), "a create at the path ran while the move did");
        assert!(made.new_branch);
        changes_over(&g).await;
        assert!(wt.join(".git").exists() && !wt.join("work.txt").exists());
        let m = manifest(&g, &item).await;
        assert!(m.complete && m.worktree_moved && m.branch_deleted, "{m:?}");
        let trashed = g.trash_root().unwrap().join(&item).join("worktree");
        assert_eq!(
            std::fs::read_to_string(trashed.join("work.txt")).unwrap(),
            "work"
        );
    }

    /// A park's removal whose caller went away while its git runs is not killed:
    /// it runs to its end, and the path stays reserved until then (no runtime starts there,
    /// `worktree_exists` waits and sees it removed, a worktree made there again waits).
    #[tokio::test]
    async fn a_removal_whose_caller_went_away_runs_to_its_end() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let fake = root.join("fake");
        std::fs::create_dir(&fake).unwrap();
        let ended = fake.join("ended");
        let g = git_except(
            &fake,
            "worktree remove",
            &format!(
                "touch '{f}/started'\nsleep 1\ngit \"$@\"\ns=$?\ntouch '{e}'\nexit $s",
                f = fake.display(),
                e = ended.display()
            ),
        );
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        let caller =
            tokio::spawn(g.worktree_remove(repo.clone(), wt.clone(), Some("twr/t".into())));
        wait_until("git began", || fake.join("started").exists()).await;
        caller.abort();
        let _ = caller.await;
        assert!(g.workdirs().begin_use(&wt, "rt").is_err());
        assert!(!g.worktree_exists(wt.clone()).await.unwrap());
        assert!(ended.exists(), "worktree_exists answered while git ran");
        assert!(!worktrees(&repo).contains(&wt));
        changes_over(&g).await;
        g.worktree_create(spec(&repo, &wt, "twr/t", None))
            .await
            .unwrap();
        assert!(wt.join(".git").exists());
    }

    /// A panic in a restore's blocking copy reaches the restore only once the copy
    /// job is over: the item's lock is held until then (an undo is refused), and the restore
    /// then fails like any failed step (what it made removed, the item as it was). A panic
    /// of the operation's own task lets its lock go (no job runs then) and fails the caller.
    #[tokio::test]
    async fn a_panic_in_a_restores_copy_lets_the_item_go_only_once_the_copy_ended() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let (repo, _wt, item) = trashed_thread(&g, &root).await;
        let before = manifest(&g, &item).await;
        let flags = JobFlags::default();
        let f = flags.clone();
        let panicking = g.clone().with_step_hook(move |step, _| {
            if step == Step::Copying {
                f.slow(1200, true);
            }
            Ok(())
        });
        let back = root.join("worktrees/t-back");
        let restore = tokio::spawn(panicking.trash_restore(RestoreRequest {
            item: item.clone(),
            branch: "twr/t".into(),
            paths: vec![back.clone()],
            other_branches: Some(Vec::new()),
        }));
        wait_until("the copy began", || flags.began()).await;
        let e = g.trash_restore_undo(item.clone()).await.unwrap_err();
        assert!(format!("{e:#}").contains("busy"), "{e:#}");
        assert!(!flags.ended());
        let e = restore.await.unwrap().unwrap_err();
        assert!(flags.ended());
        let msg = format!("{e:#}");
        assert!(
            msg.contains("panicked") && msg.contains("what it made is removed"),
            "{msg}"
        );
        assert!(!back.exists() && !branch_exists(&repo, "twr/t"));
        assert_eq!(manifest(&g, &item).await, before);
        // A panic of the task itself (in the restore's own code, between jobs).
        let task_panics = g.clone().with_step_hook(|step, _| {
            assert!(step != Step::Added, "injected panic in the restore");
            Ok(())
        });
        let e = restore_at(&task_panics, &item, "twr/t", &back)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("panicked"), "{e:#}");
        changes_over(&g).await;
        // Its lock went with it: an undo removes what it made (found by its mark).
        assert!(g.trash_restore_undo(item.clone()).await.unwrap());
        assert!(!back.exists() && !branch_exists(&repo, "twr/t"));
        restore_at(&g, &item, "twr/t", &back).await.unwrap();
    }

    /// Adds a linked worktree of the repository `of` at `at`, on a detached `HEAD` with a
    /// commit of its own (on no branch, not pushed): its id.
    fn linked_with_commit(of: &Path, at: &Path) -> String {
        run(
            of,
            &["worktree", "add", "-q", "--detach", &at.to_string_lossy()],
        );
        std::fs::write(at.join("side.txt"), "side").unwrap();
        commit_all(at, "side");
        run(at, &["rev-parse", "HEAD"]).trim().to_string()
    }

    /// A worktree holding a linked worktree of another repository (its `.git` a
    /// file naming a record outside what the deletion moves) is not moved to the trash: the
    /// inventory names it, the move is refused before anything is made or moved, and that
    /// repository's worktree list stays valid (a prune and gc there keep its detached commit).
    /// Once it is moved away with git, the deletion goes through. The same for a linked
    /// worktree of the thread's own repository.
    #[tokio::test]
    async fn a_linked_worktree_inside_refuses_the_move_to_the_trash() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let other = init_repo(&root.join("other"));
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        let side = wt.join("tools/side");
        std::fs::create_dir_all(wt.join("tools")).unwrap();
        let commit = linked_with_commit(&other, &side);
        let inv = g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap();
        assert_eq!(
            inv.linked,
            [LinkedWorktree {
                path: side.display().to_string(),
                record: other.join(".git/worktrees/side").display().to_string(),
                repo: other.display().to_string(),
                same_repository: false,
            }]
        );
        let e = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains(&format!(
                "t's worktree contains a linked worktree of another repository at {} (its \
                 record is {}): move or remove it first (`git -C {} worktree move",
                side.display(),
                other.join(".git/worktrees/side").display(),
                other.display()
            )) && msg.contains("nothing is touched"),
            "{msg}"
        );
        assert!(list(&g).await.is_empty());
        assert!(side.join("side.txt").exists() && branch_exists(&repo, "twr/t"));
        assert!(worktrees(&repo).contains(&wt));
        assert!(worktrees(&other).contains(&side));
        run(&other, &["worktree", "prune"]);
        gc_now(&other);
        assert!(worktrees(&other).contains(&side) && has_object(&other, &commit));
        // Moved away with git: the deletion goes through, and the other repository's worktree
        // list is still right.
        let away = root.join("side-moved");
        run(
            &other,
            &[
                "worktree",
                "move",
                &side.to_string_lossy(),
                &away.to_string_lossy(),
            ],
        );
        assert!(g
            .worktree_inventory(repo.clone(), wt.clone())
            .await
            .unwrap()
            .linked
            .is_empty());
        to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap();
        assert!(!wt.exists());
        run(&other, &["worktree", "prune"]);
        gc_now(&other);
        assert!(worktrees(&other).contains(&away) && has_object(&other, &commit));
        assert_eq!(run(&away, &["rev-parse", "HEAD"]).trim(), commit);

        // A linked worktree of the thread's own repository inside it.
        let wt2 = new_wt(&g, &repo, &root.join("worktrees/proj/u"), "twr/u").await;
        let own = wt2.join("nested");
        linked_with_commit(&repo, &own);
        let e = to_trash(&g, &repo, &wt2, Some("twr/u")).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("a linked worktree of its own repository at"),
            "{e:#}"
        );
        assert!(own.join("side.txt").exists() && worktrees(&repo).contains(&own));
        let inv = g.worktree_inventory(repo.clone(), wt2).await.unwrap();
        assert!(inv.linked[0].same_repository, "{inv:?}");
    }

    /// A park keeps a worktree holding a linked worktree of another repository:
    /// in a tracked directory (`git status` shows nothing; the walk finds its `.git`) and in an
    /// untracked one (`git status` shows it). The move to the trash refuses the first too.
    #[tokio::test]
    async fn a_linked_worktree_inside_keeps_the_worktree_on_park() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = with_trash(&LocalGit::default(), &root);
        let repo = init_repo(&root.join("repo"));
        let other = init_repo(&root.join("other"));
        // A tracked directory that is the checkout of a linked worktree of `other` (moved in
        // by hand, its record repaired to name it).
        let wt = new_wt(&g, &repo, &root.join("worktrees/proj/t"), "twr/t").await;
        std::fs::create_dir_all(wt.join("tracked")).unwrap();
        std::fs::write(wt.join("tracked/f"), "f").unwrap();
        commit_all(&wt, "tracked");
        let first = root.join("first");
        linked_with_commit(&other, &first);
        std::fs::rename(first.join(".git"), wt.join("tracked/.git")).unwrap();
        run(
            &other,
            &["worktree", "repair", &wt.join("tracked").to_string_lossy()],
        );
        assert!(worktrees(&other).contains(&wt.join("tracked")));
        assert!(run(&wt, &["status", "--porcelain"]).is_empty());
        let kept = remove_async(&g, &repo, &wt, "twr/t").await;
        assert!(kept_reason(kept).contains("tracked/"));
        assert!(wt.join("tracked/.git").exists());
        let e = to_trash(&g, &repo, &wt, Some("twr/t")).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("contains a linked worktree of another repository"),
            "{e:#}"
        );
        assert!(wt.join("tracked/.git").exists() && list(&g).await.is_empty());
        // An untracked directory.
        let wt2 = new_wt(&g, &repo, &root.join("worktrees/proj/u"), "twr/u").await;
        linked_with_commit(&other, &wt2.join("loose"));
        let kept = remove_async(&g, &repo, &wt2, "twr/u").await;
        assert!(matches!(kept, RemoveOutcome::KeptDirty { .. }), "{kept:?}");
        assert!(wt2.join("loose/side.txt").exists());
        assert_eq!(worktrees(&other).len(), 3);
    }

    /// Removes a test's incomplete item and its refs as a user would by hand.
    fn remove_item_by_hand(g: &LocalGit, repo: &Path, item: &str) {
        for r in run(
            repo,
            &[
                "for-each-ref",
                "--format=%(refname)",
                &format!("refs/twr-trash/{item}/"),
            ],
        )
        .lines()
        {
            run(repo, &["update-ref", "-d", r]);
        }
        trash::remove_tree(&g.trash_root().unwrap().join(item)).unwrap();
    }

    #[test]
    fn df_output_parses_even_with_spaces() {
        assert_eq!(
            parse_df(
                "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                 /dev/sda1 201000000 95000000 106000000 48% /\n"
            ),
            (Some(106_000_000 * 1024), Some(201_000_000 * 1024))
        );
        assert_eq!(
            parse_df("header\nmy fs 10 5 4 56% /mnt/my disk\n"),
            (Some(4096), Some(10240))
        );
        assert_eq!(parse_df(""), (None, None));
        assert_eq!(
            parse_df("h\n/dev/sda1 1000 1100 -100 110% /\n"),
            (Some(0), Some(1000 * 1024))
        );
        assert_eq!(parse_df("header\nno capacity here\n"), (None, None));
    }

    /// A trim deletes the ignored build output of `[threads] park_disposable` and nothing else:
    /// not an ignored file of another name, not a tracked directory of a build output's name,
    /// not untracked files. Measured before, it said so.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn trim_deletes_ignored_build_output_and_nothing_else() {
        use twr_core::git::TrimRequest;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/w").await;
        std::fs::write(
            wt.join(".gitignore"),
            "target/\nnode_modules/\n.env\n.DS_Store\n",
        )
        .unwrap();
        std::fs::create_dir_all(wt.join("build")).unwrap();
        std::fs::write(wt.join("build/keep.txt"), "tracked").unwrap();
        commit_all(&wt, "ignore and build");
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/bin"), vec![1u8; 256 * 1024]).unwrap();
        std::fs::create_dir_all(wt.join("node_modules/x")).unwrap();
        std::fs::write(wt.join("node_modules/x/i.js"), "x").unwrap();
        std::fs::write(wt.join(".env"), "SECRET=1").unwrap();
        std::fs::write(wt.join("notes.txt"), "untracked").unwrap();
        // An ignored file of the list, not a directory.
        std::fs::write(wt.join(".DS_Store"), vec![2u8; 64 * 1024]).unwrap();

        let r = g.disk_usage(Some(wt.clone())).await.unwrap();
        let d = r.worktree.unwrap();
        // The tracked build/ counts by its name too: the walk goes by name alone.
        assert!(d.build_bytes >= 256 * 1024, "{d:?}");
        assert!(d.bytes > d.build_bytes && !d.partial, "{d:?}");
        assert!(r.free.is_some_and(|f| f > 0) && r.total.is_some());
        // Without a worktree: the free space alone.
        let free_only = g.disk_usage(None).await.unwrap();
        assert!(free_only.worktree.is_none() && free_only.free.is_some());

        let req = TrimRequest {
            repo: repo.clone(),
            worktree: wt.clone(),
            branch: Some("twr/w".into()),
            other_branches: Some(Vec::new()),
            only: None,
        };
        let out = g.worktree_trim(req.clone()).await.unwrap();
        let mut deleted = out.deleted.clone();
        deleted.sort();
        assert_eq!(deleted, [".DS_Store", "node_modules/", "target/"]);
        // The file counts by its own blocks.
        assert!(out.freed >= (256 + 64) * 1024, "{out:?}");
        assert!(out.notes.is_empty(), "{out:?}");
        assert!(!wt.join("target").exists() && !wt.join("node_modules").exists());
        assert!(wt.join(".env").exists());
        assert!(wt.join("build/keep.txt").exists());
        assert!(wt.join("notes.txt").exists());
        let leftovers: Vec<String> = std::fs::read_dir(&wt)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".twr-build-output"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // Again: nothing left to delete.
        let again = g.worktree_trim(req).await.unwrap();
        assert!(again.deleted.is_empty() && again.freed == 0, "{again:?}");

        // Never the main checkout, nor a directory that is no worktree.
        let main = g
            .worktree_trim(TrimRequest {
                repo: repo.clone(),
                worktree: repo.clone(),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(format!("{main:#}").contains("not trimming"), "{main:#}");
        let gone = g
            .worktree_trim(TrimRequest {
                repo: repo.clone(),
                worktree: root.join("nowhere"),
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(format!("{gone:#}").contains("is not there"), "{gone:#}");

        // Another thread's checkout moved to the thread's path is not the thread's to trim.
        let other = new_wt(&g, &repo, &root.join("other"), "twr/other").await;
        std::fs::create_dir_all(other.join("target")).unwrap();
        std::fs::write(other.join("target/o"), "o").unwrap();
        run(
            &repo,
            &["worktree", "remove", "--force", wt.to_str().unwrap()],
        );
        run(
            &repo,
            &[
                "worktree",
                "move",
                other.to_str().unwrap(),
                wt.to_str().unwrap(),
            ],
        );
        let moved = g
            .worktree_trim(TrimRequest {
                repo: repo.clone(),
                worktree: wt.clone(),
                branch: Some("twr/w".into()),
                other_branches: Some(vec![twr_core::git::OtherBranch {
                    repo: repo.clone(),
                    branch: "twr/other".into(),
                }]),
                only: None,
            })
            .await
            .unwrap_err();
        assert!(format!("{moved:#}").contains("not trimming"), "{moved:#}");
        assert!(wt.join("target/o").exists());
    }

    /// An idle trim ([`TrimRequest::only`](twr_core::git::TrimRequest::only): `[threads]
    /// idle_trim_disposable`) deletes only the build output both lists name: an ignored
    /// `target/` goes, `build/` and `dist/` stay, and so does one only the idle list names;
    /// an empty idle list deletes nothing. A trim without it (`twr trim`) deletes all three.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[tokio::test]
    async fn an_idle_trim_deletes_only_what_both_lists_name() {
        use twr_core::git::TrimRequest;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/w").await;
        std::fs::write(
            wt.join(".gitignore"),
            "target/\nbuild/\ndist/\nout/\nsub/\n",
        )
        .unwrap();
        commit_all(&wt, "ignore");
        let make = || {
            for d in ["target/debug", "build", "dist", "out"] {
                std::fs::create_dir_all(wt.join(d)).unwrap();
                std::fs::write(wt.join(d).join("f"), vec![1u8; 8192]).unwrap();
            }
        };
        make();
        let idle_list: Vec<String> = twr_core::config::DEFAULT_IDLE_TRIM_DISPOSABLE
            .iter()
            .map(|s| s.to_string())
            .collect();
        let req = |only: Option<Vec<String>>| TrimRequest {
            repo: repo.clone(),
            worktree: wt.clone(),
            branch: Some("twr/w".into()),
            other_branches: Some(Vec::new()),
            only,
        };

        // Of the default park list, the default idle list: target/ only.
        let out = g.worktree_trim(req(Some(idle_list.clone()))).await.unwrap();
        assert_eq!(out.deleted, ["target/"], "{out:?}");
        assert!(out.freed >= 8192, "{out:?}");
        assert!(!wt.join("target").exists());
        for d in ["build/f", "dist/f", "out/f"] {
            assert!(wt.join(d).exists(), "{d}");
        }

        // An entry only the idle list names is no build output to a park: it stays.
        let out = g
            .worktree_trim(req(Some(vec!["out/".into(), "build/".into()])))
            .await
            .unwrap();
        assert_eq!(out.deleted, ["build/"], "{out:?}");
        assert!(wt.join("out/f").exists() && wt.join("dist/f").exists());

        // An empty idle list: nothing, and it says why.
        make();
        let out = g.worktree_trim(req(Some(Vec::new()))).await.unwrap();
        assert!(out.deleted.is_empty() && out.freed == 0, "{out:?}");
        assert!(
            out.notes.iter().any(|n| n.contains("idle_trim_disposable")),
            "{out:?}"
        );

        // `twr trim`: everything park_disposable names.
        let out = g.worktree_trim(req(None)).await.unwrap();
        let mut deleted = out.deleted.clone();
        deleted.sort();
        assert_eq!(deleted, ["build/", "dist/", "target/"], "{out:?}");
        assert!(wt.join("out/f").exists());

        // The two lists name the same component: an ignored `build/` (park list) in a tracked
        // `vendored/` (idle list only) stays.
        std::fs::create_dir_all(wt.join("vendored/pkg/build")).unwrap();
        std::fs::write(wt.join("vendored/pkg/keep.txt"), "k").unwrap();
        commit_all(&wt, "vendored");
        std::fs::write(wt.join("vendored/pkg/build/f"), vec![1u8; 8192]).unwrap();
        let out = g
            .worktree_trim(req(Some(vec!["vendored/".into()])))
            .await
            .unwrap();
        assert!(out.deleted.is_empty(), "{out:?}");
        assert!(wt.join("vendored/pkg/build/f").exists());
        let out = g.worktree_trim(req(None)).await.unwrap();
        assert_eq!(out.deleted, ["vendored/pkg/build/"], "{out:?}");
    }

    /// A trim goes by the mount list as a deletion does: none (a system other than Linux and
    /// macOS) refuses it, as does a mount point inside the worktree in macOS's form of the list
    /// (the points themselves), and one outside it stops nothing.
    #[tokio::test]
    async fn a_trim_needs_a_mount_list_with_no_mount_inside() {
        use twr_core::git::TrimRequest;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let g = LocalGit::default();
        let repo = init_repo(&root.join("repo"));
        let wt = new_wt(&g, &repo, &root.join("wt"), "twr/m").await;
        std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
        commit_all(&wt, "ignore");
        std::fs::create_dir_all(wt.join("target/shared")).unwrap();
        std::fs::write(wt.join("target/shared/f"), "f").unwrap();
        let req = TrimRequest {
            repo: repo.clone(),
            worktree: wt.clone(),
            branch: Some("twr/m".into()),
            other_branches: Some(Vec::new()),
            only: None,
        };

        let none = FakeMounts::set(&wt, trash::MountList::None);
        let e = g.worktree_trim(req.clone()).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("only where it can list the mount points (Linux and macOS)"),
            "{e:#}"
        );
        drop(none);

        let inside = FakeMounts::set(
            &wt,
            trash::MountList::Points(vec![
                PathBuf::from("/"),
                wt.join("target/shared"),
                root.join("wt-old"),
            ]),
        );
        let e = g.worktree_trim(req.clone()).await.unwrap_err();
        assert!(
            format!("{e:#}").contains("mounted in the worktree at target/shared"),
            "{e:#}"
        );
        assert!(wt.join("target/shared/f").exists());
        drop(inside);

        let _outside = FakeMounts::set(
            &wt,
            trash::MountList::Points(vec![PathBuf::from("/"), root.join("wt-old/target")]),
        );
        let out = g.worktree_trim(req).await.unwrap();
        assert_eq!(out.deleted, ["target/"], "{out:?}");
    }

    /// A worktree's walk counts build output by name, nested ones too, and follows no symlink.
    #[test]
    fn the_disk_walk_counts_build_output_by_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        std::fs::create_dir_all(w.join("app/node_modules/m")).unwrap();
        std::fs::write(w.join("app/node_modules/m/a.js"), vec![0u8; 64 * 1024]).unwrap();
        std::fs::create_dir_all(w.join("src")).unwrap();
        std::fs::write(w.join("src/main.rs"), vec![0u8; 8 * 1024]).unwrap();
        // A file named like a directory entry is no build output.
        std::fs::write(w.join("target"), "a file").unwrap();
        std::os::unix::fs::symlink("/usr", w.join("link")).unwrap();
        let list: Vec<String> = DEFAULT_PARK_DISPOSABLE
            .iter()
            .map(|s| s.to_string())
            .collect();
        let d = trash::worktree_disk(w, &list);
        assert!(
            d.build_bytes >= 64 * 1024 && d.build_bytes < 80 * 1024,
            "{d:?}"
        );
        assert!(d.bytes >= d.build_bytes + 8 * 1024, "{d:?}");
        // The symlink to /usr counts as itself, not what it points at.
        assert!(d.bytes < 1 << 20, "{d:?}");
        // An unreadable directory makes the reading a lower bound.
        std::fs::create_dir(w.join("locked")).unwrap();
        std::fs::set_permissions(w.join("locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::read_dir(w.join("locked")).is_ok(); // root reads anything
        assert_eq!(trash::worktree_disk(w, &list).partial, !readable);
        std::fs::set_permissions(w.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir(w.join("locked")).unwrap();
        // A directory that a symlink replaces is not followed: listed as a directory, it is
        // opened without following (the walk sees the swap only between listing and opening,
        // so here the link is simply what is there).
        std::fs::create_dir(w.join("swap")).unwrap();
        std::fs::remove_dir(w.join("swap")).unwrap();
        std::os::unix::fs::symlink("/usr", w.join("swap")).unwrap();
        let swapped = trash::worktree_disk(w, &list);
        assert!(swapped.bytes < 1 << 20, "{swapped:?}");
        std::fs::remove_file(w.join("swap")).unwrap();
        // A hard link counts once.
        std::fs::hard_link(w.join("src/main.rs"), w.join("src/copy.rs")).unwrap();
        assert_eq!(trash::worktree_disk(w, &list).bytes, d.bytes);
    }

    /// The walk's limit bounds what it reads, not only what it counts: a directory with more
    /// entries than the limit is read no further than it (a buffer of the system's beyond).
    #[test]
    fn the_disk_walk_stops_reading_a_huge_directory_at_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        std::fs::create_dir(w.join("huge")).unwrap();
        for i in 0..20_000 {
            std::fs::write(w.join("huge").join(format!("f{i:05}")), b"").unwrap();
        }
        dirfd::LISTED.with(|n| n.set(0));
        let d = trash::disk_walk(w, &[], 100);
        let listed = dirfd::LISTED.with(|n| n.get());
        assert!(d.partial, "{d:?}");
        assert!(
            listed <= 101,
            "read {listed} names for a walk of at most 100"
        );
        // Within the limit, all of it, and nothing partial.
        dirfd::LISTED.with(|n| n.set(0));
        let d = trash::disk_walk(w, &[], 30_000);
        assert!(!d.partial, "{d:?}");
        assert_eq!(dirfd::LISTED.with(|n| n.get()), 20_001);
    }
}

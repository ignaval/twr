//! Deleting threads: `thread.delete {thread}` or `{threads}` (`twr delete`, the orchestrator's
//! `delete_thread` tool).
//!
//! Nothing is deleted without the user's confirmation of what goes, and what goes goes to the
//! trash of the thread's box first (`hub::trash`): its worktree, its administrative record and
//! its branch, kept `[trash] keep_days` (then purged; `twr trash empty` sooner, `twr trash
//! restore` puts the thread back). So a loss the card misses can still be recovered. The
//! request raises a delete card: a question the hub raises on [`HUB_THREAD`] (`ext.twr.`[`DELETE_KEY`] = `{threads:
//! [{thread, name, lifecycle}], losses: [{thread, pending, loss}], asked_by, replaces?}`, options
//! [`DELETE`] and [`CANCEL`]) whose summary says what goes with each thread, and which records
//! it (`losses`, a [`Loss`] each, so a replay knows what the card said):
//!
//! - the runtime (stopped);
//! - the hub-managed worktree (with its size on disk) and what it holds, all moved to the
//!   trash: its uncommitted files (each
//!   untracked file counts), tracked files whose edits `git status` hides (assume-unchanged or
//!   skip-worktree), and its ignored files, by top-level entry with counts (`.env, tools/
//!   (12 files)`), a few named and the rest counted. A worktree that could not be inspected is
//!   said to be so, never clean. A parked thread's may be gone already;
//! - its build output, deleted rather than moved to the trash, with its size (`deletes its
//!   build output (target/, 14 GB) instead of moving it to the trash`): the ignored entries of
//!   `[threads] park_disposable` holding no git repository, which are regenerated (the
//!   worktree's size above leaves them out);
//! - every git repository removing it deletes, keyed off what the removal takes (the
//!   directory when it is there, and the worktree's administrative record either way), each
//!   with its commits on none of its remote-tracking branches: every git directory in the
//!   record, at any depth (git keeps a linked worktree's submodules' there, `modules/<name>`:
//!   `deletes the repository of its submodule libs/x with 1 commit on none of its
//!   remote-tracking branches (not pushed)`; also when the directory is gone, shown by their
//!   git directory), and those nested in the worktree, in any directory (tracked, untracked
//!   or ignored: `git status` shows none in a tracked directory), whose git
//!   directory is in it or in the record: bare repositories (a directory that is a git
//!   directory, by git's own test), `.git` directories, and `.git` files naming one there
//!   (`deletes the git repository nested at tools/x with ...`). A repository inside the
//!   worktree counts its files with the worktree's, under its path, and a bare one whose
//!   files are all tracked and unchanged (a test fixture) loses nothing. A linked worktree
//!   checked out in it whose record is elsewhere (of another repository, or another worktree
//!   of the same one) refuses the deletion: moved away, that record would name a
//!   missing checkout, and `git worktree prune` + `git gc` in its repository would lose the
//!   commits only it has; the card ends `It cannot be deleted now: api's worktree contains a
//!   linked worktree of another repository at <path> (its record is <record>): move or remove
//!   it first (...)`, `Delete` deletes nothing of that thread (an error item says so; it stays
//!   parked), and its box refuses the move before anything is made or moved. The worktree is
//!   searched for repositories within 200,000 entries; a directory not searched to the end is
//!   said (`could not search vendor/ to the end for git repositories`; its top level reads
//!   `./`). What of a repository
//!   cannot be checked is said (`(not checked: ...)`), never "nothing lost"; so is what is left
//!   of a git directory git would not take (an object store with a `HEAD` or `refs/`). File
//!   names are
//!   shown escaped where they are not plain text (`\\`, `\t`, `\xNN`): two names never read
//!   the same, so a file the card did not list is never taken for one it did;
//! - the commits deleting the card's threads loses, by one rule ([`GitOps::branch_only_commits`],
//!   for the card and for the check after Confirm alike). What goes: each thread's branch (its
//!   tip and every commit its reflog names) and its worktree's administrative record (its
//!   `HEAD` and `HEAD` reflog; also when the directory is gone already, the deletion clears the
//!   record). What stays: every other ref (branches, tags, remote-tracking branches,
//!   `refs/stash`, notes), every other reflog, and the `HEAD` of every other worktree. A commit
//!   is lost when what goes reaches it and nothing that stays does: on its `twr/<name>` branch
//!   or in its reflog, or only in its worktree's `HEAD` or `HEAD` reflog (made on a detached
//!   `HEAD`). Every thread of the card goes, so a commit only two of its threads have is lost,
//!   and the card says which threads share it (`of its lost commits, shared with b: 1 commit`).
//!   Counted per repository (its git common directory, whichever checkout of it a thread was
//!   created through) on each box: threads on other boxes or repositories have nothing in
//!   common. A reflog, record or object that cannot be read leaves that thread's commits not
//!   checked (the card says so), never "nothing lost";
//! - the branch its worktree is on when that is not the thread's (the thread switched its own
//!   worktree to it, as threads that integrate their own work do), which stays in the
//!   repository like every other branch the thread made, with its commits on no other branch,
//!   tag, remote, stash or reflog (`leaves the branch twr/api-final its worktree is on in the
//!   repository, with 3 commits on no other branch, tag or remote (not pushed)`). A worktree
//!   on another thread's recorded branch (the box is told which those are, [`Core::other_branches`])
//!   or whose record was made for another path is another thread's checkout: its box refuses
//!   to count or move it, and the card says its commits were not checked;
//! - whether the branch existed before the thread, and the queued prompts.
//!
//! A card is for each thread's life it was raised in (`lifecycle`: the seq of the thread's
//! latest `thread.created` or `thread.restored`, [`ThreadView::lifecycle_seq`]): a thread
//! deleted (by another card) and restored since, under any name and with a new branch and
//! worktree, is another life, and the card deletes nothing of it.
//!
//! Like a confirm card, only the user answers it: the orchestrator gets `relay_forbidden`, and
//! clients show it with the confirm cards (`is_confirm_card`). One card covers one or more
//! threads (at most [`THREADS_MAX`]); a thread may be on several open cards. Asking again for
//! exactly the threads of an open card checks them again: when they have nothing to lose that
//! the card does not say and are in the lives it was raised for, that card is returned
//! (`existing: true`, with what it says); otherwise a new card replaces it (the open one is
//! resolved `superseded`; the new one names it in `replaces` and its summary says what is new
//! or which thread was deleted and restored since).
//!
//! [`DELETE`] (any case) records the answer and deletes the card's threads in the background
//! ([`Hub::run_delete_card`]); a thread deleted meanwhile (by another card) is skipped. When a
//! thread of the card is in another life than the card's, nothing is deleted or stopped: the
//! answer is recorded with what changed (`ext.twr.changed`: `["a was deleted and restored since
//! (now b)"]`), and the card's other threads, checked again as they are now, get a new card
//! that replaces it (`replaces`; its summary says what changed and that it leaves those
//! threads out), as when there is more to lose than a card said; with no other thread left,
//! there is no new card (the orchestrator gets a notice when it asked). A card whose threads
//! are all deleted, or in another life, closes when the last of them is deleted. Otherwise:
//!
//! 1. Each thread is parked like `thread.park {keep, force}` (a running turn is interrupted,
//!    the runtime stopped and its exit recorded), unless it is parked already. Then the
//!    deletion waits (at most 20 s) until the hub is done with its runtimes (a kill of what was
//!    left of one it gave up on can outlast the recorded exit, [`Core::runtime_outstanding`]):
//!    nothing writes in its worktree any more, and what finishes those counts on the thread's
//!    bookkeeping, which goes with the thread. Nothing is stopped before the user confirms.
//! 2. What deleting them loses is checked again, all of them together, holding their
//!    repositories ([`Reservations`]: each repository, by its git common directory on its box,
//!    or the whole box when that is not known): deletions of threads of one repository run one
//!    after another from this check through their removals, and a check made before another
//!    deletion there was done is made again once it is (a commit two cards each said the
//!    other's branch keeps is lost by the time the second runs). Every other change of the hub
//!    that removes something of a repository holds it the same way, from its own check
//!    through its removal: a park's removal of a clean worktree (which keeps it when that
//!    would lose a commit, so a park that waited for a deletion keeps a worktree whose reflog
//!    alone has a commit now), a resume's or new thread's creation of a worktree (which clears
//!    a stale record at its path), and the removal of one made for a thread that was not
//!    created; a check made before one of them was done is made again too. When any of them has
//!    something to lose that the confirmed card did not say (a file or commit it did not list,
//!    build output it said goes to the trash, a commit of a repository in the worktree, more queued prompts, or a worktree, commits, a
//!    repository or a directory that cannot be checked now while the card could), nothing is
//!    deleted: the threads stay parked, and a new card for them (`replaces` the confirmed one) says what is new and that their runtimes stay stopped. The
//!    orchestrator gets a notice when it asked. Confirming that card starts over.
//! 3. Otherwise, one thread after another, on the thread's box ([`GitOps::worktree_trash`]): a
//!    new trash item keeps every commit its branch, worktree `HEAD`, reflogs and own refs reach
//!    (`refs/twr-trash/<item>/`), its hub-managed worktree moves into the item whatever it
//!    holds (a missing one only has its record moved, locked or not; a directory that lost its
//!    `.git` moves too), then its record (git forgets the worktree), then its branch is
//!    deleted; the item's manifest says what it holds and what the card said it loses. When
//!    part of the worktree could not be moved (copied to a trash on another file system, then
//!    files it may not remove), what is left stays and `thread.deleted` names it
//!    (`worktree_left`). A thread that works in the repository itself (`no_worktree`) or
//!    outside git keeps its directory (that is not twr's), and nothing of it goes to the trash.
//! 4. `thread.deleted` (what the check of step 2 found, and the trash item: `trash`,
//!    `trash_dir`, `trash_bytes`, `losses`) is recorded in one batch with the
//!    resolution of the thread's open items, of every other open delete card whose threads are
//!    all deleted now, and the drop of its queue. The registry sets the thread aside and drops
//!    every later event of it (until a `thread.restored`), so it leaves every listing, and its
//!    name and branch are free again; its earlier events stay in the log. The new item is the
//!    hub's (`Core::trash_busy`) from before step 3 makes it until this is recorded: nothing
//!    purges or reconciles it meanwhile. When this cannot be recorded after step 3 moved
//!    everything, the thread stays `deleting` with an error item, and the hub records it from
//!    the item's manifest once it can ([`Hub::reconcile_trash`]; also after a crash).
//!
//! Meanwhile ([`ThreadRt::deleting`], set for every thread of the card when it is confirmed,
//! cleared when a new card replaces it) the thread takes no turn, prompt, settings change,
//! resume, attach or park, and no attach starts inside its worktree
//! (`Core::worktree_change_over`). A step that fails stops that thread's deletion there: it
//! stays (parked, its worktree possibly gone) with an error item saying what failed, asking
//! again finishes the job, and the other threads of the card go on. [`CANCEL`] (or dismissing
//! the card) changes nothing.
//!
//! Refused, for the whole request (and when the card is confirmed, which then stays open):
//! an orchestrator thread (the current one: switch the orchestrator with `twr orchestrator use`
//! first; a retired one stays, as the record of an earlier orchestrator), an attached thread
//! (detach first), a thread another thread is attached inside the
//! worktree of (a remote box cannot see that attach, so the hub keeps its directory; checked
//! again right before the removal), a thread being parked, resumed or deleted, and a thread on
//! a box that is offline (its runtime and worktree are there).
//!
//! [`ThreadRt::deleting`]: super::ThreadRt

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use twr_core::adapter::Answer;
use twr_core::event::*;
use twr_core::git::{
    human_bytes, DeleteRefs, GitOps, IgnoredGroup, Listed, OtherBranch, RepoLoss, TrashInfo,
    TrashRequest, WorktreeInventory, TRASH_BIG_BYTES,
};
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::{AttentionItem, ThreadView};

use super::git::Workspace;
use super::{
    append_err, rpc, twr_ext, twr_ext_map, Batch, Caller, Core, Hub, Inner, ThreadHost,
    ATTACH_QUIESCE_TIMEOUT,
};

/// The option that deletes the threads.
pub const DELETE: &str = "Delete";

/// The option that keeps them.
pub const CANCEL: &str = "Cancel";

/// Most threads one `thread.delete` names.
pub const THREADS_MAX: usize = 32;

/// How often a deletion looks again whether its thread's runtime bookkeeping is over
/// ([`Core::runtime_outstanding`]).
const SETTLE_POLL: Duration = Duration::from_millis(20);

/// Most names a card shows of a list (hidden files, ignored entries); the rest are counted.
const SHOWN: usize = 5;

/// The answer that resolves an open delete card a new one replaces.
const SUPERSEDED: &str = super::proposal::SUPERSEDED;

/// `ext.twr` key of the `attention.resolved` of a delete card confirmed while threads of it
/// were in another life than its own: what became of each (`["a was deleted and restored
/// since (now b)"]`). Nothing was deleted.
pub(super) const CHANGED_KEY: &str = "changed";

/// What deleting a thread would lose: as a delete card says it (and records it, so a replay
/// knows), or as checked again once the card is confirmed.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Loss {
    /// The hub-managed worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<String>,
    /// Its directory is gone already.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    worktree_missing: bool,
    /// What removing it deletes: what it holds, and the repositories its record holds (only
    /// those when its directory is gone). `None`: not inspected, `unknown` says why.
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory: Option<WorktreeInventory>,
    /// Lost commits only its own `HEAD` or `HEAD` reflog reaches (made on a detached `HEAD`),
    /// also when only the worktree's record is left.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_only_commits: Option<u64>,
    /// The thread's branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The branch existed before the thread (`thread.created` `ext.twr.existing_branch`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    existing_branch: bool,
    /// Lost commits that branch (its tip or its reflog) reaches (`None`: no such branch, or
    /// not known).
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_only_commits: Option<u64>,
    /// The branch does not exist.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    branch_missing: bool,
    /// The branch its worktree is on when that is not the thread's branch (the thread switched
    /// its worktree to it): it stays in the repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout: Option<String>,
    /// That branch's commits on no other branch, tag, remote, stash or reflog once the thread
    /// is gone (`None`: not counted, or no such branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_only_commits: Option<u64>,
    /// Every commit it loses (its branch's and its `HEAD`'s), by id (`None`: not counted).
    #[serde(skip_serializing_if = "Option::is_none")]
    commits: Option<Listed>,
    /// Of those, how many each other thread deleted with it also reaches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    shared: Vec<Shared>,
    /// The directory a thread without a worktree of its own works in (it stays).
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    /// What could not be checked, and why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unknown: Vec<String>,
}

/// Lost commits a thread shares with another one deleted with it ([`Loss::shared`]).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Shared {
    /// The other thread's name.
    name: String,
    /// How many commits.
    commits: u64,
}

/// `n thing(s)`.
fn count(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// The first [`SHOWN`] of `names`, then `+K more` for the rest of `total`.
fn shown(names: impl Iterator<Item = String>, total: u64) -> String {
    let names: Vec<String> = names.take(SHOWN).collect();
    let rest = total.saturating_sub(names.len() as u64);
    let mut out = names.join(", ");
    if rest > 0 {
        out.push_str(&format!(", +{rest} more"));
    }
    out
}

/// An ignored entry as a card names it: a file by its path, a directory with its file count.
fn ignored_entry(g: &IgnoredGroup) -> String {
    if !g.path.ends_with('/') && g.files <= 1 {
        return g.path.clone();
    }
    match (g.files, g.more) {
        (0, true) => format!("{} (files not counted)", g.path),
        (n, true) => format!("{} ({n}+ files)", g.path),
        (n, false) => format!("{} ({})", g.path, count(n, "file")),
    }
}

/// What deleting a worktree does to the repository `s` in it.
fn repo_part(s: &RepoLoss) -> String {
    let mut out = if s.nested {
        format!("with it the git repository nested at {}", s.path)
    } else {
        format!("with it the repository of its submodule {}", s.path)
    };
    if s.commits.total > 0 {
        out.push_str(&format!(
            " with {} on none of its remote-tracking branches (not pushed)",
            count(s.commits.total, "commit")
        ));
    }
    if let Some(why) = &s.unchecked {
        out.push_str(&format!(" (not checked: {why})"));
    }
    out
}

impl Loss {
    /// Uncommitted files of its worktree, when inspected and there.
    fn dirty_files(&self) -> Option<u64> {
        self.inventory
            .as_ref()
            .filter(|_| !self.worktree_missing)
            .map(|i| i.dirty.total)
    }

    /// What deleting the thread does, one part each, with `pending` queued prompts.
    fn parts(&self, pending: u64) -> Vec<String> {
        let mut parts = vec!["stops its runtime".to_string()];
        let size = self
            .inventory
            .as_ref()
            .filter(|i| i.bytes > 0)
            .map(|i| format!(" ({})", human_bytes(i.bytes)))
            .unwrap_or_default();
        match (&self.worktree, self.worktree_missing, &self.inventory) {
            (Some(w), true, _) => parts.push(format!(
                "moves the record of its worktree {w} (already gone) to the trash"
            )),
            (Some(w), false, Some(inv)) => {
                let (dirty, hidden) = (inv.dirty.total, inv.hidden.total);
                parts.push(match (dirty, hidden) {
                    (0, 0) => {
                        format!(
                            "moves its worktree {w}{size} to the trash (no uncommitted changes)"
                        )
                    }
                    (0, _) => format!("moves its worktree {w}{size} to the trash"),
                    (n, _) => format!(
                        "moves its worktree {w}{size} to the trash with {} it has",
                        count(n, "uncommitted file")
                    ),
                });
                if hidden > 0 {
                    parts.push(format!(
                        "with it {} with edits git status does not show (assume-unchanged or \
                         skip-worktree: {})",
                        count(hidden, "tracked file"),
                        shown(inv.hidden.items.iter().cloned(), hidden)
                    ));
                }
                if inv.ignored_groups_total > 0 {
                    parts.push(format!(
                        "with it its ignored files ({})",
                        shown(
                            inv.ignored_groups.iter().map(ignored_entry),
                            inv.ignored_groups_total
                        )
                    ));
                }
                if inv.build_output.total > 0 {
                    parts.push(format!(
                        "deletes its build output ({}, {}) instead of moving it to the trash",
                        shown(
                            inv.build_output.items.iter().cloned(),
                            inv.build_output.total
                        ),
                        human_bytes(inv.build_output_bytes)
                    ));
                }
            }
            (Some(w), false, None) => parts.push(format!(
                "moves its worktree {w} to the trash, which could not be inspected (whatever it \
                 holds goes with it)"
            )),
            (None, ..) => {}
        }
        // In the worktree or in its record (also when its directory is gone).
        if let (Some(_), Some(inv)) = (&self.worktree, &self.inventory) {
            for repo in &inv.repositories {
                parts.push(repo_part(repo));
            }
            if !inv.unsearched.is_empty() {
                parts.push(format!(
                    "could not search {} to the end for git repositories (too many files, or \
                     unreadable: whatever repository is there goes unchecked)",
                    shown(inv.unsearched.iter().cloned(), inv.unsearched.len() as u64)
                ));
            }
        }
        if let Some(n) = self.head_only_commits.filter(|n| *n > 0) {
            parts.push(format!(
                "moves {} made on a detached HEAD in its worktree to the trash (on no branch, \
                 tag or remote; not pushed)",
                count(n, "commit")
            ));
        }
        let before = if self.existing_branch {
            ", which existed before the thread,"
        } else {
            ""
        };
        match (&self.branch, self.branch_missing, self.branch_only_commits) {
            (Some(_), true, _) | (None, ..) => {}
            (Some(b), false, Some(0)) => parts.push(format!(
                "moves its branch {b}{before} to the trash (each of its commits is on another \
                 branch, tag, remote, stash or reflog)"
            )),
            (Some(b), false, Some(n)) => parts.push(format!(
                "moves its branch {b}{before} to the trash, with {} on no other branch, tag or \
                 remote (not pushed)",
                count(n, "commit")
            )),
            (Some(b), false, None) => {
                parts.push(format!("moves its branch {b}{before} to the trash"))
            }
        }
        if let Some(on) = &self.checkout {
            let left = format!("leaves the branch {on} its worktree is on in the repository");
            parts.push(match self.checkout_only_commits {
                Some(0) => format!(
                    "{left} (each of its commits is on another branch, tag, remote, stash or \
                     reflog)"
                ),
                Some(n) => format!(
                    "{left}, with {} on no other branch, tag or remote (not pushed)",
                    count(n, "commit")
                ),
                None => left,
            });
        }
        if !self.shared.is_empty() {
            let with: Vec<String> = self
                .shared
                .iter()
                .map(|s| format!("{}: {}", s.name, count(s.commits, "commit")))
                .collect();
            parts.push(format!(
                "of its lost commits, shared with {} (deleted too)",
                with.join(", ")
            ));
        }
        if pending > 0 {
            parts.push(format!("drops {}", count(pending, "queued prompt")));
        }
        parts
    }

    /// Whether deleting the thread puts something in the trash: its worktree (or its record)
    /// or its branch.
    fn trashes(&self) -> bool {
        self.worktree.is_some() || self.branch.is_some()
    }

    /// What the card says it loses, in short, for the trash list and `thread.deleted`
    /// (`3 uncommitted files, 2 unpushed commits`; `nothing unique` when nothing).
    fn short(&self, pending: u64) -> String {
        let mut out = Vec::new();
        if let Some(n) = self.dirty_files().filter(|n| *n > 0) {
            out.push(count(n, "uncommitted file"));
        }
        if let Some(inv) = &self.inventory {
            if inv.hidden.total > 0 {
                out.push(count(inv.hidden.total, "hidden edit"));
            }
            if inv.ignored_groups_total > 0 {
                out.push(format!(
                    "ignored files ({})",
                    shown(
                        inv.ignored_groups.iter().map(|g| g.path.clone()),
                        inv.ignored_groups_total
                    )
                ));
            }
            let nested: u64 = inv.repositories.iter().map(|r| r.commits.total).sum();
            if nested > 0 {
                out.push(count(nested, "commit") + " of repositories in the worktree");
            }
        }
        if let Some(c) = self.commits.as_ref().filter(|c| c.total > 0) {
            out.push(count(c.total, "unpushed commit"));
        }
        if pending > 0 {
            out.push(count(pending, "queued prompt") + " (dropped)");
        }
        if !self.unknown.is_empty() || self.worktree.is_some() && self.inventory.is_none() {
            out.push("some not checked".into());
        }
        if out.is_empty() {
            "nothing unique".into()
        } else {
            out.join(", ")
        }
    }

    /// Why deleting the thread `name` is refused as things are: each linked worktree in its
    /// worktree whose record stays behind ([`twr_core::git::LinkedWorktree::refusal`]), and a
    /// file system mounted in it, or where they are mounted unknown
    /// ([`twr_core::git::mount_refusal`]).
    fn refusals(&self, name: &str) -> Vec<String> {
        let whose = format!("{name}'s worktree");
        self.inventory
            .iter()
            .flat_map(|i| {
                i.linked
                    .iter()
                    .map(|l| l.refusal(&whose))
                    .chain(twr_core::git::mount_refusal(
                        &whose,
                        &i.mounts,
                        i.mounts_unknown,
                    ))
            })
            .collect()
    }

    /// The card's summary for the one thread `name` with `pending` queued prompts.
    fn summary(&self, name: &str, project: Option<&str>, pending: u64) -> String {
        let mut out = format!("Delete thread {name}{}? This ", in_project(project));
        match self.parts(pending).split_last() {
            Some((last, [])) => out.push_str(last),
            Some((last, [one])) => out.push_str(&format!("{one} and {last}")),
            Some((last, rest)) => out.push_str(&format!("{}, and {last}", rest.join(", "))),
            None => {}
        }
        out.push('.');
        if !self.trashes() {
            out.push_str(" It cannot be undone.");
        }
        if let Some(d) = &self.directory {
            out.push_str(&format!(" It works in {d}, which stays as it is."));
        }
        for u in &self.unknown {
            out.push_str(&format!(" (Not checked: {u}.)"));
        }
        for r in self.refusals(name) {
            out.push_str(&format!(" It cannot be deleted now: {r}."));
        }
        out
    }

    /// The line of thread `name` on a card for several threads.
    fn line(&self, name: &str, project: Option<&str>, pending: u64) -> String {
        let mut out = format!(
            "- {name}{}: {}",
            in_project(project),
            self.parts(pending).join(", ")
        );
        if let Some(d) = &self.directory {
            out.push_str(&format!(
                "; it works in {d}, which stays as it is (cannot be undone)"
            ));
        }
        for u in &self.unknown {
            out.push_str(&format!(" (not checked: {u})"));
        }
        for r in self.refusals(name) {
            out.push_str(&format!("; it cannot be deleted now: {r}"));
        }
        out
    }

    /// What `self` (checked again, with `pending` queued prompts) has to lose that `card`
    /// (what a card said, with `card_pending`) did not say, for the user; empty: nothing. What
    /// the card could not check, it said nothing about, and the user confirmed it as such.
    fn added_since(&self, card: &Loss, pending: u64, card_pending: u64) -> Vec<&'static str> {
        let mut out = Vec::new();
        // A card that said the worktree was gone said it held no files, also when it could
        // not check its record's repositories.
        let gone = WorktreeInventory {
            gone: true,
            ..Default::default()
        };
        let card_files = card
            .inventory
            .as_ref()
            .or(card.worktree_missing.then_some(&gone));
        if let Some(old) = card_files {
            match &self.inventory {
                Some(new) => {
                    for (added, what) in [
                        (new.dirty.added_since(&old.dirty), "uncommitted files"),
                        (
                            new.hidden.added_since(&old.hidden),
                            "edits git status does not show",
                        ),
                        (new.ignored.added_since(&old.ignored), "ignored files"),
                        // Ignored files the card said go to the trash, deleted now.
                        (
                            new.build_output.added_since(&old.build_output),
                            "build output deleted, not trashed",
                        ),
                        (
                            card.inventory.is_some() && new.repositories_added_since(old),
                            "commits of repositories in the worktree",
                        ),
                    ] {
                        if added {
                            out.push(what);
                        }
                    }
                }
                None if self.worktree.is_some()
                    && (card.inventory.is_some() || !self.worktree_missing) =>
                {
                    out.push("its worktree cannot be inspected now")
                }
                None => {}
            }
        }
        if let Some(old) = &card.commits {
            match &self.commits {
                Some(new) if new.added_since(old) => out.push("commits on no branch that stays"),
                Some(_) => {}
                None => out.push("its commits cannot be counted now"),
            }
        }
        if self.checkout.is_some() && self.checkout != card.checkout {
            out.push("its worktree is on another branch");
        }
        if pending > card_pending {
            out.push("queued prompts");
        }
        out
    }
}

/// ` (project p)`, or nothing.
fn in_project(project: Option<&str>) -> String {
    project
        .map(|p| format!(" (project {p})"))
        .unwrap_or_default()
}

/// One thread a delete card names, as checked.
#[derive(Default)]
struct Target {
    id: String,
    view: ThreadView,
    workspace: Workspace,
    host: Option<Arc<dyn ThreadHost>>,
    loss: Loss,
    pending: u64,
    /// The git common directory of its repository on its box, as its box said when checking
    /// its commits (`None`: not known).
    common: Option<PathBuf>,
    /// The recorded branches of the other threads of its box ([`Core::other_branches`]).
    others: Vec<OtherBranch>,
}

impl Target {
    /// A copy of what a card says of it (tests).
    #[cfg(test)]
    fn clone_for_test(&self) -> Target {
        Target {
            id: self.id.clone(),
            view: self.view.clone(),
            loss: self.loss.clone(),
            pending: self.pending,
            ..Default::default()
        }
    }

    /// What a deletion of it holds ([`Reservations`]): its repository on its box (the whole
    /// box when its git common directory is not known); nothing when the deletion removes no
    /// worktree or branch of its (no worktree of its own, or no box to ask).
    fn reservation_key(&self) -> Option<RepoKey> {
        match (
            &self.workspace,
            &self.view.repo,
            &self.view.worktree,
            &self.host,
        ) {
            (Workspace::Worktree { .. }, Some(_), Some(_), Some(_)) => {
                Some((self.view.box_name.clone(), self.common.clone()))
            }
            _ => None,
        }
    }
}

/// A repository a deletion holds: its box, and its git common directory there (`None`: every
/// repository of the box).
type RepoKey = (String, Option<PathBuf>);

/// The repositories held by the hub's changes that remove something of a repository (a
/// worktree, a stale worktree record, a branch), so that they run one after another in each
/// repository, each holding it from its own check of what goes through its removal: a deletion
/// ([`Hub::run_delete_card`]) holds the repositories of its threads from its last check of
/// what goes through their removals; a park's removal of a clean worktree, a resume's or new
/// thread's creation of one (which clears a stale record at its path), and the removal of a
/// worktree made for a thread that was not created hold theirs around that git call, whose
/// checks run once it holds ([`Reservations::reserve_repository`]). What one deletion loses
/// depends on what stays in the repository (a commit another thread's branch or worktree
/// reflog also has is not lost), and those changes remove some of that: a deletion that
/// checked while another change was at work in its repositories checks again once that one is
/// done, and a park that waited for a deletion checks again what its removal would lose.
#[derive(Default)]
pub(super) struct Reservations {
    state: Mutex<Held>,
    /// Wakes the deletions waiting for a repository when one is released.
    released: tokio::sync::Notify,
}

/// What [`Reservations`] holds.
#[derive(Default)]
struct Held {
    /// By box: the git common directories held, and whether the whole box is.
    boxes: HashMap<String, BoxHeld>,
    /// By box: how many reservations of it were released so far (a deletion's check from
    /// before a release there may be out of date).
    releases: HashMap<String, u64>,
}

/// What of one box [`Held::boxes`] holds.
#[derive(Default)]
struct BoxHeld {
    whole: bool,
    repos: HashSet<PathBuf>,
}

impl Held {
    /// Whether nothing else holds `key`.
    fn free(&self, (box_name, common): &RepoKey) -> bool {
        let Some(b) = self.boxes.get(box_name) else {
            return true;
        };
        !b.whole
            && match common {
                Some(c) => !b.repos.contains(c),
                None => b.repos.is_empty(),
            }
    }
}

impl Reservations {
    /// How many reservations of each box were released so far.
    fn releases(&self) -> HashMap<String, u64> {
        self.lock().releases.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Held> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds the repository of the directory `repo` on box `box_name` (its git common
    /// directory, as `git` there says; the whole box when it cannot say), once nothing else
    /// holds it, for a change that removes something of it.
    pub(super) async fn reserve_repository(
        self: &Arc<Self>,
        box_name: &str,
        git: &dyn GitOps,
        repo: &Path,
    ) -> Reservation {
        let common = match git.git_common_dir(repo.to_path_buf()).await {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::debug!(repo = %repo.display(), error = %format!("{e:#}"), "no git common directory: holding the whole box");
                None
            }
        };
        self.reserve(BTreeSet::from([(box_name.to_string(), common)]))
            .await
    }

    /// Holds `keys`, all at once, once nothing else holds any of them (waiting as long as it
    /// takes: a change holding one is at work, and releases it when it is done).
    async fn reserve(self: &Arc<Self>, keys: BTreeSet<RepoKey>) -> Reservation {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            {
                let mut held = self.lock();
                if keys.iter().all(|k| held.free(k)) {
                    for (box_name, common) in &keys {
                        let b = held.boxes.entry(box_name.clone()).or_default();
                        match common {
                            Some(c) => {
                                b.repos.insert(c.clone());
                            }
                            None => b.whole = true,
                        }
                    }
                    return Reservation(Arc::new(Holding {
                        owner: Arc::clone(self),
                        keys,
                    }));
                }
            }
            released.await;
        }
    }
}

/// Repositories a change holds ([`Reservations::reserve`]), released when the last of its
/// holders is gone: the change itself, and each box operation it runs while holding them
/// ([`Reservation::during`]).
#[derive(Clone)]
pub(super) struct Reservation(Arc<Holding>);

/// What a [`Reservation`] holds; released when dropped.
struct Holding {
    owner: Arc<Reservations>,
    keys: BTreeSet<RepoKey>,
}

impl Reservation {
    /// Whether it holds every one of `keys`.
    fn covers(&self, keys: &BTreeSet<RepoKey>) -> bool {
        keys.is_subset(&self.0.keys)
    }

    /// Whether no reservation of its boxes was released since `before`
    /// ([`Reservations::releases`]): no other change removed anything there meanwhile.
    fn none_released_since(&self, before: &HashMap<String, u64>) -> bool {
        let held = self.0.owner.lock();
        self.0.keys.iter().all(|(box_name, _)| {
            held.releases.get(box_name).copied().unwrap_or(0)
                == before.get(box_name).copied().unwrap_or(0)
        })
    }

    /// Runs the box operation `op` (a [`GitOps`] change) as a task of its own that holds this
    /// reservation until `op` is over, and waits for it. When the caller goes away first (its
    /// request was dropped: a client gone, the server's connections aborted at shutdown), the
    /// task goes on: the repositories stay held until the box's answer, never released while
    /// the box may still change them. (The local box's operation runs to its end whatever
    /// its caller does, [`twr_runner::local::LocalGit`]; a remote box's request is answered
    /// once its runner reaped its git, or times out unsettled and keeps the box's later
    /// changes waiting, `crate::remote`.)
    pub(super) async fn during<T: Send + 'static>(
        &self,
        op: futures::future::BoxFuture<'static, anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let held = self.clone();
        let task = tokio::spawn(async move {
            let out = op.await;
            drop(held);
            out
        });
        match task.await {
            Ok(out) => out,
            Err(e) => Err(anyhow::anyhow!("the box operation's task failed: {e}")),
        }
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        let mut held = self.owner.lock();
        for (box_name, common) in &self.keys {
            if let Some(b) = held.boxes.get_mut(box_name) {
                match common {
                    Some(c) => {
                        b.repos.remove(c);
                    }
                    None => b.whole = false,
                }
                if !b.whole && b.repos.is_empty() {
                    held.boxes.remove(box_name);
                }
            }
            *held.releases.entry(box_name.clone()).or_default() += 1;
        }
        drop(held);
        self.owner.released.notify_waiters();
    }
}

/// The card's summary for `targets` (see [`Loss::summary`] and [`Loss::line`]), after
/// `header` when given, and what the trash does with them, which keeps items `keep_days`.
fn card_summary(targets: &[Target], header: Option<&str>, keep_days: u64) -> String {
    let mut body = match targets {
        [one] => one
            .loss
            .summary(&one.view.name, one.view.project.as_deref(), one.pending),
        _ => {
            let mut out = format!("Delete {} threads?", targets.len());
            for t in targets {
                out.push('\n');
                out.push_str(
                    &t.loss
                        .line(&t.view.name, t.view.project.as_deref(), t.pending),
                );
            }
            out
        }
    };
    let trashed: Vec<&Target> = targets.iter().filter(|t| t.loss.trashes()).collect();
    if !trashed.is_empty() {
        let days = format!("{keep_days} day{}", if keep_days == 1 { "" } else { "s" });
        let what = match trashed.as_slice() {
            [_] if targets.len() == 1 => "it",
            [_] => "the thread",
            _ => "them",
        };
        body.push_str(&format!(
            "\nThe trash keeps {what} for {days}, then purges {what}: `twr trash restore` brings \
             a thread back, `twr trash empty` purges sooner."
        ));
        let bytes: u64 = trashed
            .iter()
            .filter_map(|t| t.loss.inventory.as_ref())
            .map(|i| i.bytes)
            .sum();
        if bytes > TRASH_BIG_BYTES {
            body.push_str(&format!(
                " That holds {} on disk until then (`twr trash empty` frees it).",
                human_bytes(bytes)
            ));
        }
    }
    match header {
        Some(h) => format!("{h}\n{body}"),
        None => body,
    }
}

/// What each thread of `targets` has to lose that `card` did not say: `name: what, ...` per
/// thread with something new (a card that did not record what it said for a thread: all of it
/// is new).
fn added_since_card(card: &DeleteCard, targets: &[Target]) -> Vec<String> {
    targets
        .iter()
        .filter_map(|t| {
            let added = match card.said.get(&t.id) {
                Some((loss, pending)) => t.loss.added_since(loss, t.pending, *pending),
                None => vec!["what the card said was not recorded"],
            };
            (!added.is_empty()).then(|| format!("{}: {}", t.view.name, added.join(", ")))
        })
        .collect()
}

/// An open delete card, from its `attention.raised` (live and on replay).
#[derive(PartialEq, Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct DeleteCard {
    /// The threads it deletes, in order.
    pub(super) threads: Vec<String>,
    /// What it says each thread loses, with its queued prompts, by thread id (none for a card
    /// that did not record it).
    said: HashMap<String, (Loss, u64)>,
    /// Who asked for it ([`Source`] as a string).
    asked_by: Option<String>,
    /// The name each thread had on it, by thread id.
    names: HashMap<String, String>,
    /// Which life of each thread it was raised for ([`ThreadView::lifecycle_seq`]), by thread
    /// id (none recorded: no life of it).
    lives: HashMap<String, u64>,
}

impl DeleteCard {
    /// The delete card `p` is, if it is one naming threads. A card from before cards named
    /// several threads names one (`thread`).
    pub(super) fn from_raised(p: &AttentionRaised) -> Option<DeleteCard> {
        let card = twr_ext(&p.ext)?.get(DELETE_KEY)?;
        let listed: Vec<&Value> = match card.get("threads").and_then(Value::as_array) {
            Some(threads) => threads.iter().collect(),
            None => vec![card],
        };
        let threads: Vec<String> = listed
            .iter()
            .filter_map(|t| t.get("thread")?.as_str().map(str::to_string))
            .collect();
        let names = listed
            .iter()
            .filter_map(|t| {
                let id = t.get("thread")?.as_str()?;
                Some((id.to_string(), t.get("name")?.as_str()?.to_string()))
            })
            .collect();
        let lives = listed
            .iter()
            .filter_map(|t| {
                let id = t.get("thread")?.as_str()?;
                Some((id.to_string(), t.get("lifecycle")?.as_u64()?))
            })
            .collect();
        if threads.is_empty() {
            return None;
        }
        let said = card
            .get("losses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| {
                let thread = l.get("thread")?.as_str()?.to_string();
                let loss: Loss = serde_json::from_value(l.get("loss")?.clone()).ok()?;
                let pending = l.get("pending").and_then(Value::as_u64).unwrap_or(0);
                Some((thread, (loss, pending)))
            })
            .collect();
        let asked_by = card
            .get("asked_by")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(DeleteCard {
            threads,
            said,
            asked_by,
            names,
            lives,
        })
    }

    /// Whether the orchestrator asked for it.
    fn asked_by_orchestrator(&self) -> bool {
        self.asked_by.as_deref() == Some(&Source::Orchestrator.to_string())
    }

    /// Whether thread `id`, now in its life `lifecycle_seq` ([`ThreadView::lifecycle_seq`]), is
    /// in another life than the one this card was raised for: deleted and restored since
    /// (under any name, with a new branch and worktree). A card that did not record it was
    /// raised for no life of it.
    fn other_life(&self, id: &str, lifecycle_seq: u64) -> bool {
        self.lives.get(id) != Some(&lifecycle_seq)
    }

    /// What became of thread `id` (named `now`), in another life than this card's: `a was
    /// deleted and restored since (now b)`.
    fn life_note(&self, id: &str, now: &str) -> String {
        let was = self.names.get(id).map_or(now, String::as_str);
        if !self.lives.contains_key(id) {
            return format!("the card does not record which life of {was} it was raised for");
        }
        if was == now {
            format!("{was} was deleted and restored since")
        } else {
            format!("{was} was deleted and restored since (now {now})")
        }
    }

    /// What became of each of `targets` that is in another life than this card's
    /// ([`DeleteCard::life_note`]).
    fn lives_changed(&self, targets: &[Target]) -> Vec<String> {
        targets
            .iter()
            .filter(|t| self.other_life(&t.id, t.view.lifecycle_seq))
            .map(|t| self.life_note(&t.id, &t.view.name))
            .collect()
    }
}

/// The threads a delete card would delete, from its `attention.raised`, in order (empty: not
/// a delete card).
#[cfg(test)]
fn card_threads(p: &AttentionRaised) -> Vec<String> {
    DeleteCard::from_raised(p)
        .map(|c| c.threads)
        .unwrap_or_default()
}

impl Core {
    /// `conflict` while thread `id` is being deleted.
    pub(super) fn refuse_deleting(&self, id: &str) -> Result<(), RpcError> {
        if self.rt.get(id).is_some_and(|r| r.deleting) {
            let name = self.registry.get(id).map_or(id, |v| v.name.as_str());
            return Err(rpc(codes::CONFLICT, format!("{name} is being deleted")));
        }
        Ok(())
    }

    /// Why thread `id` cannot be deleted now (see the module docs).
    fn refuse_delete(&self, id: &str) -> Result<(), RpcError> {
        let view = self.view(id)?;
        let name = &view.name;
        if view.retired {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{name} is a retired orchestrator thread: it cannot be deleted (it stays as \
                     the record of an earlier orchestrator)"
                ),
            ));
        }
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{name} is the orchestrator thread: it cannot be deleted (switch the \
                     orchestrator with `twr orchestrator use <runtime>` first)"
                ),
            ));
        }
        self.refuse_deleting(id)?;
        let rt = self.rt.get(id);
        if let Some(c) = view
            .attached
            .clone()
            .or_else(|| rt.and_then(|r| r.attach.as_ref()).map(|a| a.client.clone()))
        {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} is attached by {c}; detach first, then delete it"),
            ));
        }
        if rt.is_some_and(|r| r.park.is_some() || r.resuming) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} is being parked or resumed; try again when that is done"),
            ));
        }
        if rt.is_some_and(|r| r.trimming) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name}'s build output is being trimmed; try again when that is done"),
            ));
        }
        self.refuse_attached_inside(id)
    }

    /// `conflict` when another thread is attached (or being attached) inside thread `id`'s
    /// hub-managed worktree: its interactive runtime works there.
    fn refuse_attached_inside(&self, id: &str) -> Result<(), RpcError> {
        let Some((box_name, worktree)) = self.managed_worktree(id) else {
            return Ok(());
        };
        match self.attach_inside(box_name, worktree, id) {
            Some((other, client)) => {
                let name = self.registry.get(id).map_or(id, |v| v.name.as_str());
                Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "thread {other} is attached (by {client}) and works in {name}'s \
                         worktree {}; end that attach first, then delete {name}",
                        worktree.display()
                    ),
                ))
            }
            None => Ok(()),
        }
    }

    /// The open delete card naming exactly the threads `ids` (in any order), if one is open.
    fn open_delete_card(&self, ids: &[String]) -> Option<(AttentionItem, DeleteCard)> {
        let mut want: Vec<&String> = ids.iter().collect();
        want.sort();
        let mut cards: Vec<&String> = self
            .delete_cards
            .iter()
            .filter(|(_, c)| {
                let mut have: Vec<&String> = c.threads.iter().collect();
                have.sort();
                have.dedup();
                have == want
            })
            .map(|(a, _)| a)
            .collect();
        cards.sort();
        cards.into_iter().find_map(|a| {
            Some((
                self.attention.get(a).cloned()?,
                self.delete_cards.get(a)?.clone(),
            ))
        })
    }

    /// Thread `id` as a deletion checks it: its view, workspace and box.
    fn delete_target(&self, hub: &Inner, id: &str) -> Result<Target, RpcError> {
        let view = self.view(id)?.clone();
        let workspace = self
            .rt
            .get(id)
            .map(|r| r.workspace.clone())
            .unwrap_or_default();
        let host = hub.host(&view.box_name);
        let others = self.other_branches(id, &view.box_name);
        Ok(Target {
            id: id.to_string(),
            view,
            workspace,
            host,
            others,
            ..Default::default()
        })
    }

    /// The recorded branches of the live threads on box `box_name` other than `id`, each with
    /// its repository ([`twr_core::git::OtherBranches`]): a worktree of `id`'s on one of them
    /// (in the same repository, as the box tells) is another thread's checkout, which a
    /// deletion never takes and a restore never puts `id`'s worktree back on, while one on
    /// any other branch is its own worktree switched to that branch.
    pub(super) fn other_branches(&self, id: &str, box_name: &str) -> Vec<OtherBranch> {
        let mut out: Vec<OtherBranch> = self
            .registry
            .list()
            .into_iter()
            .filter(|t| t.id != id && t.box_name == box_name)
            .filter_map(
                |t| match (self.rt.get(&t.id).map(|r| &r.workspace), &t.repo) {
                    (Some(Workspace::Worktree { branch, .. }), Some(repo)) => Some(OtherBranch {
                        repo: PathBuf::from(repo),
                        branch: branch.clone(),
                    }),
                    _ => None,
                },
            )
            .collect();
        out.sort_by(|a, b| (&a.repo, &a.branch).cmp(&(&b.repo, &b.branch)));
        out.dedup();
        out
    }

    /// Reset withdraws deletion requests from the old orchestrator conversation. Cards
    /// requested directly by the user remain open, along with workers' attention.
    pub(super) fn supersede_orchestrator_delete_cards(&self, batch: &mut Batch, source: &Source) {
        let mut cards: Vec<&String> = self
            .delete_cards
            .iter()
            .filter(|(id, card)| card.asked_by_orchestrator() && self.attention.get(id).is_some())
            .map(|(id, _)| id)
            .collect();
        cards.sort();
        for id in cards {
            batch.push(
                HUB_THREAD,
                None,
                source.clone(),
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: id.clone(),
                    by: ResolvedBy::User,
                    answer: Some(Answer::new(SUPERSEDED)),
                    ..Default::default()
                }),
            );
        }
    }

    /// Raises delete card `attn_id` for `targets` (as checked, their queued prompts counted
    /// here), asked by `asked_by`, its summary after `header`; with `replaces`, the card it
    /// replaces, resolved [`SUPERSEDED`] in the same batch when it is still open. Returns the
    /// `thread.delete` result.
    fn raise_delete_card(
        &mut self,
        hub: &Inner,
        attn_id: String,
        targets: &mut [Target],
        asked_by: &str,
        header: Option<&str>,
        replaces: Option<&str>,
    ) -> Result<Value, RpcError> {
        let held = self.held_lost(&targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>());
        for t in targets.iter_mut() {
            t.pending = self.store.pending_count(&t.id).unwrap_or(0)
                + held.get(&t.id).copied().unwrap_or(0);
        }
        // Each thread in the life it was checked in: the card deletes no other.
        let threads: Vec<Value> = targets
            .iter()
            .map(|t| {
                json!({
                    "thread": t.id,
                    "name": t.view.name,
                    "lifecycle": t.view.lifecycle_seq,
                })
            })
            .collect();
        let losses: Vec<Value> = targets
            .iter()
            .map(|t| json!({ "thread": t.id, "pending": t.pending, "loss": t.loss }))
            .collect();
        let mut card = json!({
            "threads": threads,
            "losses": losses,
            "asked_by": asked_by,
        });
        let mut batch = Batch::default();
        if let Some(old) = replaces {
            card["replaces"] = json!(old);
            if self.attention.get(old).is_some() {
                batch.push(
                    HUB_THREAD,
                    None,
                    Source::Hub,
                    EventBody::AttentionResolved(AttentionResolved {
                        attn_id: old.to_string(),
                        by: ResolvedBy::User,
                        answer: Some(Answer::new(SUPERSEDED)),
                        ext: twr_ext_map(json!({ "superseded_by": attn_id })),
                        ..Default::default()
                    }),
                );
            }
        }
        batch.push(
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Question,
                summary: card_summary(targets, header, hub.config.trash.keep_days),
                options: vec![DELETE.into(), CANCEL.into()],
                ext: twr_ext_map(json!({ DELETE_KEY: card })),
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch).map_err(append_err)?;
        let item = self.open_item(&attn_id)?;
        let threads = targets
            .iter()
            .map(|t| thread_json(&t.id, &t.view.name, &t.loss, t.pending))
            .collect();
        let mut out = card_json(&item, threads, false);
        if let Some(old) = replaces {
            out["replaces"] = json!(old);
        }
        Ok(out)
    }

    /// Whether `decision`, an answer to delete card `item`, deletes ([`DELETE`]) or keeps
    /// ([`CANCEL`]) the threads, in any case; `bad_request` otherwise.
    fn deletes(item: &AttentionItem, decision: &str) -> Result<bool, RpcError> {
        let d = decision.trim();
        if d.eq_ignore_ascii_case(DELETE) {
            Ok(true)
        } else if d.eq_ignore_ascii_case(CANCEL) {
            Ok(false)
        } else {
            Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is a delete card: answer {DELETE} or {CANCEL} (or dismiss it)",
                    item.attn_id
                ),
            ))
        }
    }

    /// The user's answer to delete card `item` (`card`): `decision` as given, or a dismissal
    /// (`None`). [`DELETE`] checks every thread not deleted yet, records the answer and starts
    /// their deletion ([`Hub::run_delete_card`]); [`CANCEL`] or a dismissal only closes the
    /// card. When a thread of the card is in another life than the card's (deleted and
    /// restored since, [`DeleteCard::other_life`]), [`DELETE`] deletes nothing and stops
    /// nothing: the answer is recorded with what changed (`ext.twr.`[`CHANGED_KEY`]), and a
    /// new card for the card's other threads replaces it ([`Hub::replace_changed_card`]),
    /// as when there is more to lose than a card said. Returns the card's
    /// `attention.resolved`.
    pub(super) fn answer_delete_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        card: &DeleteCard,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{} asks the user to confirm deleting threads: only the user answers it \
                     ({DELETE} or {CANCEL}); tell the user it is waiting for them",
                    item.attn_id
                ),
            ));
        }
        let delete = match decision {
            Some(d) => Self::deletes(item, d)?,
            None => false,
        };
        // Deleted meanwhile (by another card): nothing left to do for those.
        let live: Vec<String> = card
            .threads
            .iter()
            .filter(|t| !self.registry.is_deleted(t))
            .cloned()
            .collect();
        // Deleted and restored meanwhile: not the thread the user saw on the card.
        let changed: Vec<String> = if delete {
            live.iter()
                .filter_map(|t| {
                    let v = self.registry.get(t)?;
                    card.other_life(t, v.lifecycle_seq)
                        .then(|| card.life_note(t, &v.name))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !changed.is_empty() {
            return self.answer_changed_card(hub, item, card, &live, &changed, caller);
        }
        if delete {
            for t in &live {
                self.refuse_delete(t)?;
                let view = self.view(t)?.clone();
                self.require_box(&view)?;
            }
        }
        let answer = match decision {
            Some(_) if delete => Answer::new(DELETE),
            Some(_) => Answer::new(CANCEL),
            None => Answer::new(super::route::CANCEL),
        };
        let ev = self
            .append(
                hub,
                HUB_THREAD,
                None,
                caller.source(),
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: item.attn_id.clone(),
                    by: ResolvedBy::User,
                    answer: Some(answer),
                    ..Default::default()
                }),
            )
            .map_err(append_err)?;
        if delete && !live.is_empty() {
            for t in &live {
                self.rt(t).deleting = true;
            }
            let (h, caller, card_id, card) = (
                Hub {
                    inner: Arc::clone(hub),
                },
                caller.clone(),
                item.attn_id.clone(),
                card.clone(),
            );
            tokio::spawn(async move {
                h.run_delete_card(&card_id, &card, live, &caller).await;
            });
        }
        Ok(ev)
    }

    /// [`DELETE`] to delete card `item` (`card`) when threads of it are in another life than
    /// the card's (`changed`, what became of each): records the answer with `changed`
    /// (`ext.twr.`[`CHANGED_KEY`]) and deletes nothing; the card's other threads still there
    /// (of `live`) get a new card replacing it ([`Hub::replace_changed_card`]). The
    /// orchestrator gets a notice when it asked and no new card comes.
    fn answer_changed_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        card: &DeleteCard,
        live: &[String],
        changed: &[String],
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        let rest: Vec<String> = live
            .iter()
            .filter(|t| {
                self.registry
                    .get(t)
                    .is_some_and(|v| !card.other_life(t, v.lifecycle_seq))
            })
            .cloned()
            .collect();
        let ev = self
            .append(
                hub,
                HUB_THREAD,
                None,
                caller.source(),
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: item.attn_id.clone(),
                    by: ResolvedBy::User,
                    answer: Some(Answer::new(DELETE)),
                    ext: twr_ext_map(json!({ CHANGED_KEY: changed })),
                    ..Default::default()
                }),
            )
            .map_err(append_err)?;
        if rest.is_empty() {
            if card.asked_by_orchestrator() && self.orch_active() {
                self.orch.push_info(format!(
                    "- delete card {}: the user confirmed it, but {}; nothing was deleted",
                    item.attn_id,
                    changed.join("; ")
                ));
                if std::mem::take(&mut self.orch.wake) {
                    let _ = hub.notify_tx.send(());
                }
            }
            return Ok(ev);
        }
        let (h, card_id, card, changed) = (
            Hub {
                inner: Arc::clone(hub),
            },
            item.attn_id.clone(),
            card.clone(),
            changed.to_vec(),
        );
        tokio::spawn(async move {
            h.replace_changed_card(&card_id, &card, rest, &changed)
                .await;
        });
        Ok(ev)
    }
}

impl Hub {
    /// `thread.delete {thread}` or `{threads}` (`queries`, one or more, at most
    /// [`THREADS_MAX`]; the same thread twice counts once): checks what deleting the threads
    /// loses and raises the delete card for them (see the module docs), or returns the open one
    /// naming exactly them when it says all of that (a new card replaces it otherwise). Returns
    /// `{status: "needs_confirmation", threads: [{thread, thread_name, pending, worktree?,
    /// branch?, dirty_files?, hidden_changes?, ignored?, ignored_entries?, build_output?,
    /// build_output_bytes?, repositories?, unsearched?,
    /// head_only_commits?, branch_only_commits?, shared?, existing_branch?, worktree_missing?,
    /// worktree_unchecked?}], thread, thread_name, attn_id, revision, summary, options,
    /// existing, replaces?, message}` (`thread` and `thread_name`: the first thread's;
    /// `repositories: [{path, nested?, commits, unchecked?}]`, see [`RepoLoss`], also those of a
    /// gone worktree's record; `unsearched`: directories of the worktree not searched to the
    /// end for repositories).
    pub async fn delete_threads(
        &self,
        queries: &[String],
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if queries.is_empty() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "name at least one thread to delete",
            ));
        }
        if queries.len() > THREADS_MAX {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "at most {THREADS_MAX} threads per request ({} given)",
                    queries.len()
                ),
            ));
        }
        let hub = &self.inner;
        // The boxes' operations, begun with their gate checks and held until the deletion is
        // recorded (or refused): no box is switched while its threads are checked
        // (`Core::reserve_box`).
        let (mut targets, _ops) = {
            let core = hub.core();
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let mut ids: Vec<String> = Vec::new();
            for q in queries {
                let id = core.resolve(q)?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
            for id in &ids {
                core.refuse_delete(id)?;
            }
            let mut targets = Vec::new();
            let mut ops = Vec::new();
            for id in ids {
                let view = core.view(&id)?;
                core.require_box(view)?;
                ops.extend(core.reserve_box(hub, &view.box_name)?);
                targets.push(core.delete_target(hub, &id)?);
            }
            (targets, ops)
        };
        Self::check_losses(&mut targets).await;
        let mut core = hub.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        // Checked again: the lock was released meanwhile.
        let ids: Vec<String> = targets.iter().map(|t| t.id.clone()).collect();
        for id in &ids {
            core.refuse_delete(id)?;
        }
        // What was checked is the life the card names: a thread deleted and restored while it
        // was checked is asked for again.
        for t in &targets {
            if core.view(&t.id)?.lifecycle_seq != t.view.lifecycle_seq {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} was deleted and restored while it was checked; ask again",
                        t.view.name
                    ),
                ));
            }
        }
        let held = core.held_lost(&targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>());
        for t in targets.iter_mut() {
            t.pending = core.store.pending_count(&t.id).unwrap_or(0)
                + held.get(&t.id).copied().unwrap_or(0);
        }
        let asked_by = caller.source().to_string();
        let Some((item, card)) = core.open_delete_card(&ids) else {
            return core.raise_delete_card(hub, new_id(), &mut targets, &asked_by, None, None);
        };
        // The open card is returned only for the lives it was raised for, saying all of it.
        let changed = card.lives_changed(&targets);
        let added = added_since_card(&card, &targets);
        if added.is_empty() && changed.is_empty() {
            return Ok(core.existing_card_json(&item, &card));
        }
        let mut why = Vec::new();
        if !changed.is_empty() {
            why.push(changed.join("; "));
        }
        if !added.is_empty() {
            why.push(format!("there is more to lose ({})", added.join("; ")));
        }
        let header = format!(
            "This card replaces {}: since it was raised, {}.",
            item.attn_id,
            why.join("; and ")
        );
        let attn_id = new_id();
        if card.asked_by_orchestrator() && !caller.is_orchestrator() {
            core.orch.push_info(format!(
                "- delete card {}: replaced by delete card {attn_id} (there is more to lose \
                 than it said); nothing was deleted, the new card waits for the user",
                item.attn_id
            ));
        }
        core.raise_delete_card(
            hub,
            attn_id,
            &mut targets,
            &asked_by,
            Some(&header),
            Some(&item.attn_id),
        )
    }

    /// Fills in what deleting each of `targets` (all deleted together) would lose, as their
    /// boxes say now: each worktree's inventory, then the lost commits, asked of each box for
    /// all its threads at once (the box takes those whose repositories share a git common
    /// directory as one deletion set, everything of it counting as gone).
    async fn check_losses(targets: &mut [Target]) {
        let checked = futures::future::join_all(
            targets
                .iter()
                .map(|t| Self::worktree_loss(&t.view, &t.workspace, t.host.as_ref())),
        )
        .await;
        for (t, loss) in targets.iter_mut().zip(checked) {
            t.loss = loss;
            t.common = None;
        }
        // Each box has its own repositories: threads on other boxes have nothing in common.
        let mut boxes: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, t) in targets.iter().enumerate() {
            if let (Workspace::Worktree { .. }, Some(_), Some(_), Some(_)) =
                (&t.workspace, &t.view.repo, &t.view.worktree, &t.host)
            {
                boxes.entry(t.view.box_name.clone()).or_default().push(i);
            }
        }
        for members in boxes.into_values() {
            // The worktree whether its directory is there or not: its record goes either way.
            let refs: Vec<DeleteRefs> = members
                .iter()
                .map(|&i| {
                    let t = &targets[i];
                    DeleteRefs {
                        repo: t.view.repo.as_ref().map(PathBuf::from).unwrap_or_default(),
                        branch: t.loss.branch.clone(),
                        worktree: t.view.worktree.as_ref().map(PathBuf::from),
                        other_branches: Some(t.others.clone()),
                    }
                })
                .collect();
            let Some(host) = targets[members[0]].host.clone() else {
                continue;
            };
            match host.git().branch_only_commits(refs).await {
                Ok(out) => {
                    let names: Vec<String> = members
                        .iter()
                        .map(|&i| targets[i].view.name.clone())
                        .collect();
                    for (&i, o) in members.iter().zip(out) {
                        targets[i].common = o.common.clone();
                        let loss = &mut targets[i].loss;
                        if let Some(why) = o.unchecked {
                            loss.unknown.push(format!("unpushed commits ({why})"));
                            continue;
                        }
                        loss.branch_only_commits = o.branch;
                        loss.branch_missing = o.branch.is_none();
                        loss.head_only_commits = o.head;
                        loss.checkout = o.checkout;
                        loss.checkout_only_commits = o.checkout_only;
                        loss.commits = Some(o.commits);
                        loss.shared = o
                            .shared
                            .iter()
                            .filter_map(|s| {
                                Some(Shared {
                                    name: names.get(s.with)?.clone(),
                                    commits: s.commits,
                                })
                            })
                            .collect();
                    }
                }
                Err(e) => {
                    for &i in &members {
                        targets[i]
                            .loss
                            .unknown
                            .push(format!("unpushed commits ({e:#})"));
                    }
                }
            }
        }
    }

    /// What deleting thread `view` (working in `workspace`, on `host`) would lose of its
    /// worktree, as its box says now (commits: [`Hub::check_losses`]): what its directory holds
    /// when it is there, and the repositories its record holds either way.
    async fn worktree_loss(
        view: &ThreadView,
        workspace: &Workspace,
        host: Option<&Arc<dyn ThreadHost>>,
    ) -> Loss {
        let mut loss = Loss::default();
        let (
            Workspace::Worktree {
                branch,
                existing_branch,
            },
            Some(repo),
            Some(path),
        ) = (workspace, &view.repo, &view.worktree)
        else {
            loss.directory = view.worktree.clone().or(view.repo.clone());
            return loss;
        };
        loss.worktree = Some(path.clone());
        loss.branch = Some(branch.clone());
        loss.existing_branch = *existing_branch;
        let Some(host) = host else {
            loss.unknown.push(format!(
                "its worktree and commits (box {:?} is not available)",
                view.box_name
            ));
            return loss;
        };
        let git = host.git();
        // Whatever the removal deletes: the directory when it is there, and the worktree's
        // record either way (the repositories it holds go with it). The box says which.
        match git.worktree_exists(PathBuf::from(path)).await {
            Ok(exists) => loss.worktree_missing = !exists,
            Err(e) => {
                loss.unknown.push(format!("its worktree ({e:#})"));
                return loss;
            }
        }
        match git
            .worktree_inventory(PathBuf::from(repo), PathBuf::from(path))
            .await
        {
            Ok(i) => {
                loss.worktree_missing = i.gone;
                loss.inventory = Some(i);
            }
            Err(e) if loss.worktree_missing => loss
                .unknown
                .push(format!("what its worktree's record holds ({e:#})")),
            Err(e) => loss
                .unknown
                .push(format!("what its worktree holds ({e:#})")),
        }
        loss
    }

    /// Deletes the threads `threads` of delete card `card_id` (`card`) once the user confirmed
    /// it (see the module docs): stops each, checks again what goes holding their repositories
    /// ([`Reservations`], again when another deletion released one meanwhile), and removes
    /// them only when the card said all of it; otherwise raises a new card. A thread whose
    /// step fails gets an error item and stays.
    pub(super) async fn run_delete_card(
        &self,
        card_id: &str,
        card: &DeleteCard,
        threads: Vec<String>,
        caller: &Caller,
    ) {
        // 1. Nothing writes in their worktrees any more.
        let mut stopped = Vec::new();
        for t in threads {
            match self.stop_for_delete(&t, caller).await {
                Ok(()) => stopped.push(t),
                Err(why) => self.delete_failed(&t, &why),
            }
        }
        // 2. What goes, checked again, all together.
        let mut targets = Vec::new();
        {
            let core = self.inner.core();
            for t in &stopped {
                match core.delete_target(&self.inner, t) {
                    Ok(target) => targets.push(target),
                    Err(e) => {
                        tracing::warn!(thread = %t, error = %e.message, "a thread being deleted is gone")
                    }
                }
            }
        }
        if targets.is_empty() {
            return;
        }
        // Holding their repositories from the check that counts through the removals; a
        // check from before another deletion released one of them is done again.
        let reservations = &self.inner.reservations;
        let mut held: Option<Reservation> = None;
        loop {
            let before = reservations.releases();
            Self::check_losses(&mut targets).await;
            let keys: BTreeSet<RepoKey> =
                targets.iter().filter_map(Target::reservation_key).collect();
            if held.as_ref().is_some_and(|h| h.covers(&keys)) {
                break;
            }
            // What it holds does not cover them all: released first (never waiting on itself).
            drop(held.take());
            let r = reservations.reserve(keys).await;
            let current = r.none_released_since(&before);
            held = Some(r);
            if current {
                break;
            }
        }
        {
            let mut core = self.inner.core();
            let held = core.held_lost(&targets.iter().map(|t| t.id.as_str()).collect::<Vec<_>>());
            for t in targets.iter_mut() {
                t.pending = core.store.pending_count(&t.id).unwrap_or(0)
                    + held.get(&t.id).copied().unwrap_or(0);
            }
            // A thread in another life than the card's is not the one confirmed (it cannot be
            // while being deleted: checked all the same).
            let mut added = card.lives_changed(&targets);
            added.extend(added_since_card(card, &targets));
            if !added.is_empty() {
                self.raise_again(&mut core, card_id, card, &mut targets, &added, caller);
                return;
            }
        }
        // 3. and 4. Never a thread whose worktree holds a linked worktree whose record stays
        // behind (its box refuses the move too): nothing of it moves.
        for t in &targets {
            let refused = t.loss.refusals(&t.view.name);
            if !refused.is_empty() {
                self.delete_failed(&t.id, &refused.join("; "));
                continue;
            }
            match self.remove_thread(t, caller, held.as_ref()).await {
                Ok(()) => {}
                Err(Removal::Failed(why)) => self.delete_failed(&t.id, &why),
                Err(Removal::Unrecorded { item, why }) => {
                    self.delete_unrecorded(&t.id, &item, &why)
                }
            }
        }
        drop(held);
    }

    /// Card `card_id` (`card`) was confirmed while threads of it were in another life than
    /// its own (`changed`, what became of each; [`Core::answer_changed_card`]): nothing was
    /// deleted, and a new card replacing it asks again for `threads`, its other threads, as
    /// they are now (checked like `thread.delete`), leaving the changed ones out. A thread that
    /// cannot be asked for now (deleted or restored meanwhile, refused) is left out too and
    /// named; with none left there is no card.
    pub(super) async fn replace_changed_card(
        &self,
        card_id: &str,
        card: &DeleteCard,
        threads: Vec<String>,
        changed: &[String],
    ) {
        let hub = &self.inner;
        // Still the card's thread in the card's life, and deletable now; `Err((box, why))`,
        // `box` when only its box refuses (offline, or being switched).
        let usable = |core: &Core, id: &str| -> Result<(), (bool, String)> {
            let view = core.view(id).map_err(|e| (false, e.message))?;
            if card.other_life(id, view.lifecycle_seq) {
                return Err((false, card.life_note(id, &view.name)));
            }
            core.refuse_delete(id).map_err(|e| (false, e.message))?;
            core.require_box(view).map_err(|e| (true, e.message))
        };
        let mut left_out: Vec<String> = Vec::new();
        // The threads only their box left out: (id, why).
        let mut box_refused: Vec<(String, String)> = Vec::new();
        let mut targets = Vec::new();
        // The boxes' operations, begun with their gate checks and held until the new card is
        // raised: no box is switched while its threads are checked (`Core::reserve_box`).
        let mut _ops = Vec::new();
        {
            let core = hub.core();
            if core.shutting_down {
                return;
            }
            for id in &threads {
                let checked = usable(&core, id).and_then(|()| {
                    let view = core.view(id).map_err(|e| (false, e.message))?;
                    _ops.extend(
                        core.reserve_box(hub, &view.box_name)
                            .map_err(|e| (true, e.message))?,
                    );
                    core.delete_target(hub, id).map_err(|e| (false, e.message))
                });
                match checked {
                    Ok(t) => targets.push(t),
                    Err((true, why)) => box_refused.push((id.clone(), why)),
                    Err((false, why)) => left_out.push(why),
                }
            }
        }
        if !targets.is_empty() {
            Self::check_losses(&mut targets).await;
        }
        let mut core = hub.core();
        if core.shutting_down {
            return;
        }
        // Checked again: the lock was released meanwhile.
        let mut kept = Vec::new();
        for t in targets {
            match usable(&core, &t.id) {
                Ok(()) => kept.push(t),
                Err((true, why)) => box_refused.push((t.id.clone(), why)),
                Err((false, why)) => left_out.push(why),
            }
        }
        if kept.is_empty() {
            tracing::info!(
                card = card_id,
                "no thread of a changed delete card is left to ask for again"
            );
            // The user confirmed deleting those: an item on each says why nothing happened.
            for (id, why) in &box_refused {
                core.raise_delete_failure(
                    hub,
                    id,
                    &format!("card {card_id} was confirmed, but {why}; nothing was deleted"),
                );
            }
            return;
        }
        left_out.extend(box_refused.into_iter().map(|(_, why)| why));
        let (them, rest) = match changed {
            [_] => ("it", "the other thread"),
            _ => ("them", "the other threads"),
        };
        let mut header = format!(
            "Nothing was deleted: since card {card_id} was raised, {}. This card leaves {them} \
             out (ask again to delete {them}) and asks for {rest} of that card.",
            changed.join("; ")
        );
        if !left_out.is_empty() {
            header.push_str(&format!(" Left out as well: {}.", left_out.join("; ")));
        }
        let attn_id = new_id();
        let asked_by = card.asked_by.clone().unwrap_or_else(|| "hub".into());
        match core.raise_delete_card(
            hub,
            attn_id.clone(),
            &mut kept,
            &asked_by,
            Some(&header),
            Some(card_id),
        ) {
            Ok(_) => {
                if card.asked_by_orchestrator() && core.orch_active() {
                    core.orch.push_info(format!(
                        "- delete card {card_id}: the user confirmed it, but {}; nothing was \
                         deleted. Delete card {attn_id} asks for the other threads and waits \
                         for the user.",
                        changed.join("; ")
                    ));
                    if std::mem::take(&mut core.orch.wake) {
                        let _ = hub.notify_tx.send(());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(card = card_id, error = %e.message, "cannot raise a delete card for the threads of a changed one");
            }
        }
    }

    /// Step 2 of [`Hub::run_delete_card`] found more to lose than card `card_id` (`card`)
    /// said (`added`): nothing is deleted, `targets` (stopped) stay as they are, and a new
    /// card for them says why; the orchestrator gets a notice when it asked.
    fn raise_again(
        &self,
        core: &mut Core,
        card_id: &str,
        card: &DeleteCard,
        targets: &mut [Target],
        added: &[String],
        caller: &Caller,
    ) {
        for t in targets.iter() {
            if let Some(rt) = core.rt.get_mut(&t.id) {
                rt.deleting = false;
            }
        }
        let (runtimes, stay) = match targets {
            [_] => ("Its runtime was", "stays"),
            _ => ("Their runtimes were", "stay"),
        };
        let header = format!(
            "Nothing was deleted: since card {card_id} was confirmed, there is more to lose ({}). \
             {runtimes} stopped and {stay} stopped. Answer this card to delete or keep them.",
            added.join("; ")
        );
        let attn_id = new_id();
        // This is now a user-confirmed deletion, even if the original request
        // came from the orchestrator. Reset must not withdraw its reconfirmation.
        let asked_by = caller.source().to_string();
        match core.raise_delete_card(
            &self.inner,
            attn_id.clone(),
            targets,
            &asked_by,
            Some(&header),
            Some(card_id),
        ) {
            Ok(_) => {
                if card.asked_by_orchestrator() && core.orch_active() {
                    core.orch.push_info(format!(
                        "- delete card {card_id}: the user confirmed it, but there was more to \
                         lose by then ({}); nothing was deleted and the threads stay parked. \
                         Delete card {attn_id} waits for the user.",
                        added.join("; ")
                    ));
                    if std::mem::take(&mut core.orch.wake) {
                        let _ = self.inner.notify_tx.send(());
                    }
                }
            }
            Err(e) => {
                tracing::warn!(card = card_id, error = %e.message, "cannot raise a delete card again");
                for t in targets.iter() {
                    let why = format!(
                        "there was more to lose than card {card_id} said ({}), and no new card \
                         could be raised",
                        added.join("; ")
                    );
                    core.raise_delete_failure(&self.inner, &t.id, &why);
                }
            }
        }
    }

    /// Step 1 of a deletion: thread `id` parked (unless it is), and its runtimes over.
    async fn stop_for_delete(&self, id: &str, caller: &Caller) -> Result<(), String> {
        let parked = self.inner.core().view(id).map_err(|e| e.message)?.parked;
        if !parked {
            self.park_thread_as(id, true, true, caller, true)
                .await
                .map_err(|e| format!("stopping its runtime failed: {}", e.message))?;
        }
        self.settle_runtime(id).await
    }

    /// Thread `id`'s deletion failed (`why`): it is not being deleted any more, and an error
    /// item on it says so.
    fn delete_failed(&self, id: &str, why: &str) {
        tracing::warn!(thread = id, error = %why, "deleting a thread failed");
        let mut core = self.inner.core();
        if let Some(rt) = core.rt.get_mut(id) {
            rt.deleting = false;
        }
        core.raise_delete_failure(&self.inner, id, why);
    }

    /// Thread `id`'s worktree and branch are in trash item `item`, but its deletion could not
    /// be recorded (`why`): it stays `deleting` (nothing runs in it, nothing resumes it), and
    /// the hub records the deletion once it can ([`Hub::reconcile_trash`]: its next trash
    /// round, a restart, the box coming online again). An error item says so, if it can be
    /// recorded.
    fn delete_unrecorded(&self, id: &str, item: &str, why: &str) {
        tracing::error!(thread = id, item, error = %why, "a thread is in the trash but its deletion is not recorded");
        let mut core = self.inner.core();
        let Some(name) = core.registry.get(id).map(|v| v.name.clone()) else {
            return;
        };
        let raised = core.append(
            &self.inner,
            id,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary: format!(
                    "Deleting {name}: its worktree and branch are in trash item {item}, but the deletion could not be recorded: {why}. The hub records it as soon as it can."
                ),
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
        );
        if let Err(e) = raised {
            tracing::warn!(thread = id, error = %format!("{e:#}"), "cannot record that a deletion is not recorded");
        }
    }

    /// Steps 3 and 4 of a deletion, for `t` (stopped, checked again): its worktree and branch
    /// moved to the trash, then `thread.deleted`; `Err` says which step failed. The new trash
    /// item is the hub's ([`Core::trash_busy`]) from before it is made until then: nothing
    /// purges or reconciles it meanwhile.
    async fn remove_thread(
        &self,
        t: &Target,
        caller: &Caller,
        held: Option<&Reservation>,
    ) -> Result<(), Removal> {
        let item = new_id();
        self.inner.core().trash_busy.insert(item.clone());
        let r = self.remove_thread_as(t, caller, &item, held).await;
        self.inner.core().trash_busy.remove(&item);
        r
    }

    /// [`Hub::remove_thread`] with the new item's id `item`, holding its repository (`held`,
    /// when it has one) until its box is done with the move.
    async fn remove_thread_as(
        &self,
        t: &Target,
        caller: &Caller,
        item: &str,
        held: Option<&Reservation>,
    ) -> Result<(), Removal> {
        let hub = &self.inner;
        let (id, view) = (&t.id, &t.view);
        let mut deleted = ThreadDeleted {
            name: view.name.clone(),
            ..Default::default()
        };
        if let (Workspace::Worktree { branch, .. }, Some(repo), Some(path)) =
            (&t.workspace, &view.repo, &view.worktree)
        {
            let git = t
                .host
                .as_ref()
                .ok_or_else(|| {
                    Removal::Failed(format!("box {:?} is not available", view.box_name))
                })?
                .git();
            // What goes, for the record: as checked once stopped.
            deleted.dirty_files = t.loss.dirty_files();
            deleted.branch_only_commits = t.loss.branch_only_commits;
            deleted.head_only_commits = t.loss.head_only_commits;
            // A remote box does not know about attaches (their command runs over ssh): checked
            // here once more. None can begin inside the worktree from the confirmation on
            // (`Core::worktree_change_over`); a local box refuses the removal itself.
            if let Some((other, client)) =
                hub.core()
                    .attach_inside(&view.box_name, Path::new(path), id)
            {
                return Err(Removal::Failed(format!(
                    "thread {other} is attached (by {client}) and works in its worktree; end \
                     that attach first"
                )));
            }
            let existing_branch = matches!(
                t.workspace,
                Workspace::Worktree {
                    existing_branch: true,
                    ..
                }
            );
            let losses = t.loss.short(t.pending);
            let trash = git.worktree_trash(TrashRequest {
                item: item.to_string(),
                repo: PathBuf::from(repo),
                worktree: PathBuf::from(path),
                branch: Some(branch.clone()),
                other_branches: Some(t.others.clone()),
                info: TrashInfo {
                    thread: id.clone(),
                    thread_name: view.name.clone(),
                    project: view.project.clone(),
                    box_name: view.box_name.clone(),
                    runtime: view.runtime.clone(),
                    existing_branch,
                    deleted_at_ms: super::trash::deleted_at_now(),
                    losses: losses.clone(),
                },
            });
            let out = match held {
                Some(h) => h.during(trash).await,
                None => trash.await,
            }
            .map_err(|e| {
                Removal::Failed(format!(
                    "moving its worktree and branch to the trash failed: {e:#}"
                ))
            })?;
            deleted.worktree = out.worktree_moved.then(|| path.clone());
            deleted.worktree_left = out.worktree_left.then(|| path.clone());
            deleted.build_output_aside = out.build_output_aside;
            deleted.branch = out.branch_deleted.then(|| branch.clone());
            deleted.trash_dir = out.dir.as_ref().map(|d| d.display().to_string());
            deleted.trash_bytes = out.item.is_some().then_some(out.bytes);
            deleted.losses = out.item.is_some().then_some(losses);
            deleted.trash = out.item;
        }
        let failed = |why: String| match &deleted.trash {
            Some(item) => Removal::Unrecorded {
                item: item.clone(),
                why,
            },
            None => Removal::Failed(why),
        };
        let mut core = hub.core();
        // Nothing can have started since the wait above (the thread is `deleting`): checked
        // under the lock that records the deletion all the same.
        if let Some(what) = core.runtime_outstanding(id) {
            return Err(failed(what.to_string()));
        }
        let source = caller.source();
        let recorded = core.record_deleted(hub, id, deleted.clone(), source);
        recorded.map_err(|e| failed(format!("recording the deletion failed: {e:#}")))
    }

    /// Waits (at most [`ATTACH_QUIESCE_TIMEOUT`]) until nothing of thread `id`'s runtimes is
    /// outstanding ([`Core::runtime_outstanding`]): a parked runtime's exit is recorded before
    /// a kill of what was left of it ([`reap_runtime`]) is over, and that kill's end still
    /// counts on the thread's bookkeeping, which a deletion drops.
    ///
    /// [`reap_runtime`]: super::supervisor::reap_runtime
    async fn settle_runtime(&self, id: &str) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + ATTACH_QUIESCE_TIMEOUT;
        loop {
            let Some(what) = self.inner.core().runtime_outstanding(id) else {
                return Ok(());
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(format!(
                    "{what} after {}s; try again once it is done",
                    ATTACH_QUIESCE_TIMEOUT.as_secs()
                ));
            }
            tokio::time::sleep(SETTLE_POLL).await;
        }
    }
}

impl Core {
    /// The error item on thread `id` saying its deletion failed (`why`), if it is still here.
    fn raise_delete_failure(&mut self, hub: &Inner, id: &str, why: &str) {
        let Some(name) = self.registry.get(id).map(|v| v.name.clone()) else {
            return;
        };
        let raised = self.append(
            hub,
            id,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary: format!(
                    "Deleting {name} failed: {why}. It is still here; ask again to finish \
                     (`twr delete {name}`)."
                ),
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
        );
        if let Err(e) = raised {
            tracing::warn!(thread = id, error = %format!("{e:#}"), "cannot record that deleting a thread failed");
        }
    }

    /// What of thread `id`'s runtimes the hub is still busy with, if anything: a live or
    /// exiting runtime, a kill of what was left of one ([`ThreadRt::reaping`]), a stop not
    /// recorded yet, an attach, park or resume. A deletion records nothing while there is (it
    /// drops the bookkeeping those finish with).
    ///
    /// [`ThreadRt::reaping`]: super::ThreadRt
    pub(super) fn runtime_outstanding(&self, id: &str) -> Option<&'static str> {
        let rt = self.rt.get(id)?;
        if rt.live.is_some() || rt.exiting {
            Some("its runtime is still running")
        } else if rt.reaping > 0 {
            Some("what was left of its runtime is still being killed")
        } else if rt.stop_requested {
            Some("a stop of its runtime is not recorded yet")
        } else if rt.attach.is_some() {
            Some("it is being attached")
        } else if rt.park.is_some() || rt.resuming {
            Some("it is being parked or resumed")
        } else {
            None
        }
    }

    /// Step 4 of a deletion: `thread.deleted` with the resolution of the thread's open items,
    /// of every open delete card whose threads are then all deleted and of its offer card
    /// (`hub::idle_compact`), and the drop of its queue, one batch.
    pub(super) fn record_deleted(
        &mut self,
        hub: &Inner,
        id: &str,
        mut deleted: ThreadDeleted,
        source: Source,
    ) -> anyhow::Result<()> {
        // Its queued prompts, and the prompts held for it or waiting for it (`hub::after`).
        deleted.dropped_prompts = self.store.pending_count(id).unwrap_or(0)
            + self.held_lost(&[id]).get(id).copied().unwrap_or(0);
        let mut batch = Batch {
            drop_queues: vec![id.to_string()],
            ..Batch::default()
        };
        let resolved = |attn_id: &str| {
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: attn_id.to_string(),
                by: ResolvedBy::ThreadStopped,
                answer: None,
                ..Default::default()
            })
        };
        for item in self.attention.for_thread(id, &Default::default()) {
            batch.push(id, None, Source::Hub, resolved(&item.attn_id));
        }
        // A card still naming a thread that is not deleted (in the card's life of it) stays
        // open.
        let mut cards: Vec<String> = self
            .delete_cards
            .iter()
            .filter(|(_, card)| {
                card.threads.iter().all(|t| {
                    t == id
                        || self.registry.is_deleted(t)
                        || self
                            .registry
                            .get(t)
                            .is_some_and(|v| card.other_life(t, v.lifecycle_seq))
                })
            })
            .map(|(a, _)| a.clone())
            .collect();
        cards.sort();
        for attn_id in cards {
            if self.attention.get(&attn_id).is_some() {
                batch.push(HUB_THREAD, None, Source::Hub, resolved(&attn_id));
            }
        }
        // An offer to compact its context before its messages go (`hub::idle_compact`).
        self.stage_idle_offer_close(&mut batch, id);
        batch.push(id, None, source, EventBody::ThreadDeleted(deleted));
        self.append_batch(hub, batch)?;
        Ok(())
    }

    /// The result of `thread.delete` for the open card `item` (`card`), checked again just
    /// now: what it says each thread loses.
    fn existing_card_json(&self, item: &AttentionItem, card: &DeleteCard) -> Value {
        let threads = card
            .threads
            .iter()
            .map(|id| {
                let name = self
                    .registry
                    .get(id)
                    .map_or(id.as_str(), |v| v.name.as_str());
                match card.said.get(id) {
                    Some((loss, pending)) => thread_json(id, name, loss, *pending),
                    None => json!({
                        "thread": id,
                        "thread_name": name,
                        "pending": self.store.pending_count(id).unwrap_or(0)
                            + self.held_lost(&[id]).get(id).copied().unwrap_or(0),
                    }),
                }
            })
            .collect();
        let mut out = card_json(item, threads, true);
        out["message"] = json!(format!(
            "This delete card was open already; checked again just now, there is nothing more \
             to lose than it says. {}",
            str_of(&out["message"])
        ));
        out
    }
}

/// `v` as a string, or empty.
fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_default()
}

/// One thread of a `thread.delete` result: thread `id` named `name`, what it loses and its
/// queued prompts.
fn thread_json(id: &str, name: &str, l: &Loss, pending: u64) -> Value {
    let mut out = json!({
        "thread": id,
        "thread_name": name,
        "pending": pending,
    });
    let inv = l.inventory.as_ref();
    let ignored: Option<Vec<String>> = inv.filter(|i| i.ignored_groups_total > 0).map(|i| {
        i.ignored_groups
            .iter()
            .take(SHOWN)
            .map(ignored_entry)
            .collect()
    });
    let shared: Option<Vec<Value>> = (!l.shared.is_empty()).then(|| {
        l.shared
            .iter()
            .map(|s| json!({ "thread_name": s.name, "commits": s.commits }))
            .collect()
    });
    let unchecked = l.worktree.is_some() && inv.is_none();
    let repositories: Option<Vec<Value>> = inv.filter(|i| !i.repositories.is_empty()).map(|i| {
        i.repositories
            .iter()
            .map(|s| {
                let mut v = json!({ "path": s.path, "commits": s.commits.total });
                if s.nested {
                    v["nested"] = json!(true);
                }
                if let Some(why) = &s.unchecked {
                    v["unchecked"] = json!(why);
                }
                v
            })
            .collect()
    });
    let build_output = inv
        .filter(|i| i.build_output.total > 0)
        .map(|i| &i.build_output.items);
    let unsearched = inv
        .filter(|i| !i.unsearched.is_empty())
        .map(|i| &i.unsearched);
    let linked = inv.filter(|i| !i.linked.is_empty()).map(|i| &i.linked);
    let mounts = inv.filter(|i| !i.mounts.is_empty()).map(|i| &i.mounts);
    let refused = l.refusals(name);
    for (k, v) in [
        ("worktree", json!(l.worktree)),
        ("branch", json!(l.branch)),
        ("dirty_files", json!(l.dirty_files())),
        (
            "hidden_changes",
            json!(inv.map(|i| i.hidden.total).filter(|n| *n > 0)),
        ),
        ("ignored", json!(ignored)),
        (
            "ignored_entries",
            json!(inv.map(|i| i.ignored_groups_total).filter(|n| *n > 0)),
        ),
        ("build_output", json!(build_output)),
        (
            "build_output_bytes",
            json!(inv.map(|i| i.build_output_bytes).filter(|n| *n > 0)),
        ),
        ("repositories", json!(repositories)),
        ("unsearched", json!(unsearched)),
        ("linked", json!(linked)),
        ("mounts", json!(mounts)),
        (
            "mounts_unknown",
            json!(inv.filter(|i| i.mounts_unknown).map(|_| true)),
        ),
        ("refused", json!((!refused.is_empty()).then_some(refused))),
        ("head_only_commits", json!(l.head_only_commits)),
        ("branch_only_commits", json!(l.branch_only_commits)),
        ("checkout", json!(l.checkout)),
        ("checkout_only_commits", json!(l.checkout_only_commits)),
        ("shared", json!(shared)),
        ("existing_branch", json!(l.existing_branch.then_some(true))),
        (
            "worktree_missing",
            json!(l.worktree_missing.then_some(true)),
        ),
        ("worktree_unchecked", json!(unchecked.then_some(true))),
    ] {
        if !v.is_null() {
            out[k] = v;
        }
    }
    out
}

/// The result of `thread.delete` for card `card` of threads `threads` (each [`thread_json`]
/// or an existing card's).
fn card_json(card: &AttentionItem, threads: Vec<Value>, existing: bool) -> Value {
    let first = threads.first().cloned().unwrap_or(Value::Null);
    json!({
        "status": "needs_confirmation",
        "threads": threads,
        "thread": first["thread"],
        "thread_name": first["thread_name"],
        "attn_id": card.attn_id,
        "revision": card.revision,
        "summary": card.summary,
        "options": card.options,
        "existing": existing,
        "message": format!(
            "Not deleted yet: only the user can confirm it ({DELETE} on the card, or `twr \
             respond {}@{} {DELETE}`). Tell the user it is waiting for them.",
            card.attn_id, card.revision
        ),
    })
}

/// Why a thread's removal ([`Hub::remove_thread`]) did not end with `thread.deleted`.
enum Removal {
    /// Nothing of it went: it is still here, to be deleted again.
    Failed(String),
    /// Its worktree and branch are in trash item `item`, but `thread.deleted` could not be
    /// recorded (`why`).
    Unrecorded { item: String, why: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::git::LinkedWorktree;

    /// What a worktree holds: `dirty` uncommitted files, nothing else.
    fn inv(dirty: u64) -> Option<WorktreeInventory> {
        Some(WorktreeInventory {
            dirty: Listed::new((0..dirty).map(|i| format!("f{i}"))),
            ..Default::default()
        })
    }

    /// A worktree the thread switched to another branch: the card says that branch stays,
    /// with its commits on no other branch when there are, and a card confirmed after the
    /// worktree moved to yet another branch has more to say.
    #[test]
    fn the_card_names_the_branch_a_switched_worktree_is_on() {
        let loss = Loss {
            worktree: Some("/w/api".into()),
            inventory: inv(1),
            branch: Some("twr/api".into()),
            branch_only_commits: Some(0),
            checkout: Some("twr/api-final".into()),
            checkout_only_commits: Some(3),
            commits: Some(Listed::default()),
            ..Default::default()
        };
        assert_eq!(
            loss.summary("api", None, 0),
            "Delete thread api? This stops its runtime, moves its worktree /w/api to the trash \
             with 1 uncommitted file it has, moves its branch twr/api to the trash (each of its \
             commits is on another branch, tag, remote, stash or reflog), and leaves the branch \
             twr/api-final its worktree is on in the repository, with 3 commits on no other \
             branch, tag or remote (not pushed)."
        );
        // Nothing of it is lost: the short form says nothing about it.
        assert_eq!(loss.short(0), "1 uncommitted file");
        let merged = Loss {
            checkout_only_commits: Some(0),
            ..loss.clone()
        };
        assert!(merged.summary("api", None, 0).ends_with(
            "leaves the branch twr/api-final its worktree is on in the repository (each of its \
             commits is on another branch, tag, remote, stash or reflog)."
        ));
        let uncounted = Loss {
            checkout_only_commits: None,
            ..loss.clone()
        };
        assert!(uncounted.summary("api", None, 0).ends_with(
            "and leaves the branch twr/api-final its worktree is on in the repository."
        ));
        assert!(merged.added_since(&loss, 0, 0).is_empty());
        let moved = Loss {
            checkout: Some("twr/api-next".into()),
            ..loss.clone()
        };
        assert_eq!(
            moved.added_since(&loss, 0, 0),
            ["its worktree is on another branch"]
        );
        let unswitched = Loss {
            checkout: None,
            checkout_only_commits: None,
            ..loss.clone()
        };
        assert_eq!(
            moved.added_since(&unswitched, 0, 0),
            ["its worktree is on another branch"]
        );
        assert!(unswitched.added_since(&loss, 0, 0).is_empty());
    }

    #[test]
    fn the_card_says_what_goes() {
        let loss = Loss {
            worktree: Some("/w/api".into()),
            inventory: inv(3),
            branch: Some("twr/api".into()),
            branch_only_commits: Some(1),
            ..Default::default()
        };
        assert_eq!(
            loss.summary("api", Some("twr"), 2),
            "Delete thread api (project twr)? This stops its runtime, moves its worktree /w/api \
             to the trash with 3 uncommitted files it has, moves its branch twr/api to the \
             trash, with 1 commit on no other branch, tag or remote (not pushed), and drops 2 \
             queued prompts."
        );
        assert_eq!(
            loss.short(2),
            "3 uncommitted files, 2 queued prompts (dropped)"
        );
        let clean = Loss {
            worktree: Some("/w/api".into()),
            inventory: inv(0),
            branch: Some("twr/api".into()),
            branch_only_commits: Some(0),
            ..Default::default()
        };
        let s = clean.summary("api", None, 0);
        assert!(s.contains("(no uncommitted changes)"), "{s}");
        assert!(
            s.contains("each of its commits is on another branch"),
            "{s}"
        );
        // Commits made on a detached HEAD, and a branch from before the thread.
        let detached = Loss {
            worktree: Some("/w/api".into()),
            inventory: inv(0),
            head_only_commits: Some(2),
            branch: Some("twr/api".into()),
            existing_branch: true,
            branch_only_commits: Some(1),
            ..Default::default()
        };
        assert_eq!(
            detached.summary("api", None, 0),
            "Delete thread api? This stops its runtime, moves its worktree /w/api to the trash \
             (no uncommitted changes), moves 2 commits made on a detached HEAD in its worktree \
             to the trash (on no branch, tag or remote; not pushed), and moves its branch \
             twr/api, which existed before the thread, to the trash, with 1 commit on no other \
             branch, tag or remote (not pushed)."
        );
        let existing = Loss {
            branch_only_commits: Some(0),
            head_only_commits: Some(0),
            ..detached
        };
        let s = existing.summary("api", None, 0);
        assert!(!s.contains("detached"), "{s}");
        assert!(
            s.contains(
                "moves its branch twr/api, which existed before the thread, to the trash (each \
                 of its commits is on another branch, tag, remote, stash or reflog)"
            ),
            "{s}"
        );
        let repo = Loss {
            directory: Some("/r".into()),
            unknown: vec!["x (boom)".into()],
            ..Default::default()
        };
        assert_eq!(
            repo.summary("api", None, 1),
            "Delete thread api? This stops its runtime and drops 1 queued prompt. It cannot \
             be undone. It works in /r, which stays as it is. (Not checked: x (boom).)"
        );
    }

    /// The repositories that go: their lost commits, or what of them could not be checked, and
    /// the directories not searched for them; a new one, or a new commit of one, is more to
    /// lose than the card said.
    #[test]
    fn the_card_names_repositories_in_the_worktree() {
        let repo = |path: &str, nested: bool, commits: u64, unchecked: Option<&str>| RepoLoss {
            path: path.into(),
            nested,
            commits: Listed::new((0..commits).map(|i| format!("c{i}"))),
            unchecked: unchecked.map(str::to_string),
        };
        let with = |repos: Vec<RepoLoss>, unsearched: &[&str]| Loss {
            worktree: Some("/w/api".into()),
            inventory: Some(WorktreeInventory {
                repositories: repos,
                unsearched: unsearched.iter().map(|u| u.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let card = with(
            vec![
                repo("libs/x", false, 2, None),
                repo("tools/t", true, 1, None),
                repo(
                    "/r/.git/worktrees/api/modules/y",
                    false,
                    0,
                    Some("its commits (boom)"),
                ),
            ],
            &["big/"],
        );
        assert_eq!(
            card.summary("api", None, 0),
            "Delete thread api? This stops its runtime, moves its worktree /w/api to the trash \
             (no uncommitted changes), with it the repository of its submodule libs/x with 2 \
             commits on none of its remote-tracking branches (not pushed), with it the git \
             repository nested at tools/t with 1 commit on none of its remote-tracking \
             branches (not pushed), with it the repository of its submodule \
             /r/.git/worktrees/api/modules/y (not checked: its commits (boom)), and could not \
             search big/ to the end for git repositories (too many files, or unreadable: \
             whatever repository is there goes unchecked)."
        );
        assert!(card.added_since(&card, 0, 0).is_empty());
        for more in [
            with(vec![repo("libs/x", false, 3, None)], &["big/"]),
            with(vec![repo("libs/z", true, 1, None)], &[]),
            with(vec![], &["big/", "vendor/"]),
        ] {
            assert_eq!(
                more.added_since(&card, 0, 0),
                ["commits of repositories in the worktree"]
            );
        }
        let json = thread_json("t1", "api", &card, 0);
        assert_eq!(
            json["repositories"],
            json!([
                {"path": "libs/x", "commits": 2},
                {"path": "tools/t", "nested": true, "commits": 1},
                {"path": "/r/.git/worktrees/api/modules/y", "commits": 0,
                 "unchecked": "its commits (boom)"},
            ])
        );
        assert_eq!(json["unsearched"], json!(["big/"]));
    }

    /// A linked worktree in the worktree whose record stays behind is on the card
    /// as why the thread cannot be deleted now, with what to do (in the JSON too).
    #[test]
    fn the_card_says_a_linked_worktree_refuses_the_delete() {
        let linked = LinkedWorktree {
            path: "/w/api/tools/side".into(),
            record: "/o/.git/worktrees/side".into(),
            repo: "/o".into(),
            same_repository: false,
        };
        let card = Loss {
            worktree: Some("/w/api".into()),
            inventory: Some(WorktreeInventory {
                linked: vec![linked.clone()],
                ..Default::default()
            }),
            ..Default::default()
        };
        let refusal = "api's worktree contains a linked worktree of another repository at \
                       /w/api/tools/side (its record is /o/.git/worktrees/side): move or remove \
                       it first (`git -C /o worktree move /w/api/tools/side <elsewhere>`, or \
                       `git -C /o worktree remove /w/api/tools/side`)";
        assert_eq!(
            card.summary("api", None, 0),
            format!(
                "Delete thread api? This stops its runtime and moves its worktree /w/api to the \
                 trash (no uncommitted changes). It cannot be deleted now: {refusal}."
            )
        );
        assert!(card
            .line("api", None, 0)
            .ends_with(&format!("; it cannot be deleted now: {refusal}")));
        assert_eq!(card.refusals("api"), [refusal]);
        let json = thread_json("t1", "api", &card, 0);
        assert_eq!(json["refused"], json!([refusal]));
        assert_eq!(
            json["linked"],
            json!([{"path": "/w/api/tools/side", "record": "/o/.git/worktrees/side",
                    "repo": "/o"}])
        );
        let own = LinkedWorktree {
            same_repository: true,
            ..linked
        };
        assert!(own
            .refusal("api's worktree")
            .contains("a linked worktree of its own repository at"));
        assert!(Loss::default().refusals("api").is_empty());
    }

    /// A file system mounted in the worktree, or mount points that cannot be ruled out, is on
    /// the card as what refuses the delete, before it is confirmed.
    #[test]
    fn the_card_says_a_mount_point_refuses_the_delete() {
        let card = |mounts: Vec<String>, unknown: bool| Loss {
            worktree: Some("/w/api".into()),
            inventory: Some(WorktreeInventory {
                mounts,
                mounts_unknown: unknown,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mounted = card(vec!["target/shared".into(), "vendor".into()], false);
        let refusal = "api's worktree has a file system mounted in it at target/shared, vendor: \
                       unmount it first";
        assert!(mounted
            .summary("api", None, 0)
            .ends_with(&format!(" It cannot be deleted now: {refusal}.")));
        assert_eq!(mounted.refusals("api"), [refusal]);
        let json = thread_json("t1", "api", &mounted, 0);
        assert_eq!(json["mounts"], json!(["target/shared", "vendor"]));
        assert_eq!(json["refused"], json!([refusal]));
        let unknown = card(Vec::new(), true);
        assert_eq!(
            unknown.refusals("api"),
            [
                "where file systems are mounted cannot be read (/proc/self/mountinfo on Linux, \
                 getfsstat on macOS), so mount points in api's worktree cannot be ruled out"
            ]
        );
        assert_eq!(
            thread_json("t1", "api", &unknown, 0)["mounts_unknown"],
            true
        );
    }

    /// A worktree whose directory is gone: the repositories its record holds are on the card,
    /// and its files count as none (a directory back with files is more to lose, also after
    /// a card that could not look at the record).
    #[test]
    fn a_gone_worktree_names_the_repositories_of_its_record() {
        let repo = RepoLoss {
            path: "/r/.git/worktrees/api/modules/x".into(),
            commits: Listed::new(["c1".to_string()]),
            ..Default::default()
        };
        let gone = Loss {
            worktree: Some("/w/api".into()),
            worktree_missing: true,
            inventory: Some(WorktreeInventory {
                repositories: vec![repo],
                gone: true,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            gone.summary("api", None, 0),
            "Delete thread api? This stops its runtime, moves the record of its worktree /w/api \
             (already gone) to the trash, and with it the repository of its submodule \
             /r/.git/worktrees/api/modules/x with 1 commit on none of its remote-tracking \
             branches (not pushed)."
        );
        assert_eq!(gone.dirty_files(), None);
        assert!(gone.added_since(&gone, 0, 0).is_empty());
        let back = Loss {
            worktree_missing: false,
            inventory: inv(1),
            ..gone.clone()
        };
        let unread = Loss {
            inventory: None,
            ..gone.clone()
        };
        for card in [&gone, &unread] {
            assert_eq!(back.added_since(card, 0, 0), ["uncommitted files"]);
        }
        // What the card could not check, it did not say: the user confirmed it as such.
        assert!(gone.added_since(&unread, 0, 0).is_empty());
        assert!(unread.added_since(&unread, 0, 0).is_empty());
        assert_eq!(
            unread.added_since(&gone, 0, 0),
            ["its worktree cannot be inspected now"]
        );
    }

    /// A card for several threads: a header, then one line per thread with the same parts.
    #[test]
    fn a_card_for_several_threads_has_a_line_each() {
        let view = |name: &str, project: Option<&str>| ThreadView {
            name: name.into(),
            project: project.map(str::to_string),
            ..Default::default()
        };
        let targets = [
            Target {
                id: "t1".into(),
                view: view("api", Some("twr")),
                loss: Loss {
                    worktree: Some("/w/api".into()),
                    inventory: inv(2),
                    branch: Some("twr/api".into()),
                    branch_only_commits: Some(0),
                    ..Default::default()
                },
                pending: 1,
                ..Default::default()
            },
            Target {
                id: "t2".into(),
                view: view("old", Some("twr")),
                loss: Loss {
                    worktree: Some("/w/old".into()),
                    worktree_missing: true,
                    branch: Some("twr/old".into()),
                    branch_only_commits: Some(3),
                    ..Default::default()
                },
                pending: 0,
                ..Default::default()
            },
            Target {
                id: "t3".into(),
                view: view("notes", None),
                loss: Loss {
                    directory: Some("/n".into()),
                    unknown: vec!["x (boom)".into()],
                    ..Default::default()
                },
                pending: 0,
                ..Default::default()
            },
        ];
        assert_eq!(
            card_summary(&targets, None, 7),
            "Delete 3 threads?\n\
             - api (project twr): stops its runtime, moves its worktree /w/api to the trash \
             with 2 uncommitted files it has, moves its branch twr/api to the trash (each of \
             its commits is on another branch, tag, remote, stash or reflog), drops 1 queued \
             prompt\n\
             - old (project twr): stops its runtime, moves the record of its worktree /w/old \
             (already gone) to the trash, moves its branch twr/old to the trash, with 3 commits \
             on no other branch, tag or remote (not pushed)\n\
             - notes: stops its runtime; it works in /n, which stays as it is (cannot be undone) \
             (not checked: x (boom))\n\
             The trash keeps them for 7 days, then purges them: `twr trash restore` brings a \
             thread back, `twr trash empty` purges sooner."
        );
        let one = card_summary(&targets[..1], None, 1);
        assert!(
            one.starts_with("Delete thread api (project twr)? "),
            "{one}"
        );
        assert!(one.ends_with("\nThe trash keeps it for 1 day, then purges it: `twr trash restore` brings a thread back, `twr trash empty` purges sooner."), "{one}");
        // A big worktree: its size, and how to free it sooner.
        let mut big = targets[0].clone_for_test();
        big.loss.inventory.as_mut().unwrap().bytes = 3 << 30;
        let s = card_summary(&[big], None, 7);
        assert!(
            s.contains("moves its worktree /w/api (3.0 GB) to the trash"),
            "{s}"
        );
        assert!(
            s.ends_with("That holds 3.0 GB on disk until then (`twr trash empty` frees it)."),
            "{s}"
        );
    }

    /// Reservations: one repository of a box at a time, every one of it for a deletion that
    /// does not know its repository; other repositories and other boxes do not wait. A release
    /// is seen by a deletion that checked before it.
    #[tokio::test]
    async fn deletions_hold_their_repositories() {
        let r = Arc::new(Reservations::default());
        let key = |b: &str, c: Option<&str>| (b.to_string(), c.map(PathBuf::from));
        let keys = |k: &[RepoKey]| k.iter().cloned().collect::<BTreeSet<_>>();
        let before = r.releases();
        let one = r.reserve(keys(&[key("b1", Some("/r/.git"))])).await;
        assert!(one.covers(&keys(&[key("b1", Some("/r/.git"))])));
        assert!(!one.covers(&keys(&[key("b1", None)])));
        assert!(one.none_released_since(&before));
        // Another repository of the box, and the same one on another box: no wait.
        let other = r
            .reserve(keys(&[
                key("b1", Some("/s/.git")),
                key("b2", Some("/r/.git")),
            ]))
            .await;
        drop(other);
        assert!(!one.none_released_since(&before), "b1 had a release");
        let wait = Duration::from_millis(50);
        for blocked in [key("b1", Some("/r/.git")), key("b1", None)] {
            assert!(
                tokio::time::timeout(wait, r.reserve(keys(std::slice::from_ref(&blocked))))
                    .await
                    .is_err(),
                "{blocked:?} is held"
            );
        }
        let whole = {
            let r = &r;
            let waiting = async move { r.reserve(keys(&[key("b1", None)])).await };
            let (whole, ()) = tokio::join!(waiting, async move { drop(one) });
            whole
        };
        assert!(
            tokio::time::timeout(wait, r.reserve(keys(&[key("b1", Some("/t/.git"))])))
                .await
                .is_err(),
            "the whole box is held"
        );
        drop(whole);
        r.reserve(keys(&[key("b1", Some("/t/.git"))])).await;
    }

    /// A card names its threads in order; one from before several threads names one.
    #[test]
    fn a_card_names_its_threads() {
        let raised = |v: Value| AttentionRaised {
            ext: twr_ext_map(v),
            ..Default::default()
        };
        let p = raised(json!({DELETE_KEY: {"threads": [
            {"thread": "t2", "name": "b"}, {"thread": "t1", "name": "a"}], "asked_by": "user"}}));
        assert_eq!(card_threads(&p), ["t2", "t1"]);
        let p = raised(json!({DELETE_KEY: {"thread": "t1", "name": "a"}}));
        assert_eq!(card_threads(&p), ["t1"]);
        assert!(card_threads(&raised(json!({"proposal": {}}))).is_empty());
    }
}

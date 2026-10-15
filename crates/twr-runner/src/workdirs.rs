//! Working-directory reservations of one box: what keeps a worktree change (`git worktree
//! remove` / `add`) and a runtime start in the same directory from overlapping. The box's
//! [`crate::local::LocalGit`] owns the set; the runtimes of the box (a runner's
//! [`crate::LocalBox`], the hub's own box) take their uses from it.
//!
//! - A runtime holds a [`WorkdirUse`] of its working directory for as long as it runs. A use
//!   cannot begin while a change of that directory (or of one containing it) is in progress:
//!   the start fails with an error that says so.
//! - A worktree change holds a [`WorkdirChange`] of its path from before its safety checks
//!   until its work is over, whoever still waits for it: the whole operation runs as a task
//!   of its own that its caller cannot stop ([`crate::local`]), and the guard of a
//!   removal or creation moves into the task that supervises its git, which drops it only once
//!   git's whole process group is gone (killed past its timeout, and reaped).
//!   A removal or a trim is refused while a use is inside its path;
//!   a change waits (bounded) for an overlapping change to finish.
//! - [`Workdirs::settled`] waits (bounded) until no change overlapping a path is in progress,
//!   so asking whether a worktree exists sees the outcome of a removal still running.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::Notify;
use twr_core::paths::resolve_lenient;

/// How long a worktree change, or a question about a worktree, waits for an overlapping
/// change in progress before it gives up (less than the hub's request timeout, so the hub
/// hears why).
pub const CHANGE_WAIT: Duration = Duration::from_secs(30);

/// What a [`WorkdirChange`] does to its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// `git worktree add`.
    Create,
    /// `git worktree remove`.
    Remove,
    /// `twr trim`: its build output deleted. Like a removal, refused while a runtime works in
    /// it (the hub stops an idle one first), and none starts until the trim is over.
    Trim,
}

impl ChangeKind {
    fn verb(self) -> &'static str {
        match self {
            ChangeKind::Create => "created",
            ChangeKind::Remove => "removed",
            ChangeKind::Trim => "trimmed",
        }
    }
}

/// Why a change cannot begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocked {
    /// A runtime (of thread `holder`) works in the path (removals and trims only).
    InUse {
        /// The thread whose runtime works there.
        holder: String,
    },
    /// Another change of an overlapping path did not finish in time.
    Busy(String),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocked::InUse { holder } => {
                write!(
                    f,
                    "a runtime of thread {holder} still runs in it on this box"
                )
            }
            Blocked::Busy(msg) => f.write_str(msg),
        }
    }
}

/// A path as given and resolved with the filesystem's semantics as far as it exists
/// ([`resolve_lenient`]: symlinks followed before a later `..` applies).
#[derive(Debug, Clone)]
struct Place {
    raw: PathBuf,
    /// `None`: which directory it is cannot be established (then it overlaps everything).
    canon: Option<PathBuf>,
}

impl Place {
    fn of(p: &Path) -> Place {
        Place {
            raw: p.to_path_buf(),
            canon: resolve_lenient(p),
        }
    }

    /// Whether this is `other` or inside it. Fails closed: a place whose directory cannot be
    /// established is within anything (a removal keeps it, a start waits for changes).
    fn within(&self, other: &Place) -> bool {
        if self.raw.starts_with(&other.raw) {
            return true;
        }
        match (&self.canon, &other.canon) {
            (Some(a), Some(b)) => a.starts_with(b),
            _ => true,
        }
    }

    fn overlaps(&self, other: &Place) -> bool {
        self.within(other) || other.within(self)
    }
}

#[derive(Default)]
struct State {
    next: u64,
    /// Runtimes' working directories, with their threads.
    uses: HashMap<u64, (Place, String)>,
    /// Worktree changes in progress.
    changes: HashMap<u64, (Place, ChangeKind)>,
}

/// The working-directory reservations of one box (see the module docs).
#[derive(Default)]
pub struct Workdirs {
    state: Mutex<State>,
    /// Woken whenever a change ends.
    changed: Notify,
}

impl std::fmt::Debug for Workdirs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = self.lock();
        f.debug_struct("Workdirs")
            .field("uses", &st.uses.len())
            .field("changes", &st.changes.len())
            .finish()
    }
}

impl Workdirs {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A runtime of thread `holder` starts in `dir`: its use, held until dropped (`None` for
    /// no directory). Fails while a change of `dir`, or of a directory containing it, is in
    /// progress.
    pub fn begin_use(
        self: &Arc<Self>,
        dir: &Path,
        holder: &str,
    ) -> Result<Option<WorkdirUse>, String> {
        if dir.as_os_str().is_empty() {
            return Ok(None);
        }
        // (Resolved outside the lock.)
        let place = Place::of(dir);
        let mut st = self.lock();
        if let Some((p, kind)) = st.changes.values().find(|(p, _)| place.within(p)) {
            return Err(format!(
                "cannot start a runtime in {}: the worktree {} is being {} on this box; try \
                 again once that has finished",
                dir.display(),
                p.raw.display(),
                kind.verb()
            ));
        }
        st.next += 1;
        let id = st.next;
        st.uses.insert(id, (place, holder.to_string()));
        Ok(Some(WorkdirUse {
            set: Arc::clone(self),
            id,
        }))
    }

    /// A worktree change of `path` begins: its reservation, held until dropped. A removal is
    /// refused while a runtime works in `path` (or inside it). Waits up to `wait` for another
    /// change of an overlapping path to finish.
    pub async fn begin_change(
        self: &Arc<Self>,
        path: &Path,
        kind: ChangeKind,
        wait: Duration,
    ) -> Result<WorkdirChange, Blocked> {
        let place = Place::of(path);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let ended = self.changed.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            let (other, other_kind) = {
                let mut st = self.lock();
                if matches!(kind, ChangeKind::Remove | ChangeKind::Trim) {
                    if let Some((_, holder)) = st.uses.values().find(|(u, _)| u.within(&place)) {
                        return Err(Blocked::InUse {
                            holder: holder.clone(),
                        });
                    }
                }
                let busy = st
                    .changes
                    .values()
                    .find(|(p, _)| p.overlaps(&place))
                    .map(|(p, k)| (p.raw.clone(), *k));
                match busy {
                    None => {
                        st.next += 1;
                        let id = st.next;
                        st.changes.insert(id, (place, kind));
                        return Ok(WorkdirChange {
                            set: Arc::clone(self),
                            id,
                        });
                    }
                    Some(b) => b,
                }
            };
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                return Err(Blocked::Busy(format!(
                    "the worktree {} is still being {} on this box (waited {}s); try again \
                     once that has finished",
                    other.display(),
                    other_kind.verb(),
                    wait.as_secs()
                )));
            }
        }
    }

    /// Waits (up to `wait`) until no change of a path overlapping `path` is in progress.
    pub async fn settled(&self, path: &Path, wait: Duration) -> Result<(), String> {
        let place = Place::of(path);
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let ended = self.changed.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            let busy = self
                .lock()
                .changes
                .values()
                .find(|(p, _)| p.overlaps(&place))
                .map(|(p, k)| (p.raw.clone(), *k));
            let Some((other, kind)) = busy else {
                return Ok(());
            };
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                return Err(format!(
                    "the worktree {} is still being {} on this box (waited {}s); try again \
                     once that has finished",
                    other.display(),
                    kind.verb(),
                    wait.as_secs()
                ));
            }
        }
    }
}

/// A runtime's use of its working directory ([`Workdirs::begin_use`]); ends when dropped.
#[derive(Debug)]
pub struct WorkdirUse {
    set: Arc<Workdirs>,
    id: u64,
}

impl Drop for WorkdirUse {
    fn drop(&mut self) {
        self.set.lock().uses.remove(&self.id);
    }
}

/// A worktree change in progress ([`Workdirs::begin_change`]); ends when dropped.
#[derive(Debug)]
pub struct WorkdirChange {
    set: Arc<Workdirs>,
    id: u64,
}

impl Drop for WorkdirChange {
    fn drop(&mut self) {
        self.set.lock().changes.remove(&self.id);
        self.set.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A place whose directory cannot be established overlaps everything (fail closed): a
    /// removal keeps it, a start waits for changes.
    #[tokio::test]
    async fn unresolvable_places_overlap_everything() {
        let dir = tempfile::tempdir().unwrap();
        let set = Arc::new(Workdirs::default());
        // `..` after a missing component: what it names depends on what `missing` becomes.
        let unknown = dir.path().join("missing").join("..").join("x");
        let u = set.begin_use(&unknown, "t1").unwrap();
        let e = set
            .begin_change(&dir.path().join("wt"), ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            Blocked::InUse {
                holder: "t1".into()
            }
        );
        drop(u);
        let c = set
            .begin_change(&dir.path().join("wt"), ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap();
        assert!(set.begin_use(&unknown, "t1").is_err());
        drop(c);
    }

    #[tokio::test]
    async fn uses_and_changes_exclude_each_other() {
        let set = Arc::new(Workdirs::default());
        let wt = Path::new("/nonexistent-twr/worktrees/p/t1");
        // A runtime in the worktree (or below it) keeps a removal out; not a creation.
        let u = set.begin_use(&wt.join("sub"), "t1").unwrap();
        let e = set
            .begin_change(wt, ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(
            e,
            Blocked::InUse {
                holder: "t1".into()
            }
        );
        drop(u);
        // A removal in progress keeps a start out, and waits out an overlapping change.
        let c = set
            .begin_change(wt, ChangeKind::Remove, Duration::ZERO)
            .await
            .unwrap();
        let e = set.begin_use(wt, "t1").unwrap_err();
        assert!(e.contains("being removed"), "{e}");
        // Elsewhere is fine.
        assert!(set
            .begin_use(Path::new("/nonexistent-twr/other"), "t2")
            .unwrap()
            .is_some());
        assert!(matches!(
            set.begin_change(wt, ChangeKind::Create, Duration::from_millis(20))
                .await,
            Err(Blocked::Busy(_))
        ));
        assert!(set
            .settled(wt, Duration::from_millis(20))
            .await
            .unwrap_err()
            .contains("still being removed"));
        let waiting = {
            let set = Arc::clone(&set);
            tokio::spawn(async move {
                set.begin_change(wt, ChangeKind::Create, Duration::from_secs(60))
                    .await
                    .map(drop)
            })
        };
        tokio::task::yield_now().await;
        drop(c);
        waiting.await.unwrap().unwrap();
        set.settled(wt, Duration::ZERO).await.unwrap();
        assert!(set.begin_use(wt, "t1").unwrap().is_some());
        assert!(set.begin_use(Path::new(""), "t1").unwrap().is_none());
    }
}

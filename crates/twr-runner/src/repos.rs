//! A box's side of `twr box project`, which the CLI runs over ssh with the box's twr build:
//! finding a project's repository on the box (`twr runner --find-repo`, [`find_repo`]) and
//! cloning one there in the user's terminal (`twr runner --clone-repo`, [`clone_repo`]).
//!
//! **Finding.** The candidates are the hint (the path the CLI expects, from the hub's
//! repository's place under its home) when it is in a git repository, then the git
//! repositories named `--name` under the default project roots in the box's home
//! ([`DEFAULT_PROJECT_ROOTS`], by the hub's `project_roots` rules:
//! [`twr_core::project_lookup`]). Each is canonical, says whether it is its repository's top
//! level (`git rev-parse --show-toplevel` is the path itself; only such a candidate can be
//! recorded) and carries its `origin` as git resolves it, [`redact`]ed, and whether that names
//! the hub repository's `origin` ([`RemoteUrl::identity`]). Ordered: those whose origin
//! matches first, then the hint, then the rest. Every git command runs with twr's no-network
//! git settings ([`LocalGit`]). The report is [`FindReport::complete`] only when the box has
//! an absolute home to search, git runs there ([`FindReport::git_missing`]), no limit (the
//! lookup's, [`MAX_CANDIDATES`], the report's size, [`REPORT_MAX_BYTES`], the search's
//! deadline, [`FIND_DEADLINE`]: [`FindReport::timed_out`]) cut the search short, and git read
//! every candidate (an `origin` it could not read is not "none"; a folder found by its name
//! with a `.git` in it that git does not take for a repository, a broken `.git` file say, is
//! not "no repository"): the CLI records a candidate by itself only from a complete report. A
//! git command that fails is reported and logged without git's message, which may quote the
//! config (a value holding a URL with a password, say).
//!
//! **Cloning.** Only a URL [`RemoteUrl::parse`] accepts (`https://`, `ssh://`, scp-like; no
//! credentials) into `<parent>/<name>`: `name` one plain folder name
//! ([`is_plain_name`]); `parent` an existing directory, not a symbolic link, this user's, that
//! neither its group nor other users can write to, on a path that no other user can change
//! but through a directory on the way that its group may write to ([`check_ancestors`], the
//! builds tree's rules: such a directory is allowed, and logged; the target is then reached
//! through the parent held open, not through that path). The target is made with an
//! exclusive `mkdir`, mode 0700 (an existing one is refused: never reused), in the parent as
//! this process holds it open, then `git clone -- <url> <name>` runs in that parent (changed
//! into by its descriptor, not its path) with the user's own git
//! config (their credential helpers), the session's `GIT_*` variables removed,
//! `GIT_ALLOW_PROTOCOL=https:ssh` (which git applies to every URL it reaches, a
//! `url.<base>.insteadOf` rewrite of the config included, over any `protocol.<name>.allow`, so
//! a rewrite to `ext::` or `file://` is refused by git and runs nothing), `-c
//! protocol.allow=never -c protocol.https.allow=always -c protocol.ssh.allow=always`, and
//! `GIT_LFS_SKIP_SMUDGE=1` (Git LFS, when installed, fetches from an endpoint the repository's
//! own `.lfsconfig` may choose, outside that allowlist: its files stay pointers until the user
//! runs `git lfs pull` there), its stdin, stdout and stderr this process's (the user's
//! terminal: git asks for what it needs there). git runs in a process group of its own, made
//! the terminal's foreground group ([`run_group`]): the terminal's signals (Ctrl-C) go to it
//! and the helpers it starts, not to this process, which takes the terminal back afterwards.
//! The signal handlers are in place before the target is made. A SIGTERM to this process, or a
//! hangup (SIGHUP: the ssh connection dropped, the terminal closed; forwarded to git's group
//! first), stops the clone: SIGTERM to git's whole group, [`STOP_GRACE`] for it to end, then
//! SIGKILL to what is left, and every member waited for. A clone that failed or was stopped
//! then removes the directory it made, whatever git left in it (only this user can write in
//! it), while it is still that directory (the same device and inode, a directory of this
//! user's, mode 0700), reached through the parent held open and removed through descriptors
//! alone (each directory listed and emptied as it is held open, [`Dir::listed`], on macOS as on
//! Linux), without following a symbolic link in it or leaving its file system; a directory that
//! replaced it is left as it is. A clone that succeeded
//! must still be that directory and its repository's top level (git, run in it through the
//! parent held open, says so), in the parent held open, which must still be the directory at
//! the parent's path (one moved meanwhile keeps the clone, left there and not recorded); its
//! canonical path is the result. twr never fetches afterwards.

use std::ffi::OsStr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::{Deserialize, Serialize};
use twr_core::config::{expand_home, DEFAULT_PROJECT_ROOTS};
use twr_core::git::GitOps;
use twr_core::gitremote::{redact, RemoteUrl};
use twr_core::paths::{check_ancestors, effective_uid};
use twr_core::project_lookup::{self, is_plain_name};

use crate::local::dirfd::{self, Dir};
use crate::local::{LocalGit, SAFE_CONFIG};

/// Most candidates one search reports (and reads an `origin` of); a search that found more is
/// not [`FindReport::complete`].
pub const MAX_CANDIDATES: usize = 32;

/// The most bytes `--find-repo`'s report (its JSON) takes: half of what the CLI reads of the
/// ssh that runs it ([`twr_core::runner::SSH_OUTPUT_MAX`]), the rest left to what a login
/// script prints there. Candidates that do not fit are left out, the last first and the hint
/// last of all, and the report is not [`FindReport::complete`] then; a path is never cut.
pub const REPORT_MAX_BYTES: usize = twr_core::runner::SSH_OUTPUT_MAX / 2;

const _: () = assert!(REPORT_MAX_BYTES < twr_core::runner::SSH_OUTPUT_MAX);

/// How long `--find-repo` searches at most: past it, it stops (the git it runs killed) and
/// reports what it found, not [`FindReport::complete`] ([`FindReport::timed_out`]). Below the
/// CLI's own wait for the ssh that runs it, so that the box side ends first, and says so.
pub const FIND_DEADLINE: Duration = Duration::from_secs(90);

/// How long a stopped clone's git ([`clone_repo`]) has to end after its SIGTERM before what is
/// left of its process group is killed (SIGKILL).
pub const STOP_GRACE: Duration = Duration::from_secs(5);

/// The most bytes of a candidate's `origin` the report shows ([`Candidate::origin`]): a longer
/// one is cut there, `...` after it (whether it matches is told from all of it).
pub const ORIGIN_SHOWN_MAX: usize = 1024;

/// What `--clone-repo` prints, then a space and the clone's canonical path, as its last line
/// (after git's own output and a newline, so that it starts a line whatever git's last line
/// was): how the CLI finds what to record.
pub const CLONE_PATH_MARK: &str = "twr-clone-path";

/// Test only (debug builds only, ignored by a release build): `1` lets `--clone-repo`'s git
/// use the `file` transport too, so that a test can clone a local bare repository reached
/// through an `https://` URL that a `url.<base>.insteadOf` of the test's git config rewrites.
pub const TEST_ALLOW_FILE_ENV: &str = "TWR_TEST_CLONE_ALLOW_FILE";

/// `--find-repo`'s report (printed as JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FindReport {
    /// The box's home directory (`$HOME`, when absolute), where the roots are; `None`: no roots
    /// were searched (and the report is not complete).
    pub home: Option<String>,
    /// The box had a home to search, git runs there, no limit cut the search short (the
    /// roots' [`project_lookup::MAX_DIRS`] / [`project_lookup::MAX_ENTRIES`],
    /// [`MAX_CANDIDATES`], [`REPORT_MAX_BYTES`], [`FIND_DEADLINE`]), every folder searched
    /// could be read ([`project_lookup::Found::unreadable`], the hint's path), and git told
    /// everything asked of every repository found (whether it is one, its `origin`).
    pub complete: bool,
    /// git could not be found on the box (on the `PATH` this process has): nothing could be
    /// checked, and nothing is listed.
    #[serde(default)]
    pub git_missing: bool,
    /// The search stopped at its deadline ([`FIND_DEADLINE`]).
    #[serde(default)]
    pub timed_out: bool,
    pub candidates: Vec<Candidate>,
}

/// One repository `--find-repo` found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    /// Canonical.
    pub path: String,
    /// `path` is its repository's top level (else it is a folder inside one: only the hint can
    /// be).
    pub toplevel: bool,
    /// Its `origin` as git resolves it, [`redact`]ed, cut at [`ORIGIN_SHOWN_MAX`] bytes;
    /// `None`: none, or it could not be read ([`Candidate::origin_unread`]).
    pub origin: Option<String>,
    /// git could not read its `origin` (not the same as having none): the report is not
    /// [`FindReport::complete`], and it does not match.
    #[serde(default)]
    pub origin_unread: bool,
    /// A top level whose `origin` names the repository `--origin` names.
    pub matches_origin: bool,
    pub reason: Reason,
}

/// Why a candidate is listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// It is the `--hint` path.
    Hint,
    /// A repository of that folder name under a project root.
    Name,
}

/// `--find-repo --name <name> [--origin <url>] [--hint <path>]` with `git`, in `home`, until
/// `deadline` (see the module docs; [`FIND_DEADLINE`] from its start). Refused: a `name` that
/// is not one plain folder name, an `origin` that [`RemoteUrl::parse`] refuses (the hub sends
/// it without credentials), a relative `hint`.
pub async fn find_repo(
    git: &LocalGit,
    home: Option<&Path>,
    name: &str,
    origin: Option<&str>,
    hint: Option<&Path>,
    deadline: tokio::time::Instant,
) -> anyhow::Result<FindReport> {
    if !is_plain_name(name) {
        bail!(
            "--name {name:?} is not a folder name (letters, digits, `-`, `_` and `.`, not \
             starting with `.` or `-`)"
        );
    }
    let wanted = origin
        .map(|o| RemoteUrl::parse(o).map_err(|e| anyhow!("--origin {e}")))
        .transpose()?
        .map(|u| u.identity());
    if let Some(h) = hint.filter(|h| !h.is_absolute()) {
        bail!("--hint {} is not an absolute path", h.display());
    }
    let home = home.filter(|h| h.is_absolute());
    // No home: the roots were not searched, and one of them may hold it.
    let mut complete = home.is_some();
    let mut timed_out = false;
    let mut found: Vec<(PathBuf, Reason)> = Vec::new();
    // A hint that is not there is not a candidate; one that could not be resolved may be.
    match hint.map(std::fs::canonicalize) {
        Some(Ok(h)) if h.is_dir() => found.push((h, Reason::Hint)),
        Some(Err(e))
            if !matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            complete = false
        }
        _ => {}
    }
    if let Some(home) = home {
        let roots: Vec<PathBuf> = DEFAULT_PROJECT_ROOTS
            .iter()
            .filter_map(|r| expand_home(Path::new(r), Some(home)))
            .collect();
        let name = name.to_string();
        // The walk stops listing at the deadline; should one call of it hang (a dead network
        // mount), the search goes on without it (that thread is left to the process's exit).
        let until = std::time::Instant::now()
            + deadline.saturating_duration_since(tokio::time::Instant::now());
        let walk = tokio::task::spawn_blocking(move || {
            project_lookup::find_until(&roots, &name, Some(until))
        });
        match tokio::time::timeout_at(deadline, walk).await {
            Ok(lookup) => {
                let lookup = lookup.context("searching the project roots")?;
                complete &= !lookup.truncated && !lookup.unreadable;
                timed_out |= lookup.truncated && std::time::Instant::now() >= until;
                for repo in lookup.repos {
                    if !found.iter().any(|(p, _)| *p == repo) {
                        found.push((repo, Reason::Name));
                    }
                }
            }
            Err(_) => {
                complete = false;
                timed_out = true;
            }
        }
    }
    if found.len() > MAX_CANDIDATES {
        found.truncate(MAX_CANDIDATES);
        complete = false;
    }
    // Without git nothing can be told of any of them ([`GitOps::repo_root`] takes a missing
    // git for "no repository").
    let git_missing = match tokio::time::timeout_at(deadline, git.runs()).await {
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => true,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, "find-repo: git --version failed");
            false
        }
        Ok(Ok(())) => false,
        Err(_) => {
            timed_out = true;
            false
        }
    };
    if git_missing || timed_out {
        complete = false;
        found.clear();
    }
    let mut candidates = Vec::new();
    for (path, reason) in found {
        // Only a path the CLI can record (TOML, JSON) and show on one line.
        let Some(text) = path.to_str().filter(|t| !t.chars().any(char::is_control)) else {
            complete = false;
            continue;
        };
        let Ok(top) = tokio::time::timeout_at(deadline, git.repo_root(path.clone())).await else {
            timed_out = true;
            complete = false;
            break;
        };
        let top = match top {
            Ok(Some(top)) => top,
            // Not in a repository: a hint elsewhere is no candidate. A folder found by its
            // `.git` that git does not take for a repository (a broken `.git` file, say) may
            // be the project's all the same: the search is not complete without it.
            Ok(None) => {
                if reason == Reason::Name {
                    tracing::debug!(
                        path = %path.display(),
                        "find-repo: a folder with a .git that git finds no repository in"
                    );
                    complete = false;
                }
                continue;
            }
            // Git could not tell: one may be missing. (Its message is not logged: git quotes
            // the config in it, a value holding a URL with a password, say.)
            Err(_) => {
                tracing::debug!(
                    path = %path.display(),
                    "find-repo: git rev-parse --show-toplevel failed"
                );
                complete = false;
                continue;
            }
        };
        // A top level that cannot be resolved: this one may be it.
        let toplevel = match std::fs::canonicalize(&top) {
            Ok(t) => t == path,
            Err(_) => {
                complete = false;
                false
            }
        };
        // No origin (`git remote get-url` says there is no such remote) is not one git could
        // not read: that one may be the project's, so nothing is complete without it.
        // (Its error holds no message of git's: [`LocalGit::origin_url`].)
        let Ok(raw) = tokio::time::timeout_at(deadline, git.origin_url(&path)).await else {
            timed_out = true;
            complete = false;
            break;
        };
        let (raw, origin_unread) = match raw {
            Ok(raw) => (raw, false),
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    error = %format!("{e:#}"),
                    "find-repo: git remote get-url origin failed"
                );
                complete = false;
                (None, true)
            }
        };
        let matches_origin = toplevel
            && match (&wanted, &raw) {
                (Some(w), Some(r)) => RemoteUrl::parse_lenient(r).is_ok_and(|u| u.identity() == *w),
                _ => false,
            };
        candidates.push(Candidate {
            path: text.to_string(),
            toplevel,
            origin: raw.as_deref().map(shown_origin),
            origin_unread,
            matches_origin,
            reason,
        });
    }
    if timed_out {
        tracing::debug!("find-repo: stopped at its deadline; what was found by then is reported");
    }
    // Origin matches, then top levels before a folder inside one (only the hint can be), then
    // the hint before the rest.
    candidates.sort_by_key(|c| (!c.matches_origin, !c.toplevel, c.reason));
    let mut report = FindReport {
        home: home.and_then(|h| h.to_str()).map(str::to_string),
        complete,
        git_missing,
        timed_out,
        candidates,
    };
    fit(&mut report);
    Ok(report)
}

/// An `origin` as a report shows it: [`redact`]ed, then cut at [`ORIGIN_SHOWN_MAX`] bytes (a
/// character boundary) with `...` after it. (The CLI shows a box's origins through it again:
/// another build of twr on the box may have sent them as they are.)
pub fn shown_origin(raw: &str) -> String {
    let mut shown = redact(raw);
    if shown.len() > ORIGIN_SHOWN_MAX {
        let mut end = ORIGIN_SHOWN_MAX;
        while !shown.is_char_boundary(end) {
            end -= 1;
        }
        shown.truncate(end);
        shown.push_str("...");
    }
    shown
}

/// Leaves candidates out of `report` until its JSON fits [`REPORT_MAX_BYTES`]: the last one
/// first, the hint (what `--path` checks) last of all, and then the home; the report is not
/// [`FindReport::complete`] once anything was left out.
fn fit(report: &mut FindReport) {
    let size = |r: &FindReport| serde_json::to_vec(r).map_or(usize::MAX, |v| v.len());
    while size(report) > REPORT_MAX_BYTES {
        report.complete = false;
        let last = report
            .candidates
            .iter()
            .rposition(|c| c.reason != Reason::Hint)
            .or_else(|| report.candidates.len().checked_sub(1));
        match last {
            Some(i) => {
                report.candidates.remove(i);
            }
            None if report.home.is_some() => report.home = None,
            None => break,
        }
    }
}

/// The directory a clone was reserved ([`reserve_clone`]): made by this process, empty.
#[derive(Debug)]
pub struct CloneTarget {
    url: RemoteUrl,
    /// Canonical (in a canonical parent).
    path: PathBuf,
    /// The parent, held open as it was checked: what the target is made, checked and removed
    /// in (by its name there: `mkdirat`, `fstatat`, `openat`), whatever its path names
    /// meanwhile.
    parent: Dir,
    /// The target's name in it.
    name: String,
    /// Its device and inode as made: what cleaning up checks it still is.
    dev: u64,
    ino: u64,
}

/// Checks a clone's `url`, `parent` and `name` (see the module docs), and makes its target
/// `<parent>/<name>` (exclusively: an existing one is refused).
pub fn reserve_clone(url: &str, parent: &Path, name: &str) -> anyhow::Result<CloneTarget> {
    let url = RemoteUrl::parse(url).map_err(|e| anyhow!("{e}"))?;
    if !is_plain_name(name) {
        bail!(
            "{name:?} is not a folder name (letters, digits, `-`, `_` and `.`, not starting with \
             `.` or `-`)"
        );
    }
    let (parent, held) = check_parent(parent)?;
    let path = parent.join(name);
    let mut target = CloneTarget {
        url,
        path,
        parent: held,
        name: name.to_string(),
        dev: 0,
        ino: 0,
    };
    let name = OsStr::new(&target.name);
    // Mode 0700 as it is made (a umask only takes bits away): never, not even for a moment,
    // a directory another user could put something in while git fills it.
    if let Err(e) = target.parent.make_dir(name, 0o700) {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            bail!(
                "{} exists already: twr clones only into a new directory (pick another place, \
                 or record that one if it is the repository)",
                target.path.display()
            );
        }
        return Err(anyhow!("making {}: {e}", target.path.display()));
    }
    let meta = target
        .entry()
        .with_context(|| format!("reading {}", target.path.display()))?;
    if let Err(why) = check_made(&meta) {
        // (Removed only while it is a directory, empty: `remove_dir` takes no other.)
        let left = match meta.dir && target.parent.remove_dir(name).is_ok() {
            true => "removed again",
            false => "left as it is",
        };
        bail!(
            "{} is not what twr made: {why} ({left})",
            target.path.display()
        );
    }
    (target.dev, target.ino) = (meta.dev, meta.ino);
    Ok(target)
}

/// What [`check_made`] reads of a directory entry: from `fstatat` without following a symbolic
/// link ([`CloneTarget::entry`]), or from the metadata of the directory opened.
#[derive(Debug, Clone, Copy)]
struct Seen {
    /// A symbolic link (only an `fstatat` sees one).
    symlink: bool,
    /// A directory.
    dir: bool,
    uid: u32,
    /// Its permission bits (and set-ID and sticky bits).
    mode: u32,
    dev: u64,
    ino: u64,
}

impl From<&std::fs::Metadata> for Seen {
    fn from(m: &std::fs::Metadata) -> Seen {
        Seen {
            symlink: m.file_type().is_symlink(),
            dir: m.is_dir(),
            uid: m.uid(),
            mode: m.mode() & 0o7777,
            dev: m.dev(),
            ino: m.ino(),
        }
    }
}

/// Why `meta`, a clone's target as [`reserve_clone`] made it, is not the directory it asked
/// for: one (not a symbolic link), this user's, mode 0700 (the umask may have taken bits of
/// this user's away; a set-group-ID bit the parent passes on is fine).
fn check_made(meta: &Seen) -> Result<(), String> {
    if meta.symlink || !meta.dir {
        return Err("not a directory".into());
    }
    if meta.uid != effective_uid() {
        return Err(format!("it belongs to uid {}", meta.uid));
    }
    let mode = meta.mode & 0o777;
    if mode != 0o700 {
        return Err(format!(
            "its mode is {mode:03o}, not 700 (does the umask take this user's bits away?)"
        ));
    }
    Ok(())
}

/// `parent`, canonical, when a clone may go in it (see the module docs), and the directory
/// itself, held open.
fn check_parent(parent: &Path) -> anyhow::Result<(PathBuf, Dir)> {
    let shown = parent.display();
    if !parent.is_absolute() {
        bail!("{shown} is not an absolute path");
    }
    if parent
        .to_str()
        .is_none_or(|p| p.chars().any(char::is_control))
    {
        bail!("{shown}: twr clones only under a path of printable UTF-8 text");
    }
    let meta = std::fs::symlink_metadata(parent).with_context(|| format!("reading {shown}"))?;
    if meta.file_type().is_symlink() {
        bail!("{shown} is a symbolic link: give the directory it points to");
    }
    if !meta.is_dir() {
        bail!("{shown} is not a directory");
    }
    if meta.uid() != effective_uid() {
        bail!("{shown} belongs to uid {}, not to you", meta.uid());
    }
    if meta.permissions().mode() & 0o022 != 0 {
        bail!(
            "{shown}: its group or other users can write to it, so they could put something of \
             theirs where the clone goes (`chmod go-w` it, or pick another directory)"
        );
    }
    check_ancestors(parent)?;
    let canonical = std::fs::canonicalize(parent).with_context(|| format!("resolving {shown}"))?;
    let held = Dir::open(&canonical).with_context(|| format!("opening {shown}"))?;
    let again = held.file().metadata()?;
    if (again.dev(), again.ino()) != (meta.dev(), meta.ino()) {
        bail!("{shown} changed while it was checked");
    }
    Ok((canonical, held))
}

impl CloneTarget {
    /// Where the clone goes (canonical).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The URL, as it is cloned.
    pub fn url(&self) -> &RemoteUrl {
        &self.url
    }

    /// The target's entry in the parent held open (`fstatat`, a symbolic link not followed),
    /// whatever the directories above the parent now are (a group may be able to write to
    /// one: [`check_ancestors`]).
    // (`st_dev` and `st_mode` are of other types on macOS.)
    #[allow(clippy::unnecessary_cast)]
    fn entry(&self) -> std::io::Result<Seen> {
        use std::os::fd::AsRawFd;
        let name = std::ffi::CString::new(self.name.as_str())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        // SAFETY: an all-zero `stat` is a valid value; fstatat(2) writes into it.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: fstatat(2) of a NUL-terminated name in a descriptor this process holds open,
        // with a valid pointer to `st`.
        let r = unsafe {
            libc::fstatat(
                self.parent.file().as_raw_fd(),
                name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if r == -1 {
            return Err(std::io::Error::last_os_error());
        }
        let kind = st.st_mode & libc::S_IFMT;
        Ok(Seen {
            symlink: kind == libc::S_IFLNK,
            dir: kind == libc::S_IFDIR,
            uid: st.st_uid,
            mode: st.st_mode as u32 & 0o7777,
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        })
    }

    /// The `git clone` to run (see the module docs), with this process's stdin, stdout and
    /// stderr; the `file` transport allowed too only by [`TEST_ALLOW_FILE_ENV`] in a debug
    /// build.
    pub fn command(&self) -> tokio::process::Command {
        let allow_file = cfg!(debug_assertions)
            && std::env::var_os(TEST_ALLOW_FILE_ENV).is_some_and(|v| v == "1");
        self.command_with(allow_file)
    }

    /// git runs in the parent held open (it changes into it by its descriptor before it execs)
    /// and clones into the target's name there: should the parent be moved, and another
    /// directory put at its path meanwhile (through a directory above it that its group may
    /// write to), git still clones into the target made, not into that one. `self` must
    /// outlive the command (its parent's descriptor is what git changes into).
    fn command_with(&self, allow_file: bool) -> tokio::process::Command {
        use std::os::fd::AsRawFd;
        let mut c = tokio::process::Command::new("git");
        without_git_env(&mut c);
        let parent = self.parent.file().as_raw_fd();
        // SAFETY: fchdir(2) is async-signal-safe and the closure allocates nothing (an
        // `io::Error` of an OS error does not); the descriptor is this process's, open while
        // `self` is (closed in git as it execs: `O_CLOEXEC`).
        unsafe {
            c.pre_exec(move || match libc::fchdir(parent) {
                -1 => Err(std::io::Error::last_os_error()),
                _ => Ok(()),
            });
        }
        let mut protocols = "https:ssh".to_string();
        c.args(["-c", "protocol.allow=never"])
            .args(["-c", "protocol.https.allow=always"])
            .args(["-c", "protocol.ssh.allow=always"]);
        if allow_file {
            protocols.push_str(":file");
            c.args(["-c", "protocol.file.allow=always"]);
        }
        c.env("GIT_ALLOW_PROTOCOL", protocols)
            // Git LFS fetches from an endpoint the repository may choose: not at all.
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .arg("clone")
            .arg("--")
            .arg(self.url.redacted())
            .arg(&self.name);
        c
    }

    /// After git ran (`status`): on success, the clone's canonical path once it is still the
    /// directory made and its repository's top level, in the parent at that path
    /// ([`CloneTarget::verify`]); else an error, the target removed when it is still the
    /// directory made ([`CloneTarget::clean_up`]).
    pub async fn finish(self, status: std::io::Result<ExitStatus>) -> anyhow::Result<PathBuf> {
        let why = match status {
            Ok(s) if s.success() => return self.verify().await,
            Ok(s) => match s.code() {
                Some(c) => format!("git clone failed (exit {c})"),
                None => "git clone was stopped by a signal".to_string(),
            },
            Err(e) => format!("running git: {e}"),
        };
        bail!("{why} for {}: {}", self.url, self.left())
    }

    /// Cleans up ([`CloneTarget::clean_up`]): what it says of what is left.
    fn left(&self) -> String {
        match self.clean_up() {
            Ok(()) => "nothing was left behind".to_string(),
            Err(e) => format!("{} was left as it is: {e:#}", self.path.display()),
        }
    }

    /// The clone's canonical path, once the target, reached in the parent held open, is still
    /// the directory made ([`CloneTarget::open_made`]), git (run in it, by its descriptor)
    /// says its repository's top level is that directory, and the parent held open is still
    /// the directory at the parent's path (the path recorded names the clone). A parent moved
    /// meanwhile keeps the clone, which is left as it is there, not recorded.
    async fn verify(&self) -> anyhow::Result<PathBuf> {
        let refused = |why: &str| {
            anyhow!(
                "git cloned {} into {}, but {why}: left as it is, not recorded",
                self.url,
                self.path.display()
            )
        };
        let dir = self.open_made().map_err(|why| refused(&why))?;
        // (git's message is not shown: it may quote the user's git config.)
        let top = top_level(dir.file())
            .await
            .map_err(|_| refused("`git rev-parse --show-toplevel` failed there"))?;
        let same = |m: &std::fs::Metadata, dev: u64, ino: u64| (m.dev(), m.ino()) == (dev, ino);
        if !std::fs::symlink_metadata(&top).is_ok_and(|m| same(&m, self.dev, self.ino)) {
            return Err(refused("that is not a repository's top level now"));
        }
        let held = self.parent.file().metadata().map_err(|e| {
            refused(&format!(
                "the directory it was made in could not be read ({e})"
            ))
        })?;
        let parent = self.path.parent().unwrap_or(&self.path);
        if !std::fs::symlink_metadata(parent).is_ok_and(|m| same(&m, held.dev(), held.ino())) {
            return Err(refused(&format!(
                "the directory it was made in is no longer at {} (it was moved)",
                parent.display()
            )));
        }
        let at_path =
            std::fs::symlink_metadata(&self.path).is_ok_and(|m| same(&m, self.dev, self.ino));
        if !at_path || std::fs::canonicalize(&self.path).ok().as_ref() != Some(&self.path) {
            return Err(refused("it is no longer at that path"));
        }
        Ok(self.path.clone())
    }

    /// The target, opened in the parent held open (no symbolic link followed), while it is
    /// still the directory [`reserve_clone`] made (the same device and inode, as made:
    /// [`check_made`]); else why not.
    fn open_made(&self) -> Result<Dir, String> {
        let gone = "it is not the directory twr made any more";
        let dir = self
            .parent
            .dir(OsStr::new(&self.name))
            .map_err(|e| format!("{gone} (opening it: {e})"))?;
        let meta = dir
            .file()
            .metadata()
            .map_err(|e| format!("it could not be read ({e})"))?;
        if (meta.dev(), meta.ino()) != (self.dev, self.ino) {
            return Err(gone.into());
        }
        check_made(&Seen::from(&meta)).map_err(|why| format!("{gone}: {why}"))?;
        Ok(dir)
    }

    /// `Err` (why) unless the target, reached in the parent held open, is still the
    /// directory [`reserve_clone`] made: the same device and inode, and as made
    /// ([`check_made`]). (`Ok(None)`: it is gone.)
    fn still_made(&self) -> Result<Option<Seen>, String> {
        let meta = match self.entry() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("it could not be read ({e})")),
        };
        if (meta.dev, meta.ino) != (self.dev, self.ino) {
            return Err("it is not the directory twr made any more".into());
        }
        check_made(&meta)
            .map_err(|why| format!("it is not the directory twr made any more: {why}"))?;
        Ok(Some(meta))
    }

    /// Removes the target, whatever git left in it (only this user can write in a directory of
    /// theirs of mode 0700), when it is still the directory [`reserve_clone`] made
    /// ([`CloneTarget::still_made`], then [`CloneTarget::open_made`]), in the parent held open:
    /// emptied through the directory opened ([`remove_contents`]), then removed by its name in
    /// the parent. Gone already is fine.
    pub fn clean_up(&self) -> anyhow::Result<()> {
        match self.still_made() {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => {}
            Err(why) => bail!("{why}"),
        }
        let dir = self.open_made().map_err(|why| anyhow!("{why}"))?;
        remove_contents(&dir, self.dev)?;
        drop(dir);
        self.parent.remove_dir(OsStr::new(&self.name))?;
        Ok(())
    }
}

/// Removes everything in `dir`, a clone's target as made, whatever it is (a `.git` directory,
/// a partial checkout), through descriptors alone: each directory held open and listed through
/// its descriptor ([`Dir::listed`]), each entry unlinked by its name in it, a directory opened
/// there (no symbolic link followed: a link is removed, never what it names), made readable,
/// writable and searchable by its owner when it is not, emptied the same way and removed.
/// Refuses (an error, what is left stays) a directory on another file system than `dev` (a
/// mount point). Each level holds a descriptor open while it empties the next.
fn remove_contents(dir: &Dir, dev: u64) -> std::io::Result<()> {
    use std::io::ErrorKind;
    for name in dir.listed()? {
        let unlinked = match dir.unlink(&name) {
            Ok(()) => continue,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => e,
        };
        let sub = match dir.dir(&name) {
            Ok(d) => d,
            Err(e) if e.kind() == ErrorKind::PermissionDenied => {
                dir.open_up(&name)?;
                dir.dir(&name)?
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
        remove_contents(&sub, dev)?;
        dir.remove_dir(&name)?;
    }
    Ok(())
}

/// Removes the session's git variables (a `GIT_DIR`, `GIT_CONFIG_*`, `GIT_SSH_COMMAND`, ...)
/// from `c`'s environment: they are not the clone's.
fn without_git_env(c: &mut tokio::process::Command) {
    for (k, _) in std::env::vars_os() {
        if k.to_str().is_some_and(|k| k.starts_with("GIT_")) {
            c.env_remove(k);
        }
    }
}

/// How long [`top_level`]'s git may take.
const TOP_LEVEL_TIMEOUT: Duration = Duration::from_secs(60);

/// What `git rev-parse --show-toplevel` prints, run in `dir` (git changes into it by its
/// descriptor before it execs) with twr's safe git settings ([`SAFE_CONFIG`]), without the
/// session's git variables; git's message is not kept (it may quote the user's config).
async fn top_level(dir: &std::fs::File) -> anyhow::Result<PathBuf> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    use std::process::Stdio;
    let mut c = tokio::process::Command::new("git");
    without_git_env(&mut c);
    for kv in SAFE_CONFIG {
        c.arg("-c").arg(kv);
    }
    c.args(["rev-parse", "--show-toplevel"])
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    let fd = dir.as_raw_fd();
    // SAFETY: fchdir(2) is async-signal-safe and the closure allocates nothing; `dir` is open
    // until the command has run (it is awaited here).
    unsafe {
        c.pre_exec(move || match libc::fchdir(fd) {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }
    let out = tokio::time::timeout(TOP_LEVEL_TIMEOUT, c.output())
        .await
        .map_err(|_| anyhow!("git rev-parse did not finish in time"))??;
    if !out.status.success() {
        bail!("git rev-parse failed");
    }
    let mut top = out.stdout;
    if top.last() == Some(&b'\n') {
        top.pop();
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(top)))
}

/// The name of signal `sig` ([`clone_repo`]'s stops).
fn signal_name(sig: libc::c_int) -> &'static str {
    match sig {
        libc::SIGHUP => "SIGHUP (the connection or the terminal closed)",
        libc::SIGTERM => "SIGTERM",
        _ => "a signal",
    }
}

/// Waits for signal stream `s` (forever without one).
async fn received(s: &mut Option<tokio::signal::unix::Signal>) {
    match s.as_mut() {
        Some(s) => {
            s.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// `--clone-repo --url <url> --parent <parent> --name <name>` (see the module docs): the
/// clone's canonical path. The caller checked that stdin and stdout are a terminal.
pub async fn clone_repo(url: &str, parent: &Path, name: &str) -> anyhow::Result<PathBuf> {
    clone_repo_with(url, parent, name, STOP_GRACE).await
}

/// [`clone_repo`] with `grace` for a stopped git ([`STOP_GRACE`]).
async fn clone_repo_with(
    url: &str,
    parent: &Path,
    name: &str,
    grace: Duration,
) -> anyhow::Result<PathBuf> {
    use futures::FutureExt as _;
    use tokio::signal::unix::{signal, SignalKind};
    // Installed before the target is made, held while git runs: the terminal's signals go to
    // git's process group, the terminal's foreground one ([`run_group`]); should one reach
    // this process all the same (it is not the foreground group when stdin is not its
    // terminal), it does not stop it before it has cleaned up (a handler installed here is
    // the default again in git, which execs). A SIGTERM or a hangup stops git's whole group.
    let held: Vec<_> = [SignalKind::interrupt(), SignalKind::quit()]
        .into_iter()
        .filter_map(|k| signal(k).ok())
        .collect();
    let mut term = signal(SignalKind::terminate()).ok();
    let mut hup = signal(SignalKind::hangup()).ok();
    let by = std::cell::Cell::new(None);
    let stopped = async {
        let sig = tokio::select! {
            () = received(&mut term) => libc::SIGTERM,
            () = received(&mut hup) => libc::SIGHUP,
        };
        by.set(Some(sig));
        sig
    };
    let mut stopped = std::pin::pin!(stopped);
    let target = reserve_clone(url, parent, name)?;
    #[cfg(test)]
    if tests::RAISE_AFTER_RESERVE.load(std::sync::atomic::Ordering::Relaxed) {
        // SAFETY: raise(3) of a signal this process handles (above).
        unsafe { libc::raise(libc::SIGTERM) };
    }
    // Stopped while the target was made (as far as the runtime has seen it yet): git never
    // starts. (One it sees later stops git as soon as it runs, below.)
    if let Some(sig) = stopped.as_mut().now_or_never() {
        bail!(
            "stopped by {} before git started: {}",
            signal_name(sig),
            target.left()
        );
    }
    #[cfg(test)]
    tests::before_spawn(&target);
    let status = run_group(target.command(), stopped, grace).await;
    drop(held);
    let done = target.finish(status).await;
    match by.get() {
        Some(sig) => done.with_context(|| format!("stopped by {}", signal_name(sig))),
        None => done,
    }
}

/// Runs `cmd` (with this process's stdin, stdout and stderr) as the leader of a process group
/// of its own, and, when this process's group is the foreground one of the terminal on its
/// stdin, makes that group the terminal's foreground group (in the child, before it execs):
/// what the user types, Ctrl-C included, goes to it and whatever it starts (transport and
/// credential helpers), and a question it asks there is answered. The terminal goes back to
/// this process's group once the leader has exited.
///
/// When `stop` is ready first (with a signal: SIGTERM, or SIGHUP, which is sent first), the
/// group gets that signal and SIGTERM, `grace` to end (every member), then SIGKILL. When it
/// failed, or was stopped, the whole group is killed and waited for (every member gone,
/// [`crate::local::wait_group_gone`]) before this returns, so that cleaning up after it races
/// no helper of it still writing in the target: the group is signalled while its leader has
/// exited but is not reaped yet (`waitid` with `WNOWAIT`, [`exited_unreaped`]), so its id names
/// no other group. One that succeeded leaves its group as it is (a credential cache daemon git
/// started stays) on Linux; elsewhere what `waitid` reports of the leader is not trusted (macOS
/// does not fill it in reliably), so the group is killed whatever the status (fail closed: a
/// credential cache daemon is stopped after a successful clone too, and git starts another
/// when it needs one). The status returned is always the reap's.
///
/// Should that wait fail: with `ECHILD` the leader is no longer this process's to wait for
/// (something else reaped it, so its id may name another process): its group is not signalled
/// again (the wait's answer is looked at before each signal of a stop, and wins over a stop
/// that comes at the same time), and the reap's error is returned
/// ([`twr_adapters::process::ownership_lost`]: nothing signals it after that, this future's
/// drop included); otherwise it is still this process's and not reaped (only this function
/// reaps it), so its group is killed (the leader with it, exited or not) and waited for before
/// the reap. Dropped before it returns, it kills the group (while it still owns its leader).
pub(crate) async fn run_group(
    cmd: tokio::process::Command,
    stop: impl std::future::Future<Output = libc::c_int>,
    grace: Duration,
) -> std::io::Result<ExitStatus> {
    run_group_with(cmd, stop, grace, exited_unreaped).await
}

/// [`run_group`], `wait` standing for [`exited_unreaped`] (tests make it fail).
async fn run_group_with(
    mut cmd: tokio::process::Command,
    stop: impl std::future::Future<Output = libc::c_int>,
    grace: Duration,
    wait: impl FnOnce(u32) -> std::io::Result<bool> + Send + 'static,
) -> std::io::Result<ExitStatus> {
    // SAFETY: isatty, tcgetpgrp and getpgrp only read; no memory is passed.
    let hand_over = unsafe { libc::isatty(0) == 1 && libc::tcgetpgrp(0) == libc::getpgrp() };
    // SAFETY: setpgid, sigemptyset, sigaddset, sigprocmask, tcsetpgrp and getpid are
    // async-signal-safe; the closure allocates nothing (an `io::Error` of an OS error does not)
    // and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if hand_over {
                // From a background group (this one, now) with SIGTTOU blocked, or the kernel
                // would stop it for asking; the mask as it was again before exec.
                let mut set: libc::sigset_t = std::mem::zeroed();
                let mut old: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGTTOU);
                libc::sigprocmask(libc::SIG_BLOCK, &set, &mut old);
                let r = libc::tcsetpgrp(0, libc::getpid());
                let e = std::io::Error::last_os_error();
                libc::sigprocmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
                if r == -1 {
                    return Err(e);
                }
            }
            Ok(())
        });
    }
    // Not the `Child`'s `kill_on_drop`: it would signal the leader's id even once something
    // else reaped it. The group guard below is the backstop.
    cmd.kill_on_drop(false);
    // Whatever happens from here on, the terminal comes back (a child that handed it over and
    // then failed to exec included).
    let _terminal = hand_over.then_some(TerminalBack);
    let mut child = cmd.spawn()?;
    let Some(pid) = child.id() else {
        return child.wait().await;
    };
    // A backstop: should this future be dropped, the group (the leader in it) is not left
    // running. Disarmed once the leader is reaped, here or (ownership lost) elsewhere.
    let mut guard = GroupGuard(Some(pid));
    let mut leader = tokio::task::spawn_blocking(move || wait(pid));
    tokio::pin!(stop);
    // The leader first: a wait that has answered (an `ECHILD` above all) is seen before a stop
    // that came at the same time signals anything.
    let (exited, stopped) = tokio::select! {
        biased;
        r = &mut leader => (r, false),
        sig = &mut stop => {
            // What the leader's wait said by now, if it has: signalled only while it does not
            // say the leader is gone to something else (not reaped here: only this task reaps
            // it, below, so until then `pid` still names its group).
            let mut exited = None;
            for sig in [Some(sig), (sig != libc::SIGTERM).then_some(libc::SIGTERM)]
                .into_iter()
                .flatten()
            {
                if exited.is_none() && leader.is_finished() {
                    exited = Some((&mut leader).await);
                }
                if exited.as_ref().is_some_and(lost) {
                    break;
                }
                signal_group(pid, sig);
            }
            let ended = tokio::time::timeout(grace, async {
                let r = match exited.take() {
                    Some(r) => r,
                    None => (&mut leader).await,
                };
                let waitable = matches!(r, Ok(Ok(_)));
                exited = Some(r);
                if waitable {
                    crate::local::wait_group_gone(pid, "git clone").await;
                }
            })
            .await
            .is_ok();
            if !ended && !exited.as_ref().is_some_and(lost) {
                tracing::debug!(pid, "git clone did not end within its grace: killing its group");
                signal_group(pid, libc::SIGKILL);
            }
            let exited = match exited {
                Some(r) => r,
                None => leader.await,
            };
            (exited, true)
        }
    };
    let succeeded = matches!(exited, Ok(Ok(true)));
    if stopped || !succeeded {
        match &exited {
            r if lost(r) => {
                // Not this process's child any more: its id may name another group by now.
                tracing::warn!(pid, "waitid: git clone is not this process's child any more; its group is not signalled again");
            }
            r => {
                if let Ok(Err(e)) = r {
                    tracing::warn!(pid, error = %e, "waitid failed: killing git clone's process group before reaping it");
                }
                // Not reaped (only this function reaps it): `pid` still names its group.
                signal_group(pid, libc::SIGKILL);
                crate::local::wait_group_gone(pid, "git clone").await;
            }
        }
    }
    let status = child.wait().await;
    match &status {
        Ok(_) => guard.0 = None,
        Err(e) if twr_adapters::process::ownership_lost(e) => {
            tracing::warn!(
                pid,
                "git clone was reaped by something else: its group is not signalled"
            );
            guard.0 = None;
        }
        Err(_) => {}
    }
    status
}

/// Whether `r`, the result of [`run_group`]'s wait for the leader, says it is not this
/// process's child any more (`ECHILD`): its id is then not signalled again.
fn lost(r: &Result<std::io::Result<bool>, tokio::task::JoinError>) -> bool {
    matches!(r, Ok(Err(e)) if twr_adapters::process::ownership_lost(e))
}

/// Kills process group `.0` when dropped ([`run_group`] dropped before it returns), unless
/// cleared first (its leader reaped: the id may be another process's by then).
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            signal_group(pgid, libc::SIGKILL);
        }
    }
}

/// Sends `sig` to process group `pgid` (its leader this process's child, not reaped).
fn signal_group(pgid: u32, sig: libc::c_int) {
    // SAFETY: kill(2) with a negative pid signals that process group; no memory is passed.
    let r = unsafe { libc::kill(-(pgid as libc::pid_t), sig) };
    tracing::debug!(
        pgid,
        sig,
        signalled = r == 0,
        "signalled git clone's process group"
    );
}

/// Blocks until child `pid` has exited, without reaping it (`waitid` with `WNOWAIT`): whether
/// it is known to have exited with status 0, which only Linux says (macOS does not fill in
/// `waitid`'s status reliably: `false` there, whatever it was; [`run_group`] then fails
/// closed).
fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value; waitid(2) writes into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) with a valid pointer to `info`; WNOWAIT leaves the child waitable.
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            // SAFETY: waitid filled `info` for an exited child: `si_status` is its status.
            return Ok(cfg!(target_os = "linux")
                && info.si_code == libc::CLD_EXITED
                && unsafe { info.si_status() } == 0);
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Gives the terminal on stdin back to this process's group when dropped ([`run_group`]).
struct TerminalBack;

impl Drop for TerminalBack {
    fn drop(&mut self) {
        // SAFETY: sigset and mask calls on local values, getpgrp and tcsetpgrp on fd 0; this
        // thread's mask is as it was again before returning.
        let failed = unsafe {
            // From a background group: SIGTTOU blocked (in this thread, which asks), or the
            // kernel would stop this process for it.
            let mut set: libc::sigset_t = std::mem::zeroed();
            let mut old: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGTTOU);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old);
            let failed =
                (libc::tcsetpgrp(0, libc::getpgrp()) == -1).then(std::io::Error::last_os_error);
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
            failed
        };
        if let Some(e) = failed {
            tracing::debug!(error = %e, "could not take the terminal back from git clone");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::process::Stdio;
    use twr_core::testkit::git::{init_repo, run};

    /// A git config home for the clone's git: `HOME` with `config` as its `.gitconfig`.
    fn git_home(dir: &Path, config: &str) -> PathBuf {
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".gitconfig"), config).unwrap();
        home
    }

    /// A directory a clone may go in (no group or other write, whatever the umask).
    fn parent(dir: &Path) -> PathBuf {
        let p = dir.join("parent");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.canonicalize().unwrap()
    }

    /// Runs `target`'s clone with git config home `home`, its output captured: (status,
    /// stderr).
    async fn run_clone(
        target: &CloneTarget,
        home: &Path,
        allow_file: bool,
    ) -> (std::io::Result<ExitStatus>, String) {
        let out = target
            .command_with(allow_file)
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        match out {
            Ok(o) => (
                Ok(o.status),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            ),
            Err(e) => (Err(e), String::new()),
        }
    }

    /// An `https://` URL that the box's git config rewrites to `ext::` (or to a local
    /// `file://` repository) is refused by git: the command never runs, and the failed clone
    /// leaves nothing.
    #[tokio::test]
    async fn a_rewrite_to_another_transport_is_refused_by_git() {
        let d = tempfile::tempdir().unwrap();
        let ran = d.path().join("ran");
        let bare = d.path().join("bare.git");
        run(d.path(), &["init", "-q", "--bare", bare.to_str().unwrap()]);
        let home = git_home(
            d.path(),
            &format!(
                "[url \"ext::sh -c touch% {}% \"]\n\tinsteadOf = https://example.invalid/ext/\n\
                 [url \"file://{}\"]\n\tinsteadOf = https://example.invalid/file\n\
                 [protocol \"ext\"]\n\tallow = always\n[protocol]\n\tallow = always\n",
                ran.display(),
                bare.display()
            ),
        );
        let parent = parent(d.path());
        for (url, transport) in [
            ("https://example.invalid/ext/r", "'ext'"),
            ("https://example.invalid/file", "'file'"),
        ] {
            let target = reserve_clone(url, &parent, "r").unwrap();
            assert!(target.path().is_dir());
            let (status, stderr) = run_clone(&target, &home, false).await;
            assert!(!status.as_ref().unwrap().success(), "{stderr}");
            assert!(
                stderr.contains(&format!("transport {transport} not allowed")),
                "{stderr}"
            );
            let e = target.finish(status).await.unwrap_err();
            let e = format!("{e:#}");
            assert!(e.contains("nothing was left behind"), "{e}");
            assert!(!parent.join("r").exists());
            assert!(!ran.exists(), "ext:: ran its command");
        }
    }

    /// A clone that succeeds (a local bare repository through the test-only `file` seam,
    /// reached by an `https://` URL the config rewrites) is its repository's top level, at the
    /// canonical path; the seam is what lets `file` through.
    #[tokio::test]
    async fn a_clone_is_the_new_repositorys_top_level() {
        let d = tempfile::tempdir().unwrap();
        let src = init_repo(&d.path().join("src"));
        let bare = d.path().join("bare.git");
        run(
            d.path(),
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let home = git_home(
            d.path(),
            &format!(
                "[url \"file://{}\"]\n\tinsteadOf = https://example.invalid/o/api.git\n",
                bare.display()
            ),
        );
        let parent = parent(d.path());
        let target = reserve_clone("https://example.invalid/o/api.git", &parent, "api").unwrap();
        let (status, stderr) = run_clone(&target, &home, true).await;
        assert!(status.as_ref().unwrap().success(), "{stderr}");
        let path = target.finish(status).await.unwrap();
        assert_eq!(path, parent.join("api"));
        assert!(path.join("README.md").is_file());
        assert_eq!(
            LocalGit::default().repo_root(path.clone()).await.unwrap(),
            Some(path.clone())
        );
    }

    #[tokio::test]
    async fn the_command_is_git_clone_of_the_url_alone() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let target = reserve_clone("git@github.com:o/r.git", &parent, "r").unwrap();
        let c = target.command_with(false);
        let c = c.as_std();
        assert_eq!(c.get_program(), "git");
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let want = [
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "protocol.ssh.allow=always",
            "clone",
            "--",
            "git@github.com:o/r.git",
        ];
        assert_eq!(&args[..want.len()], want);
        // The target's name: git runs in the parent held open.
        assert_eq!(args[want.len()..], ["r"]);
        let allowed = |c: &std::process::Command| {
            c.get_envs()
                .find(|(k, _)| *k == OsStr::new("GIT_ALLOW_PROTOCOL"))
                .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
        };
        assert_eq!(allowed(c).as_deref(), Some("https:ssh"));
        // Git LFS fetches nothing (its endpoint is the repository's choice).
        assert!(c
            .get_envs()
            .any(|(k, v)| k == OsStr::new("GIT_LFS_SKIP_SMUDGE") && v == Some(OsStr::new("1"))));
        let target = reserve_clone("https://h/r", &parent, "s").unwrap();
        let c = target.command_with(true);
        assert_eq!(allowed(c.as_std()).as_deref(), Some("https:ssh:file"));
        assert!(c
            .as_std()
            .get_args()
            .any(|a| a == OsStr::new("protocol.file.allow=always")));
        // The session's git variables are removed.
        let removed: Vec<_> = c
            .as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(removed.iter().all(|k| k.starts_with("GIT_")), "{removed:?}");
    }

    #[test]
    fn what_a_clone_refuses_it_refuses_before_making_anything() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let refused = |url: &str, parent: &Path, name: &str| {
            format!("{:#}", reserve_clone(url, parent, name).unwrap_err())
        };
        // URLs (`gitremote` has the rest).
        for (url, says) in [
            ("file:///srv/r.git", "file://"),
            ("/srv/r.git", "local path"),
            ("ext::sh -c x", "space"),
            ("https://u:hunter2@h/r", "password or a token"),
            ("https://u:hunter2@SEC2@h/r", "more than one `@`"),
            ("me:hunter2@SEC2@h:r", "more than one `@`"),
            ("ssh://git:hunter2:SEC2@SEC3@h/r", "more than one `@`"),
            ("me:hunter2/SEC2@SEC3@h:r", "an `@` after the host"),
            ("https://u:hunter2#SEC2@SEC3@h/r", "query"),
            ("-u", "`-`"),
        ] {
            let e = refused(url, &parent, "r");
            assert!(e.contains(says), "{url}: {e}");
            assert!(!e.contains("hunter2") && !e.contains("SEC"), "{e}");
        }
        // Names.
        for name in ["", "..", ".hidden", "a/b", "-x", "a b"] {
            let e = refused("https://h/r", &parent, name);
            assert!(e.contains("not a folder name"), "{name:?}: {e}");
        }
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
        // An existing target, empty or not, is never reused.
        std::fs::create_dir(parent.join("r")).unwrap();
        let e = refused("https://h/r", &parent, "r");
        assert!(e.contains("exists already"), "{e}");
        std::fs::write(parent.join("f"), "").unwrap();
        let e = refused("https://h/r", &parent, "f");
        assert!(e.contains("exists already"), "{e}");
        assert!(parent.join("r").is_dir() && parent.join("f").is_file());
        // Parents: relative, missing, a file, a symbolic link, group or other writable.
        let e = refused("https://h/r", Path::new("rel"), "r");
        assert!(e.contains("not an absolute path"), "{e}");
        let e = refused("https://h/r", &d.path().join("missing"), "r");
        assert!(e.contains("reading"), "{e}");
        let e = refused("https://h/r", &parent.join("f"), "r");
        assert!(e.contains("not a directory"), "{e}");
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&parent, &link).unwrap();
        let e = refused("https://h/r", &link, "x");
        assert!(e.contains("symbolic link"), "{e}");
        for mode in [0o775, 0o757] {
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = refused("https://h/r", &parent, "x");
            assert!(e.contains("can write to it"), "{mode:o}: {e}");
        }
        assert!(!parent.join("x").exists());
        let e = refused("https://h/r", Path::new("/tmp/a\nb"), "x");
        assert!(e.contains("printable"), "{e}");
    }

    /// A failed clone's directory is removed, whatever git left in it, only while it is the
    /// one made (the same inode, mode 0700); a symbolic link in it is removed, not followed.
    #[tokio::test]
    async fn a_failed_clone_removes_only_what_it_made() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let fail = || -> std::io::Result<ExitStatus> {
            use std::os::unix::process::ExitStatusExt;
            Ok(ExitStatus::from_raw(128 << 8))
        };
        // Holding a partial `.git`: removed.
        let t = reserve_clone("https://h/r", &parent, "a").unwrap();
        std::fs::create_dir_all(t.path().join(".git/objects")).unwrap();
        let e = format!("{:#}", t.finish(fail()).await.unwrap_err());
        assert!(
            e.contains("exit 128") && e.contains("nothing was left"),
            "{e}"
        );
        assert!(!parent.join("a").exists());
        // Holding a partial checkout and a symbolic link out of it: removed, the link's
        // target untouched.
        let outside = d.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep"), "x").unwrap();
        let t = reserve_clone("https://h/r", &parent, "b").unwrap();
        std::fs::create_dir_all(t.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(t.path().join("src/deep")).unwrap();
        std::fs::write(t.path().join("src/deep/file.rs"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, t.path().join("src/link")).unwrap();
        let e = format!("{:#}", t.finish(fail()).await.unwrap_err());
        assert!(e.contains("nothing was left"), "{e}");
        assert!(!parent.join("b").exists());
        assert!(outside.join("keep").is_file());
        // Its mode changed (not as made): kept.
        let t = reserve_clone("https://h/r", &parent, "m").unwrap();
        std::fs::write(t.path().join("mine"), "x").unwrap();
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = format!("{:#}", t.finish(fail()).await.unwrap_err());
        assert!(
            e.contains("left as it is") && e.contains("mode is 755"),
            "{e}"
        );
        assert!(parent.join("m/mine").is_file());
        // Replaced by another directory: kept.
        // (Made while the original exists, so it is another inode.)
        let t = reserve_clone("https://h/r", &parent, "c").unwrap();
        let other = d.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::remove_dir(t.path()).unwrap();
        std::fs::rename(&other, t.path()).unwrap();
        let e = format!("{:#}", t.finish(fail()).await.unwrap_err());
        assert!(e.contains("not the directory twr made"), "{e}");
        assert!(parent.join("c").is_dir());
    }

    /// A failed clone's directory is removed through the parent held open, not through its
    /// path: with the parent renamed and another directory put at its path (holding an entry
    /// of the target's name), the target, a partial `.git`, a read-only directory and a
    /// symbolic link in it, goes from the parent where it now is, and what is at the path
    /// stays as it is.
    #[tokio::test]
    async fn a_failed_clone_is_removed_in_its_parent_wherever_that_went() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let fail = || -> std::io::Result<ExitStatus> {
            use std::os::unix::process::ExitStatusExt;
            Ok(ExitStatus::from_raw(128 << 8))
        };
        let t = reserve_clone("https://h/r", &parent, "a").unwrap();
        std::fs::create_dir_all(t.path().join(".git/objects/pack")).unwrap();
        std::fs::write(t.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(t.path().join("src/ro")).unwrap();
        std::fs::write(t.path().join("src/ro/file"), "x").unwrap();
        std::fs::set_permissions(
            t.path().join("src/ro"),
            std::fs::Permissions::from_mode(0o500),
        )
        .unwrap();
        std::os::unix::fs::symlink("/", t.path().join("src/root")).unwrap();
        let moved = d.path().join("moved");
        std::fs::rename(&parent, &moved).unwrap();
        // Another directory at the parent's path, with its own `a` (and a `.git` in it).
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&parent)
            .unwrap();
        std::fs::create_dir_all(parent.join("a/.git")).unwrap();
        std::fs::write(parent.join("a/keep"), "x").unwrap();
        let e = format!("{:#}", t.finish(fail()).await.unwrap_err());
        assert!(e.contains("nothing was left"), "{e}");
        assert!(!moved.join("a").exists(), "the target was not removed");
        assert!(parent.join("a/keep").is_file() && parent.join("a/.git").is_dir());
    }

    /// Runs test `name` of this test binary again, alone, in a child process with `env` and
    /// umask `umask`: whether it passed, and what it printed.
    fn rerun(name: &str, env: &[(&str, &Path)], umask: libc::mode_t) -> (bool, String) {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new(std::env::current_exe().unwrap());
        c.args(["--exact", name, "--nocapture", "--test-threads=1"])
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        // SAFETY: umask(2) is async-signal-safe and cannot fail.
        unsafe {
            c.pre_exec(move || {
                libc::umask(umask);
                Ok(())
            });
        }
        let out = c.output().unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success() && text.contains("1 passed"), text)
    }

    /// Under a umask that leaves group write on (002), the target is still made 0700, where a
    /// plain `mkdir` makes it group-writable.
    #[test]
    fn the_target_is_0700_whatever_the_umask() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let (ok, out) = rerun(
            "repos::tests::umask_child",
            &[("TWR_TEST_UMASK_PARENT", &parent)],
            0o002,
        );
        assert!(ok, "{out}");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&parent.join("plain")),
            0o775,
            "the umask did not apply"
        );
        assert_eq!(mode(&parent.join("r")), 0o700);
    }

    /// [`the_target_is_0700_whatever_the_umask`]'s child (a no-op in a plain test run).
    #[test]
    fn umask_child() {
        let Some(parent) = std::env::var_os("TWR_TEST_UMASK_PARENT").map(PathBuf::from) else {
            return;
        };
        std::fs::create_dir(parent.join("plain")).unwrap();
        let t = reserve_clone("https://h/r", &parent, "r").unwrap();
        let meta = std::fs::symlink_metadata(t.path()).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o700);
        check_made(&Seen::from(&meta)).unwrap();
    }

    #[test]
    fn a_target_not_as_asked_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("x");
        std::fs::DirBuilder::new().mode(0o700).create(&p).unwrap();
        let meta = |p: &Path| Seen::from(&std::fs::symlink_metadata(p).unwrap());
        check_made(&meta(&p)).unwrap();
        for (mode, says) in [(0o770, "770"), (0o500, "500"), (0o701, "701")] {
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).unwrap();
            let e = check_made(&meta(&p)).unwrap_err();
            assert!(e.contains(says), "{e}");
        }
        std::os::unix::fs::symlink(&p, d.path().join("l")).unwrap();
        assert!(check_made(&meta(&d.path().join("l"))).is_err());
        std::fs::write(d.path().join("f"), "").unwrap();
        assert!(check_made(&meta(&d.path().join("f"))).is_err());
    }

    /// A clone git said it made is recorded only while it is still the directory twr made:
    /// one put in its place, or whose mode changed, is left as it is and not recorded.
    #[tokio::test]
    async fn a_clone_is_recorded_only_while_it_is_the_directory_made() {
        use std::os::unix::process::ExitStatusExt;
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        let ok = || -> std::io::Result<ExitStatus> { Ok(ExitStatus::from_raw(0)) };
        // Replaced by another repository.
        let t = reserve_clone("https://h/r", &parent, "a").unwrap();
        std::fs::remove_dir(t.path()).unwrap();
        let other = d.path().join("other");
        init_repo(&other);
        std::fs::rename(&other, t.path()).unwrap();
        let e = format!("{:#}", t.finish(ok()).await.unwrap_err());
        assert!(e.contains("not the directory twr made"), "{e}");
        assert!(e.contains("not recorded"), "{e}");
        assert!(parent.join("a/.git").exists());
        // The same directory, its mode changed.
        let t = reserve_clone("https://h/r", &parent, "b").unwrap();
        run(t.path(), &["init", "-q"]);
        std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o750)).unwrap();
        let e = format!("{:#}", t.finish(ok()).await.unwrap_err());
        assert!(e.contains("mode is 750"), "{e}");
        assert!(parent.join("b/.git").exists());
        // As made: recorded.
        let t = reserve_clone("https://h/r", &parent, "c").unwrap();
        run(t.path(), &["init", "-q"]);
        let path = t.finish(ok()).await.unwrap();
        assert_eq!(path, parent.join("c"));
    }

    // ---------------------------------------------------------------------------------------
    // run_group
    // ---------------------------------------------------------------------------------------

    /// Whether process `pid` runs (has not exited: a zombie has).
    #[cfg(target_os = "linux")]
    fn running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().next());
        !matches!(state, Some("Z" | "X" | "x") | None)
    }

    /// The pid a fake git wrote into `file`, once it has.
    #[cfg(target_os = "linux")]
    async fn written_pid(file: &Path) -> u32 {
        for _ in 0..2000 {
            if let Some(pid) = std::fs::read_to_string(file)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("{} was never written", file.display());
    }

    /// A fake git (`sh -c <script> <file>`) that starts a long-lived helper in the background
    /// and writes its pid into `file`, then does `then`.
    #[cfg(target_os = "linux")]
    fn fake_git(file: &Path, then: &str) -> tokio::process::Command {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c")
            .arg(format!(
                "sleep 300 </dev/null >/dev/null 2>&1 & echo $! > \"$0.tmp\" && mv \"$0.tmp\" \
                 \"$0\"; {then}"
            ))
            .arg(file)
            .stdin(Stdio::null());
        c
    }

    /// Stopped (SIGTERM to `--clone-repo`) or failed, git's whole process group is gone, a
    /// helper it left included, before `run_group` returns (and cleaning up starts); one that
    /// succeeded leaves its helpers (a credential cache daemon).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_stopped_or_failed_git_takes_its_helpers_with_it() {
        let d = tempfile::tempdir().unwrap();
        // Stopped while it and its helper run.
        let file = d.path().join("stopped");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let stop = async move {
            let _ = rx.await;
            libc::SIGTERM
        };
        let trigger = {
            let file = file.clone();
            async move {
                let pid = written_pid(&file).await;
                assert!(running(pid));
                tx.send(()).unwrap();
                pid
            }
        };
        let (status, helper) = tokio::join!(
            run_group(fake_git(&file, "wait"), stop, Duration::from_secs(5)),
            trigger
        );
        assert!(!status.unwrap().success());
        assert!(!running(helper), "the helper outlived its stopped git");
        // Failed on its own, its helper still running.
        let file = d.path().join("failed");
        let status = run_group(
            fake_git(&file, "exit 3"),
            std::future::pending(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(status.code(), Some(3));
        let helper = written_pid(&file).await;
        assert!(!running(helper), "the helper outlived its failed git");
        // Succeeded: its helper stays.
        let file = d.path().join("succeeded");
        let status = run_group(
            fake_git(&file, "exit 0"),
            std::future::pending(),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert!(status.success());
        let helper = written_pid(&file).await;
        assert!(running(helper));
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper as libc::pid_t, libc::SIGKILL) };
    }

    /// When `waitid` cannot be trusted or fails (injected): a git that exited 0 but whose status
    /// `waitid` does not vouch for (as on macOS) has its group killed all the same (fail
    /// closed), the status still the reap's (success); one whose wait fails while it runs, or
    /// after it ended on a stop, has its whole group killed and waited for before the reap (a
    /// helper that ignores SIGTERM included); with `ECHILD` (not this process's child any
    /// more) its group is not signalled, and the status is the reap's (an error when something
    /// else did reap it).
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn an_untrusted_or_failed_waitid_fails_closed() {
        let d = tempfile::tempdir().unwrap();
        let forever = || std::future::pending::<libc::c_int>();
        let grace = Duration::from_secs(5);
        // Exited 0, the status not vouched for: its helper is killed, success still reported.
        let file = d.path().join("unvouched");
        let unvouched = |pid| exited_unreaped(pid).map(|_| false);
        let status = run_group_with(fake_git(&file, "exit 0"), forever(), grace, unvouched)
            .await
            .unwrap();
        assert!(status.success(), "{status:?}");
        assert!(
            !running(written_pid(&file).await),
            "the helper outlived its git"
        );
        // The wait fails at once, git still running: its group is killed, git included.
        let file = d.path().join("failed-wait");
        let written = file.clone();
        let einval = move |_| {
            // (Once git started its helper: the group is what is killed.)
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            while !written.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::io::Error::from_raw_os_error(libc::EINVAL))
        };
        let start = std::time::Instant::now();
        let status = run_group_with(fake_git(&file, "wait"), forever(), grace, einval)
            .await
            .unwrap();
        assert_eq!(
            std::os::unix::process::ExitStatusExt::signal(&status),
            Some(libc::SIGKILL)
        );
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(
            !running(written_pid(&file).await),
            "the helper outlived its git"
        );
        // Stopped, git ends on its signal, then its wait fails: its group (a helper that
        // ignores SIGTERM) is killed.
        let file = d.path().join("stopped");
        let mut git = tokio::process::Command::new("sh");
        git.arg("-c")
            .arg(
                "(trap '' TERM HUP; exec sleep 300) </dev/null >/dev/null 2>&1 & echo $! > \
                 \"$0.tmp\" && mv \"$0.tmp\" \"$0\"; wait",
            )
            .arg(&file)
            .stdin(Stdio::null());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let stop = async move {
            let _ = rx.await;
            libc::SIGTERM
        };
        let after_exit = |pid| {
            exited_unreaped(pid)?;
            Err(std::io::Error::from_raw_os_error(libc::EIO))
        };
        let trigger = {
            let file = file.clone();
            async move {
                let pid = written_pid(&file).await;
                tx.send(()).unwrap();
                pid
            }
        };
        let (status, helper) = tokio::join!(run_group_with(git, stop, grace, after_exit), trigger);
        assert!(!status.unwrap().success());
        assert!(!running(helper), "the helper outlived its stopped git");
        // ECHILD: not signalled, the reap's status reported.
        let file = d.path().join("not-mine");
        let echild = |pid| {
            exited_unreaped(pid)?;
            Err(std::io::Error::from_raw_os_error(libc::ECHILD))
        };
        let status = run_group_with(fake_git(&file, "exit 3"), forever(), grace, echild)
            .await
            .unwrap();
        assert_eq!(status.code(), Some(3));
        let helper = written_pid(&file).await;
        assert!(
            running(helper),
            "a group no longer known to be its was signalled"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper as libc::pid_t, libc::SIGKILL) };
        // Reaped by something else first (for real): `waitid` and the reap fail with ECHILD,
        // the error is returned, and nothing signals the group (the helper runs on).
        let file = d.path().join("reaped-elsewhere");
        let reaped_elsewhere = |pid: u32| {
            let mut status = 0;
            // SAFETY: waitpid(2) of this test's child, with a valid pointer to `status`.
            let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
            assert_eq!(r, pid as libc::pid_t);
            exited_unreaped(pid)
        };
        let e = run_group_with(
            fake_git(&file, "exit 3"),
            forever(),
            grace,
            reaped_elsewhere,
        )
        .await
        .unwrap_err();
        assert!(twr_adapters::process::ownership_lost(&e), "{e}");
        let helper = written_pid(&file).await;
        assert!(
            running(helper),
            "the group of a git reaped elsewhere was signalled"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper as libc::pid_t, libc::SIGKILL) };
        // Reaped elsewhere, and a stop that comes once the wait has said so: the wait's answer
        // is seen first, and the stop signals nothing (the helper runs on).
        let file = d.path().join("reaped-then-stopped");
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let reaped_then_told = move |pid: u32| {
            let mut status = 0;
            // SAFETY: waitpid(2) of this test's child, with a valid pointer to `status`.
            let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
            assert_eq!(r, pid as libc::pid_t);
            let answer = exited_unreaped(pid);
            let _ = tx.send(());
            answer
        };
        let stop = async move {
            let _ = rx.await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            libc::SIGHUP
        };
        let e = run_group_with(fake_git(&file, "exit 3"), stop, grace, reaped_then_told)
            .await
            .unwrap_err();
        assert!(twr_adapters::process::ownership_lost(&e), "{e}");
        let helper = written_pid(&file).await;
        assert!(
            running(helper),
            "a stop signalled the group of a git reaped elsewhere"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper as libc::pid_t, libc::SIGKILL) };
    }

    /// A fake git (`sh -c <script> <pid file> <target>`) in the middle of a slow checkout: it
    /// writes files into the target, starts a helper (whose pid it writes into the pid file),
    /// and waits; with `stubborn`, it and its helper ignore SIGTERM and SIGHUP.
    #[cfg(target_os = "linux")]
    fn slow_checkout(pid_file: &Path, target: &Path, stubborn: bool) -> tokio::process::Command {
        let traps = if stubborn { "trap '' TERM HUP; " } else { "" };
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c")
            .arg(format!(
                "{traps}mkdir -p \"$1/.git/objects\" \"$1/src/deep\" && echo x > \
                 \"$1/src/deep/a.rs\" || exit 9; sleep 300 </dev/null >/dev/null 2>&1 & echo \
                 $! > \"$0.tmp\" && mv \"$0.tmp\" \"$0\"; wait"
            ))
            .arg(pid_file)
            .arg(target)
            .stdin(Stdio::null());
        c
    }

    /// Stopped during a slow checkout: git's group gets the signal (a hangup forwarded first)
    /// and SIGTERM; one that ends on it ends at once, one that ignores them is killed after the
    /// grace; either way every member is gone before `run_group` returns, and the target, with
    /// all git wrote in it, is removed.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_stopped_clone_ends_gracefully_or_is_killed_then_cleaned_up() {
        let d = tempfile::tempdir().unwrap();
        let parent = parent(d.path());
        for (name, stubborn, sig) in [
            ("polite", false, libc::SIGHUP),
            ("stubborn", true, libc::SIGTERM),
        ] {
            let t = reserve_clone("https://h/r", &parent, name).unwrap();
            let file = d.path().join(format!("{name}.pid"));
            let (tx, rx) = tokio::sync::oneshot::channel::<()>();
            let stop = async move {
                let _ = rx.await;
                sig
            };
            let trigger = {
                let (file, target) = (file.clone(), t.path().to_path_buf());
                async move {
                    let pid = written_pid(&file).await;
                    assert!(running(pid));
                    assert!(target.join("src/deep/a.rs").is_file());
                    tx.send(()).unwrap();
                    (pid, std::time::Instant::now())
                }
            };
            let grace = Duration::from_secs(2);
            let (status, (helper, stopped_at)) = tokio::join!(
                run_group(slow_checkout(&file, t.path(), stubborn), stop, grace),
                trigger
            );
            let took = stopped_at.elapsed();
            let status = status.unwrap();
            assert!(!status.success(), "{name}");
            assert!(
                !running(helper),
                "{name}: the helper outlived its stopped git"
            );
            if stubborn {
                assert!(took >= grace, "{name}: killed before its grace: {took:?}");
            } else {
                assert!(took < grace, "{name}: did not end on its signal: {took:?}");
            }
            let e = format!("{:#}", t.finish(Ok(status)).await.unwrap_err());
            assert!(e.contains("stopped by a signal"), "{name}: {e}");
            assert!(e.contains("nothing was left behind"), "{name}: {e}");
            assert!(!parent.join(name).exists(), "{name}");
        }
    }

    /// `twr runner --clone-repo` (in a child process: [`clone_child`]) hung up on (SIGHUP, the
    /// ssh connection dropped) while a fake git on its `PATH` checks out: git and its helper
    /// are stopped, the target removed, and it says why. A SIGTERM that comes right after the
    /// target is made (its handlers are in place before) does the same: the process is not
    /// killed by it, and leaves nothing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hangup_or_an_early_sigterm_stops_the_clone_and_cleans_up() {
        for mode in ["hup", "term-after-reserve"] {
            let d = tempfile::tempdir().unwrap();
            let dir = d.path().canonicalize().unwrap();
            let parent = parent(&dir);
            let bin = dir.join("bin");
            std::fs::create_dir(&bin).unwrap();
            let git = bin.join("git");
            std::fs::write(
                &git,
                "#!/bin/sh\nfor a; do t=$a; done\n\
                 mkdir -p \"$t/.git/objects\" \"$t/src\" && echo x > \"$t/src/a.rs\"\n\
                 sleep 300 </dev/null >/dev/null 2>&1 &\n\
                 echo \"$$ $!\" > \"$TWR_TEST_CLONE_CHILD/git.tmp\" && \
                 mv \"$TWR_TEST_CLONE_CHILD/git.tmp\" \"$TWR_TEST_CLONE_CHILD/git.pids\"\n\
                 wait\n",
            )
            .unwrap();
            std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let log = std::fs::File::create(dir.join("log")).unwrap();
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "repos::tests::clone_child", "--nocapture"])
                .arg("--test-threads=1")
                .env("TWR_TEST_CLONE_CHILD", &dir)
                .env("TWR_TEST_CLONE_MODE", mode)
                .env("PATH", path)
                .stdin(Stdio::null())
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap();
            let shown = || std::fs::read_to_string(dir.join("log")).unwrap_or_default();
            let deadline = std::time::Instant::now() + Duration::from_secs(60);
            let pids = dir.join("git.pids");
            let mut git_pids = Vec::new();
            if mode == "hup" {
                while !pids.exists() {
                    assert!(std::time::Instant::now() < deadline, "{}", shown());
                    std::thread::sleep(Duration::from_millis(20));
                }
                git_pids = std::fs::read_to_string(&pids)
                    .unwrap()
                    .split_whitespace()
                    .map(|p| p.parse::<u32>().unwrap())
                    .collect();
                assert!(git_pids.iter().all(|p| running(*p)));
                // SAFETY: kill(2) of the child this test started.
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) };
            }
            let status = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s;
                }
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("{mode}: it did not end: {}", shown());
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(status.success(), "{mode}: {status:?}: {}", shown());
            let said = std::fs::read_to_string(dir.join("result")).unwrap();
            let by = if mode == "hup" { "SIGHUP" } else { "SIGTERM" };
            assert!(said.contains(&format!("stopped by {by}")), "{mode}: {said}");
            assert!(said.contains("nothing was left behind"), "{mode}: {said}");
            assert!(!parent.join("r").exists(), "{mode}");
            for p in git_pids {
                assert!(!running(p), "{mode}: {p} outlived the hangup");
            }
        }
    }

    /// Set in [`clone_child`] to raise SIGTERM right after the target is made.
    pub(super) static RAISE_AFTER_RESERVE: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// Set in [`clone_child`] to move the parent away (to `<parent>.moved`) and make another
    /// directory at its path right before git starts ([`before_spawn`]).
    static MOVE_PARENT_BEFORE_SPAWN: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// [`clone_repo_with`]'s seam right before git starts: [`MOVE_PARENT_BEFORE_SPAWN`].
    pub(super) fn before_spawn(target: &CloneTarget) {
        if !MOVE_PARENT_BEFORE_SPAWN.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let parent = target.path().parent().unwrap();
        let mut moved = parent.as_os_str().to_owned();
        moved.push(".moved");
        std::fs::rename(parent, &moved).unwrap();
        std::fs::create_dir(parent).unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// The parent moved away and another directory made at its path after the target was made
    /// and before git starts (a member of a group that may write to a directory above it can
    /// do that): git clones into the target made, in the parent held open, nothing goes into
    /// the directory now at the parent's path, and the clone is left there, not recorded.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_parent_moved_before_git_runs_keeps_the_clone_unrecorded() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().canonicalize().unwrap();
        let parent = parent(&dir);
        let src = init_repo(&dir.join("src"));
        let bare = dir.join("bare.git");
        run(
            &dir,
            &[
                "clone",
                "-q",
                "--bare",
                src.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        let home = git_home(
            &dir,
            &format!(
                "[url \"file://{}\"]\n\tinsteadOf = https://h.example/o/r\n",
                bare.display()
            ),
        );
        let (ok, out) = rerun(
            "repos::tests::clone_child",
            &[
                ("TWR_TEST_CLONE_CHILD", &dir),
                ("TWR_TEST_CLONE_MODE", Path::new("move-parent")),
                ("HOME", &home),
                ("XDG_CONFIG_HOME", &home.join(".config")),
                (TEST_ALLOW_FILE_ENV, Path::new("1")),
            ],
            0o022,
        );
        assert!(ok, "{out}");
        let said = std::fs::read_to_string(dir.join("result")).unwrap();
        assert!(said.contains("it was moved"), "{said}");
        assert!(said.contains("not recorded"), "{said}");
        let moved = dir.join("parent.moved");
        assert!(moved.join("r/README.md").is_file(), "{said}");
        assert!(moved.join("r/.git").is_dir(), "{said}");
        assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0, "{said}");
    }

    /// [`a_hangup_or_an_early_sigterm_stops_the_clone_and_cleans_up`]'s and
    /// [`a_parent_moved_before_git_runs_keeps_the_clone_unrecorded`]'s child (a no-op in a
    /// plain test run): the clone, its error written into `result`.
    #[cfg(target_os = "linux")]
    #[test]
    fn clone_child() {
        let Some(dir) = std::env::var_os("TWR_TEST_CLONE_CHILD").map(PathBuf::from) else {
            return;
        };
        match std::env::var("TWR_TEST_CLONE_MODE").as_deref() {
            Ok("term-after-reserve") => {
                RAISE_AFTER_RESERVE.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok("move-parent") => {
                MOVE_PARENT_BEFORE_SPAWN.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            _ => {}
        }
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let e = rt
            .block_on(clone_repo_with(
                "https://h.example/o/r",
                &dir.join("parent"),
                "r",
                Duration::from_secs(5),
            ))
            .unwrap_err();
        std::fs::write(dir.join("result"), format!("{e:#}")).unwrap();
    }

    /// In a terminal (a pseudo-terminal `script` makes), git's process group is the terminal's
    /// foreground group while it runs: Ctrl-C typed there stops git (by SIGINT) and not this
    /// process, which has the terminal back afterwards.
    #[cfg(target_os = "linux")]
    #[test]
    fn git_has_the_terminal_and_ctrl_c_stops_it_alone() {
        use std::io::Write as _;
        if std::process::Command::new("script")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("no `script` here: skipped");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let line = format!(
            "{} --exact repos::tests::foreground_child --nocapture --test-threads=1",
            twr_core::paths::sh_word(exe.to_str().unwrap())
        );
        let log = std::fs::File::create(d.path().join("log")).unwrap();
        let mut script = std::process::Command::new("script")
            .args(["-qec", &line, "/dev/null"])
            .env("TWR_TEST_FG_DIR", d.path())
            .stdin(Stdio::piped())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut typed = script.stdin.take().unwrap();
        let shown = || std::fs::read_to_string(d.path().join("log")).unwrap_or_default();
        let stat = d.path().join("stat");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !stat.exists() {
            assert!(std::time::Instant::now() < deadline, "{}", shown());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        typed.write_all(b"\x03").unwrap();
        let status = loop {
            if let Some(s) = script.try_wait().unwrap() {
                break s;
            }
            if std::time::Instant::now() > deadline {
                let _ = script.kill();
                panic!("Ctrl-C did not end it: {}", shown());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        drop(typed);
        assert!(status.success(), "{}", shown());
        // git's process group (field 5 of its stat) was the terminal's foreground one (8).
        let stat = std::fs::read_to_string(&stat).unwrap();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        let (git_group, foreground) = (fields[2], fields[5]);
        assert_eq!(git_group, foreground, "{stat}");
        let out = std::fs::read_to_string(d.path().join("out")).unwrap();
        assert!(out.contains("signal=Some(2)"), "{out}");
        assert!(out.contains("back=true"), "{out}");
        assert!(!out.contains(&format!("me={git_group} ")), "{out}");
    }

    /// [`git_has_the_terminal_and_ctrl_c_stops_it_alone`]'s child, in the terminal (a no-op in
    /// a plain test run).
    #[cfg(target_os = "linux")]
    #[test]
    fn foreground_child() {
        use std::os::unix::process::ExitStatusExt;
        let Some(dir) = std::env::var_os("TWR_TEST_FG_DIR").map(PathBuf::from) else {
            return;
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c")
            .arg("cat /proc/$$/stat > \"$0.tmp\" && mv \"$0.tmp\" \"$0\"; exec sleep 30")
            .arg(dir.join("stat"));
        let status = rt
            .block_on(run_group(c, std::future::pending(), Duration::from_secs(5)))
            .unwrap();
        // SAFETY: tcgetpgrp and getpgrp only read.
        let (fg, me) = unsafe { (libc::tcgetpgrp(0), libc::getpgrp()) };
        std::fs::write(
            dir.join("out"),
            format!("signal={:?} back={} me={me} ", status.signal(), fg == me),
        )
        .unwrap();
    }

    // ---------------------------------------------------------------------------------------
    // --find-repo
    // ---------------------------------------------------------------------------------------

    fn repo_with_origin(dir: &Path, origin: Option<&str>) -> PathBuf {
        let p = init_repo(dir);
        if let Some(o) = origin {
            run(&p, &["remote", "add", "origin", o]);
        }
        p
    }

    /// A deadline no test reaches.
    fn later() -> tokio::time::Instant {
        tokio::time::Instant::now() + FIND_DEADLINE
    }

    async fn find(
        home: &Path,
        name: &str,
        origin: Option<&str>,
        hint: Option<&Path>,
    ) -> FindReport {
        find_repo(
            &LocalGit::default(),
            Some(home),
            name,
            origin,
            hint,
            later(),
        )
        .await
        .unwrap()
    }

    fn paths(r: &FindReport) -> Vec<(String, bool, Reason)> {
        r.candidates
            .iter()
            .map(|c| (c.path.clone(), c.matches_origin, c.reason))
            .collect()
    }

    #[tokio::test]
    async fn finds_the_hint_and_the_roots_origin_matches_first() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let s = |p: &Path| p.to_str().unwrap().to_string();
        // Under the roots: a match by name only, then one with the origin (another form of it).
        let code = repo_with_origin(&home.join("code/api"), Some("https://github.com/other/api"));
        let org = repo_with_origin(&home.join("src/org/api"), Some("git@github.com:o/api.git"));
        // Not a repository: not a candidate.
        std::fs::create_dir_all(home.join("projects/api")).unwrap();
        // Not a default root.
        repo_with_origin(
            &home.join("elsewhere/api"),
            Some("git@github.com:o/api.git"),
        );
        // The hint, with credentials in its origin (shown redacted, still matched).
        let hint = repo_with_origin(
            &home.join("work/api"),
            Some("https://user:hunter2@github.com/o/api"),
        );

        let r = find(
            &home,
            "api",
            Some("https://github.com/o/api.git"),
            Some(&hint),
        )
        .await;
        assert!(r.complete);
        assert_eq!(r.home.as_deref(), Some(s(&home).as_str()));
        assert_eq!(
            paths(&r),
            [
                (s(&hint), true, Reason::Hint),
                (s(&org), true, Reason::Name),
                (s(&code), false, Reason::Name),
            ]
        );
        assert!(r.candidates.iter().all(|c| c.toplevel));
        assert_eq!(
            r.candidates[0].origin.as_deref(),
            Some("https://***@github.com/o/api")
        );
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert!(json.contains("\"reason\":\"hint\""), "{json}");
        // No origin given: the hint first, then by name.
        let r = find(&home, "api", None, Some(&hint)).await;
        assert_eq!(
            paths(&r),
            [
                (s(&hint), false, Reason::Hint),
                (s(&code), false, Reason::Name),
                (s(&org), false, Reason::Name),
            ]
        );
        // A hint that is a root's candidate is listed once, as the hint.
        let r = find(&home, "api", None, Some(&code)).await;
        assert_eq!(
            paths(&r),
            [
                (s(&code), false, Reason::Hint),
                (s(&org), false, Reason::Name)
            ]
        );
        // A hint inside a repository: listed, not a top level, never a match; one outside any,
        // or missing: not listed.
        std::fs::create_dir_all(hint.join("sub")).unwrap();
        let r = find(
            &home,
            "api",
            Some("https://github.com/o/api"),
            Some(&hint.join("sub")),
        )
        .await;
        let sub = &r.candidates[r.candidates.len() - 1];
        assert_eq!((sub.toplevel, sub.matches_origin), (false, false));
        assert_eq!(sub.path, s(&hint.join("sub")));
        assert_eq!(r.candidates.len(), 3);
        let r = find(&home, "api", None, Some(&home.join("projects/api"))).await;
        assert_eq!(r.candidates.len(), 2);
        let r = find(&home, "api", None, Some(&home.join("nope"))).await;
        assert_eq!(r.candidates.len(), 2);
        // A symbolic link to a repository: its canonical path.
        std::os::unix::fs::symlink(&hint, home.join("ln")).unwrap();
        let r = find(&home, "api", None, Some(&home.join("ln"))).await;
        assert_eq!(r.candidates[0].path, s(&hint));
        // A repository without an origin.
        let none = repo_with_origin(&home.join("code/web"), None);
        let r = find(&home, "web", Some("https://github.com/o/web"), None).await;
        assert_eq!(paths(&r), [(s(&none), false, Reason::Name)]);
        assert_eq!(r.candidates[0].origin, None);
        // Nothing of the name.
        assert!(find(&home, "zzz", None, None).await.candidates.is_empty());
    }

    #[tokio::test]
    async fn find_refuses_bad_operands_and_says_when_it_is_incomplete() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let git = LocalGit::default();
        for name in ["", "..", "a/b", ".x", "-x"] {
            let e = find_repo(&git, Some(&home), name, None, None, later())
                .await
                .unwrap_err();
            assert!(
                format!("{e:#}").contains("not a folder name"),
                "{name:?}: {e:#}"
            );
        }
        for origin in [
            "file:///x",
            "ext::sh",
            "https://u:hunter2@h/r",
            "/srv/r",
            // A password with `@`, `:`, `?`, `#` or `/` in it: no part of it in the error.
            "me:hunter2@SEC2@host:repo",
            "https://u:hunter2:SEC2@SEC3@host/r",
            "ssh://git:hunter2@SEC2/SEC3@host/r",
            "me:hunter2?SEC2#SEC3@SEC4@host:r",
        ] {
            let e = find_repo(&git, Some(&home), "api", Some(origin), None, later())
                .await
                .unwrap_err();
            let e = format!("{e:#}");
            assert!(
                e.contains("--origin") && !e.contains("hunter2") && !e.contains("SEC"),
                "{e}"
            );
        }
        let e = find_repo(
            &git,
            Some(&home),
            "api",
            None,
            Some(Path::new("rel")),
            later(),
        )
        .await
        .unwrap_err();
        assert!(format!("{e:#}").contains("not an absolute path"));
        // No home: the hint alone, and the search is not complete (the roots were not
        // searched).
        let hint = init_repo(&home.join("work/api"));
        let r = find_repo(&git, None, "api", None, Some(&hint), later())
            .await
            .unwrap();
        assert_eq!((r.home, r.complete, r.candidates.len()), (None, false, 1));
        let r = find_repo(&git, None, "api", None, None, later())
            .await
            .unwrap();
        assert_eq!((r.complete, r.candidates.len()), (false, 0));
        std::fs::remove_dir_all(home.join("work")).unwrap();
        // Too many folders in a root: incomplete.
        let code = home.join("code");
        for i in 0..project_lookup::MAX_DIRS + 2 {
            std::fs::create_dir_all(code.join(format!("g{i:04}"))).unwrap();
        }
        let r = find(&home, "api", None, None).await;
        assert!(!r.complete);
    }

    /// A candidate's `origin` whose password holds `@` (and `:`, `?`, `#`, `/`) is shown
    /// redacted, matches nothing, and no part of it is in the report.
    #[tokio::test]
    async fn an_origin_with_a_password_holding_at_signs_is_never_shown() {
        for (origin, shown) in [
            ("me:SEC1@SEC2@host:repo", "me:***@host:repo"),
            (
                "https://u:SEC1:SEC2@SEC3@host/repo",
                "https://***@host/repo",
            ),
            (
                "ssh://git:SEC1/SEC2@SEC3@host/repo",
                "ssh://git:***@host/repo",
            ),
            ("me:SEC1?SEC2#SEC3@SEC4@host:repo", "me:***@host:repo"),
        ] {
            let d = tempfile::tempdir().unwrap();
            let home = d.path().canonicalize().unwrap();
            repo_with_origin(&home.join("code/api"), Some(origin));
            let r = find(&home, "api", Some("git@host:repo"), None).await;
            assert_eq!(r.candidates.len(), 1, "{origin}");
            let c = &r.candidates[0];
            assert_eq!(c.origin.as_deref(), Some(shown), "{origin}");
            assert!(!c.matches_origin, "{origin}");
            let json = serde_json::to_string(&r).unwrap();
            assert!(!json.contains("SEC"), "{json}");
        }
    }

    /// A repository whose `origin` git cannot read (a `remote.origin.url` without a value: `git
    /// rev-parse` still works) is listed as such, never "no origin" and never a match, and the
    /// report is not complete: it may have been the project's.
    #[tokio::test]
    async fn an_origin_git_cannot_read_makes_the_report_incomplete() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let wanted = "https://github.com/o/api";
        let good = repo_with_origin(&home.join("code/api"), Some(wanted));
        let bad = repo_with_origin(&home.join("src/api"), None);
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(bad.join(".git/config"))
            .unwrap();
        std::io::Write::write_all(&mut config, b"[remote \"origin\"]\n\turl\n").unwrap();
        let r = find(&home, "api", Some(wanted), None).await;
        assert!(!r.complete, "{r:?}");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let good_c = r.candidates.iter().find(|c| c.path == s(&good)).unwrap();
        assert!(good_c.matches_origin && !good_c.origin_unread);
        let bad_c = r.candidates.iter().find(|c| c.path == s(&bad)).unwrap();
        assert!(bad_c.toplevel, "{bad_c:?}");
        assert!(bad_c.origin_unread && bad_c.origin.is_none() && !bad_c.matches_origin);
        // Without it: one with no origin at all is not unread, and the report is complete.
        std::fs::remove_dir_all(&bad).unwrap();
        repo_with_origin(&bad, None);
        let r = find(&home, "api", Some(wanted), None).await;
        assert!(r.complete, "{r:?}");
        assert!(r.candidates.iter().all(|c| !c.origin_unread));
        // An older report (no `origin_unread`) still reads.
        let old: Candidate = serde_json::from_str(
            r#"{"path":"/h/api","toplevel":true,"origin":null,"matches_origin":false,"reason":"name"}"#,
        )
        .unwrap();
        assert!(!old.origin_unread);
    }

    /// Log lines, kept for a test to read.
    #[derive(Clone, Default)]
    struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// git fails on a repository whose config it quotes in its message, a URL with a password
    /// in it (an invalid `remote.origin.fetch` for `git remote get-url origin`, a bad
    /// `core.repositoryformatversion` for `git rev-parse`): the report is incomplete, and the
    /// password is neither in it nor in a log line.
    #[tokio::test]
    async fn git_s_messages_are_neither_reported_nor_logged() {
        let logs = Captured::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .finish();
        let _logs = tracing::subscriber::set_default(subscriber);
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let wanted = "https://github.com/o/api";
        repo_with_origin(&home.join("code/api"), Some(wanted));
        let fetch = repo_with_origin(&home.join("src/api"), Some(wanted));
        run(
            &fetch,
            &[
                "config",
                "remote.origin.fetch",
                "https://user:SECRET1@host/repo",
            ],
        );
        let rev = repo_with_origin(&home.join("projects/api"), Some(wanted));
        run(
            &rev,
            &[
                "config",
                "core.repositoryformatversion",
                "https://user:SECRET2@host/repo",
            ],
        );
        let r = find(&home, "api", Some(wanted), None).await;
        assert!(!r.complete, "{r:?}");
        let s = |p: &Path| p.to_str().unwrap().to_string();
        let c = r.candidates.iter().find(|c| c.path == s(&fetch)).unwrap();
        assert!(c.origin_unread && c.origin.is_none(), "{c:?}");
        // git could not tell whether it is a repository at all: not a candidate.
        assert!(!r.candidates.iter().any(|c| c.path == s(&rev)), "{r:?}");
        let json = serde_json::to_string(&r).unwrap();
        let logged = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
        assert!(logged.contains("get-url origin failed"), "{logged}");
        assert!(
            logged.contains("rev-parse --show-toplevel failed"),
            "{logged}"
        );
        for text in [&json, &logged] {
            assert!(!text.contains("SECRET"), "{text}");
        }
    }

    /// A huge `origin` is shown cut (it still matches, by all of it), and the hint with it fits
    /// the report; candidates that do not fit [`REPORT_MAX_BYTES`] are left out, the hint last,
    /// and the report is then incomplete. Paths are never cut.
    #[tokio::test]
    async fn the_report_fits_its_byte_budget() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let huge = format!("https://h.example/o/{}", "a".repeat(100_000));
        let hint = repo_with_origin(&home.join("work/api"), Some(&huge));
        let r = find(&home, "api", Some(&huge), Some(&hint)).await;
        assert!(r.complete, "a cut origin leaves nothing out");
        let [c] = r.candidates.as_slice() else {
            panic!("{:?}", r.candidates.len())
        };
        assert_eq!(c.path, hint.to_str().unwrap());
        assert!(c.matches_origin);
        let shown = c.origin.as_deref().unwrap();
        assert!(
            shown.ends_with("...") && shown.len() == ORIGIN_SHOWN_MAX + 3,
            "{shown}"
        );
        assert!(serde_json::to_vec(&r).unwrap().len() <= REPORT_MAX_BYTES);

        // Many with long origins and paths: the ones that match come first, the hint (which
        // does not match) last; what does not fit goes, the hint stays.
        let wanted = format!("https://h.example/o/{}", "b".repeat(2000));
        let org = "o".repeat(200);
        for i in 0..MAX_CANDIDATES - 1 {
            repo_with_origin(&home.join(format!("code/{org}{i:02}/api")), Some(&wanted));
        }
        let r = find(&home, "api", Some(&wanted), Some(&hint)).await;
        let size = serde_json::to_vec(&r).unwrap().len();
        assert!(size <= REPORT_MAX_BYTES, "{size}");
        assert!(!r.complete);
        assert!(
            r.candidates.len() < MAX_CANDIDATES,
            "{}",
            r.candidates.len()
        );
        let last = r.candidates.last().unwrap();
        assert_eq!(
            (last.reason, last.path.as_str()),
            (Reason::Hint, hint.to_str().unwrap())
        );
        assert!(r.candidates[..r.candidates.len() - 1]
            .iter()
            .all(|c| c.matches_origin && c.path.contains(&org)));
    }

    /// A git that hangs: the search stops at its deadline, with git killed, and says so.
    #[tokio::test]
    async fn a_search_stops_at_its_deadline() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        repo_with_origin(&home.join("code/api"), None);
        let slow = home.join("slow-git");
        std::fs::write(
            &slow,
            "#!/bin/sh\ncase $1 in --version) echo 'git version 2.40.0'; exit 0 ;; esac\n\
             exec sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = tokio::time::Instant::now();
        let r = find_repo(
            &LocalGit::with_bin(&slow),
            Some(&home),
            "api",
            None,
            None,
            started + Duration::from_secs(1),
        )
        .await
        .unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(10), "{took:?}");
        assert!(r.timed_out && !r.complete, "{r:?}");
        assert!(r.candidates.is_empty(), "{r:?}");
        // An older report (no `timed_out`, `git_missing`) still reads.
        let old: FindReport =
            serde_json::from_str(r#"{"home":null,"complete":true,"candidates":[]}"#).unwrap();
        assert!(!old.timed_out && !old.git_missing);
    }

    /// A folder found by its name whose `.git` git does not take for a repository (a `.git`
    /// file naming nothing) is no candidate, and the search is not complete; a box without git
    /// says so, lists nothing and is not complete either.
    #[tokio::test]
    async fn a_broken_repository_or_no_git_is_incomplete() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        let good = repo_with_origin(&home.join("code/api"), None);
        let r = find(&home, "api", None, None).await;
        assert!(r.complete && r.candidates.len() == 1, "{r:?}");
        std::fs::create_dir_all(home.join("src/api")).unwrap();
        std::fs::write(home.join("src/api/.git"), "gitdir: /nonexistent/x\n").unwrap();
        let r = find(&home, "api", None, None).await;
        assert!(!r.complete && !r.git_missing, "{r:?}");
        assert_eq!(r.candidates.len(), 1, "{r:?}");
        assert_eq!(r.candidates[0].path, good.to_str().unwrap());
        // No git.
        let none = LocalGit::with_bin(home.join("no-such-git"));
        let r = find_repo(&none, Some(&home), "api", None, Some(&good), later())
            .await
            .unwrap();
        assert!(
            r.git_missing && !r.complete && r.candidates.is_empty(),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn too_many_candidates_is_incomplete() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().canonicalize().unwrap();
        for i in 0..MAX_CANDIDATES + 1 {
            let p = home.join(format!("code/org{i:02}/api"));
            std::fs::create_dir_all(&p).unwrap();
            run(&p, &["init", "-q"]);
        }
        let r = find(&home, "api", None, None).await;
        assert!(!r.complete);
        assert_eq!(r.candidates.len(), MAX_CANDIDATES);
    }
}

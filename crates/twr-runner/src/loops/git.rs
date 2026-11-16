//! The git a loop reads, on the box its thread runs on (the hub's own, or a runner's through
//! `Op::LoopGit`, [`super::git_answer`]): what a scope covers, for the setup card (`branch · 7 commits, 23 files`), and the
//! base and baseline digest `loop.started` records.
//!
//! Every command goes through the box's own [`LocalGit::read`]: `git -c <SAFE_CONFIG> -C
//! <dir>` with no hooks, no fsmonitor, no transport, the repository's drivers turned off
//! where a command could run one, no prompt or pager, stdin closed, in a session and process
//! group of its own (`twr_adapters::process::new_session`, after
//! `twr_core::fork::prepare_fork`), output capped and killed after the read timeout: never a
//! `git` this module spawns itself. Refs a request names are checked before they reach git
//! ([`Scope`]'s parser refuses one that starts with `-` or holds `..`), and every ref is
//! passed as `<ref>^{commit}` after `--verify`, so nothing a user typed is read as an option.
//!
//! Each review round's diff (`diff-<slug>-<round>.patch`, [`round_diff`]) is built here too,
//! through the same [`LocalGit::read`]: the port of the skills' `collect-diff.sh`, from the
//! loop's recorded base instead of `HEAD`.
//!
//! An implementation loop's **baseline** ([`baseline`]) is read here as well: `HEAD`, the patch of
//! what the worktree already held ([`round_diff`] from `HEAD`) and, per path the worktree had
//! changed, a digest of its content ([`worktree_paths`]). The same digests, read again, say
//! which of those paths the loop has not touched since (a round's diff leaves them out,
//! [`unchanged_since`]) and what a commit left behind ([`repo_after`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::local::{GitRead, LocalGit};
use twr_core::loops::settings::Scope;
use twr_core::proto::{codes, RpcError};

use super::rpc;

/// Where a loop's thread works: what [`scope`] reads.
#[derive(Debug, Clone)]
pub struct Target {
    /// The thread's name (for messages).
    pub name: String,
    /// Its working directory (its worktree, or the repository it works in).
    pub dir: PathBuf,
    /// The ref its work merges back into (`thread.created` `ext.twr.base_ref`), if recorded.
    pub base_ref: Option<String>,
    /// Its repository's slug in the loop folder.
    pub slug: String,
}

/// What a scope covers, as git says now.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScopeInfo {
    /// The scope (the request's, else the default rule's).
    pub scope: Scope,
    /// The commit the diffs start from.
    pub base: String,
    /// `HEAD`.
    pub head: String,
    /// Commits from the base to `HEAD`.
    pub commits: u64,
    /// Files changed from the base to the working tree, untracked ones included.
    pub files: u64,
    /// The digest of the changes in the working tree beyond `HEAD` (`sha256:…`).
    pub dirty_digest: String,
    /// The working tree had no change beyond `HEAD`, untracked files included.
    pub clean: bool,
}

impl ScopeInfo {
    /// What the setup card says it covers: `branch · 7 commits, 23 files`, `uncommitted · 3
    /// files`.
    pub fn summary(&self) -> String {
        let plural = |n: u64, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        if self.scope != Scope::Uncommitted {
            parts.push(plural(self.commits, "commit"));
        }
        parts.push(plural(self.files, "file"));
        format!("{} · {}", self.scope, parts.join(", "))
    }
}

/// Runs `args` in `dir`; git's failure is an error naming `what`.
async fn read(git: &LocalGit, dir: &Path, what: &str, args: &[&str]) -> Result<GitRead, RpcError> {
    let out = git
        .read(dir, args)
        .await
        .map_err(|e| rpc(codes::INTERNAL, format!("cannot read {what}: {e:#}")))?;
    Ok(out)
}

/// The commit `rev` names in `dir`, or `None` when it names none.
async fn commit(git: &LocalGit, dir: &Path, rev: &str) -> Result<Option<String>, RpcError> {
    let spec = format!("{rev}^{{commit}}");
    let out = read(
        git,
        dir,
        rev,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .await?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok((out.ok && !text.is_empty()).then_some(text))
}

/// The NUL-separated names `args` prints.
async fn names(
    git: &LocalGit,
    dir: &Path,
    what: &str,
    args: &[&str],
) -> Result<Vec<Vec<u8>>, RpcError> {
    let out = read(git, dir, what, args).await?;
    if !out.ok {
        return Err(rpc(
            codes::INTERNAL,
            format!("cannot read {what}: {}", out.stderr.trim()),
        ));
    }
    Ok(out
        .stdout
        .split(|b| *b == 0)
        .filter(|n| !n.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// The commits from `from` to `head`.
async fn count(git: &LocalGit, dir: &Path, from: &str, head: &str) -> Result<u64, RpcError> {
    let range = format!("{from}..{head}");
    let out = read(git, dir, "the commits", &["rev-list", "--count", &range]).await?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0))
}

/// What `scope` (`None`: the default rule, `branch` when the thread has commits beyond its
/// base, else `uncommitted`) covers in `target`'s working directory: its base, `HEAD`, the
/// commits and files it reviews, and the digest of the working tree's changes. `unsupported`
/// when the directory is no git repository (or has no commit yet); `bad_request` for a scope
/// it cannot take (`branch` with no base recorded, a `since` ref or `last` count that names
/// no commit).
pub async fn scope(
    git: &LocalGit,
    target: &Target,
    scope: Option<Scope>,
) -> Result<ScopeInfo, RpcError> {
    let dir = target.dir.as_path();
    let name = &target.name;
    let Some(head) = commit(git, dir, "HEAD").await? else {
        return Err(rpc(
            codes::UNSUPPORTED,
            format!(
                "{name} has no git history to review in {} (no repository, or no commit yet)",
                dir.display()
            ),
        ));
    };
    // The branch's base: the merge-base of HEAD and the ref its work merges back into.
    let merge_base = match &target.base_ref {
        Some(base_ref) => match commit(git, dir, base_ref).await? {
            Some(base) => {
                let out = read(git, dir, "the merge-base", &["merge-base", &head, &base]).await?;
                let mb = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (out.ok && !mb.is_empty()).then_some(mb)
            }
            None => None,
        },
        None => None,
    };
    let own_commits = match &merge_base {
        Some(mb) => count(git, dir, mb, &head).await? > 0,
        None => false,
    };
    let scope = scope.unwrap_or(Scope::default_for(own_commits));
    let base = match &scope {
        Scope::Branch => merge_base.clone().ok_or_else(|| {
            rpc(
                codes::BAD_REQUEST,
                match &target.base_ref {
                    Some(r) => format!(
                        "{name}'s branch has no merge-base with {r}: review it with another \
                         scope (uncommitted, since <ref>, last <n>)"
                    ),
                    None => format!(
                        "{name} records no base branch (it works in its repository, or was made \
                         before twr recorded one): review it with another scope (uncommitted, \
                         since <ref>, last <n>)"
                    ),
                },
            )
        })?,
        Scope::Uncommitted => head.clone(),
        Scope::Since(r) => commit(git, dir, r).await?.ok_or_else(|| {
            rpc(
                codes::BAD_REQUEST,
                format!("since {r}: {r} names no commit in {name}'s repository"),
            )
        })?,
        Scope::Last(n) => commit(git, dir, &format!("HEAD~{n}"))
            .await?
            .ok_or_else(|| {
                rpc(
                    codes::BAD_REQUEST,
                    format!("last {n}: {name}'s branch has fewer than {n} commits"),
                )
            })?,
    };
    let commits = if base == head {
        0
    } else {
        count(git, dir, &base, &head).await?
    };
    // Tracked files changed from the base to the working tree, and untracked ones.
    let mut files = names(
        git,
        dir,
        "the changed files",
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            &base,
            "--",
        ],
    )
    .await?;
    let untracked = names(
        git,
        dir,
        "the untracked files",
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await?;
    files.extend(untracked.iter().cloned());
    files.sort();
    files.dedup();
    let (dirty_digest, clean) = dirty_digest(git, dir, &untracked).await?;
    Ok(ScopeInfo {
        scope,
        base,
        head,
        commits,
        files: files.len() as u64,
        dirty_digest,
        clean,
    })
}

/// The digest of the working tree's changes beyond `HEAD`: its binary diff (or, when that is
/// too large to read, its status) and the untracked files' names; and whether there are none.
async fn dirty_digest(
    git: &LocalGit,
    dir: &Path,
    untracked: &[Vec<u8>],
) -> Result<(String, bool), RpcError> {
    let mut buf = Vec::new();
    let mut clean = untracked.is_empty();
    match git
        .read(
            dir,
            &[
                "diff",
                "HEAD",
                "--binary",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--",
            ],
        )
        .await
    {
        Ok(out) if out.ok => {
            clean &= out.stdout.is_empty();
            buf.extend_from_slice(b"diff\0");
            buf.extend_from_slice(&out.stdout);
        }
        // Too large to read whole (or refused): the status stands in for it.
        _ => {
            let out = read(
                git,
                dir,
                "the working tree's status",
                &["status", "--porcelain=v2", "-z", "--untracked-files=no"],
            )
            .await?;
            clean &= out.stdout.is_empty();
            buf.extend_from_slice(b"status\0");
            buf.extend_from_slice(&out.stdout);
        }
    }
    buf.extend_from_slice(b"\0untracked\0");
    for n in untracked {
        buf.extend_from_slice(n);
        buf.push(0);
    }
    Ok((twr_core::blob::blob_ref(&buf), clean))
}

/// The most bytes of a round's diff the reviewer gets per repository; past it the patch is cut
/// at a line and says so, and the prompt says it loudly ([`RoundDiff::truncated`]).
pub const MAX_DIFF_BYTES: usize = 8 << 20;

/// The most untracked files a round's diff shows whole; the others are named.
pub const MAX_UNTRACKED: usize = 500;

/// A round's diff of one repository ([`round_diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundDiff {
    /// The patch, as the reviewer reads it.
    pub patch: Vec<u8>,
    /// `(kept, whole)` bytes when it was cut ([`MAX_DIFF_BYTES`]), or untracked files were
    /// left out ([`MAX_UNTRACKED`]: `whole` counts them as 0 bytes then).
    pub truncated: Option<(usize, usize)>,
}

/// The diff a review round reads of `repo` (the thread's worktree), from commit `base` to the
/// working tree, as the skills' `collect-diff.sh` prints one: a header naming the repository,
/// its branch, `HEAD` and the base; `git status --short`; every tracked change from `base` to
/// the working tree (committed, staged and not: `git diff <base>`); and every untracked file
/// (not ignored) whole, as an addition (`git diff --no-index /dev/null <file>`). Binary files
/// read `Binary files … differ`, never their bytes; a symbolic link is named with its target;
/// a directory git lists as untracked (an embedded repository) is named, not entered. Read-only
/// in git terms (`--no-optional-locks`, no external diff or textconv driver), and through
/// [`LocalGit::read`] only. Past [`MAX_DIFF_BYTES`] it is cut at a line. `Err`: a reason the
/// diff cannot be built (the worktree is gone, the base is no commit there, git failed).
///
/// `skip` names paths (relative to the repository's top) left out of the changes and of the
/// untracked files: an implementation loop's baseline paths the loop has not touched since
/// ([`unchanged_since`]). The header counts them. Past [`MAX_SKIPPED`] none is left out.
pub async fn round_diff(
    git: &LocalGit,
    repo: &Path,
    base: &str,
    skip: &[String],
) -> Result<RoundDiff, String> {
    let skip: &[String] = if skip.len() > MAX_SKIPPED { &[] } else { skip };
    use std::fmt::Write as _;
    if !repo.is_dir() {
        return Err(format!("{} is not there any more", repo.display()));
    }
    let fail = |what: &str, e: RpcError| format!("cannot read {what}: {}", e.message);
    let top = read(
        git,
        repo,
        "the repository",
        &["rev-parse", "--show-toplevel"],
    )
    .await
    .map_err(|e| fail("the repository", e))?;
    if !top.ok {
        return Err(format!(
            "{} is no git repository: {}",
            repo.display(),
            top.stderr.trim()
        ));
    }
    let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
    let dir = top.as_path();
    let base = match commit(git, dir, base)
        .await
        .map_err(|e| fail("the base", e))?
    {
        Some(b) => b,
        None => {
            return Err(format!(
                "the loop's base {base} is no commit in {}",
                dir.display()
            ))
        }
    };
    let head = commit(git, dir, "HEAD")
        .await
        .map_err(|e| fail("HEAD", e))?
        .unwrap_or_else(|| "(no commit)".into());
    let branch = read(
        git,
        dir,
        "the branch",
        &["symbolic-ref", "--short", "-q", "HEAD"],
    )
    .await
    .ok()
    .filter(|o| o.ok)
    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    .filter(|b| !b.is_empty())
    .unwrap_or_else(|| "(detached)".into());
    let mut out: Vec<u8> = Vec::new();
    let mut head_text = String::new();
    let _ = writeln!(
        head_text,
        "############################################################"
    );
    let _ = writeln!(head_text, "# REPO:   {}", dir.display());
    let _ = writeln!(head_text, "# BRANCH: {branch}");
    let _ = writeln!(head_text, "# HEAD:   {head}");
    let _ = writeln!(head_text, "# BASE:   {base}");
    if !skip.is_empty() {
        let _ = writeln!(
            head_text,
            "# LEFT OUT: {} path{} the worktree had changed before the loop began, untouched since",
            skip.len(),
            if skip.len() == 1 { "" } else { "s" }
        );
    }
    let _ = writeln!(
        head_text,
        "############################################################"
    );
    let _ = writeln!(head_text);
    let _ = writeln!(head_text, "===== git status --short =====");
    out.extend_from_slice(head_text.as_bytes());
    let status = read(
        git,
        dir,
        "the status",
        &[
            "--no-optional-locks",
            "status",
            "--short",
            "--untracked-files=normal",
        ],
    )
    .await
    .map_err(|e| fail("the status", e))?;
    out.extend_from_slice(&status.stdout);
    out.extend_from_slice(
        format!(
            "\n===== changes from the base {base} to the working tree (git diff {base}) =====\n"
        )
        .as_bytes(),
    );
    let excludes: Vec<String> = skip
        .iter()
        .map(|p| format!(":(exclude,literal){p}"))
        .collect();
    let mut args: Vec<&str> = vec![
        "--no-optional-locks",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        &base,
        "--",
    ];
    if !excludes.is_empty() {
        args.push(".");
        args.extend(excludes.iter().map(String::as_str));
    }
    let tracked = read(git, dir, "the changes", &args).await;
    let mut whole_extra = 0usize;
    match tracked {
        Ok(d) if d.ok => out.extend_from_slice(&d.stdout),
        Ok(d) => return Err(format!("git diff {base} failed: {}", d.stderr.trim())),
        // More than git's reads keep: said, and the rest is the reviewer's to read on disk.
        Err(e) => {
            out.extend_from_slice(
                format!(
                    "[twr: the diff of the tracked changes could not be read whole ({}): read \
                     `git diff {base}` in {} yourself]\n",
                    e.message,
                    dir.display()
                )
                .as_bytes(),
            );
            whole_extra = MAX_DIFF_BYTES + 1;
        }
    }
    out.extend_from_slice(b"\n===== new untracked files (full contents as unified diff) =====\n");
    let untracked = names(
        git,
        dir,
        "the untracked files",
        &["ls-files", "--others", "--exclude-standard", "-z"],
    )
    .await
    .map_err(|e| fail("the untracked files", e))?;
    let untracked: Vec<Vec<u8>> = untracked
        .into_iter()
        .filter(|n| !skip.iter().any(|p| p.as_bytes() == n.as_slice()))
        .collect();
    if untracked.is_empty() {
        out.extend_from_slice(b"(none)\n");
    }
    let mut left_out = 0usize;
    for (i, name) in untracked.iter().enumerate() {
        let shown = String::from_utf8_lossy(name).into_owned();
        out.push(b'\n');
        if i >= MAX_UNTRACKED {
            left_out += 1;
            out.extend_from_slice(format!("(untracked file not shown: {shown})\n").as_bytes());
            continue;
        }
        let path = dir.join(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(name));
        let meta = std::fs::symlink_metadata(&path);
        match meta {
            Ok(m) if m.file_type().is_symlink() => {
                let target = std::fs::read_link(&path)
                    .map(|t| t.display().to_string())
                    .unwrap_or_else(|_| "?".into());
                out.extend_from_slice(
                    format!("new untracked symlink: {shown} -> {target}\n").as_bytes(),
                );
                continue;
            }
            Ok(m) if m.is_dir() => {
                out.extend_from_slice(
                    format!("(embedded repository or directory {shown:?} NOT included)\n")
                        .as_bytes(),
                );
                continue;
            }
            Ok(_) => {}
            Err(_) => {
                out.extend_from_slice(format!("(untracked file gone: {shown})\n").as_bytes());
                continue;
            }
        }
        let Ok(rel) = std::str::from_utf8(name) else {
            out.extend_from_slice(
                format!("(untracked file with a name that is not UTF-8: {shown})\n").as_bytes(),
            );
            continue;
        };
        let d = read(
            git,
            dir,
            "an untracked file",
            &[
                "--no-optional-locks",
                "diff",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-index",
                "--",
                "/dev/null",
                rel,
            ],
        )
        .await;
        match d {
            // (`--no-index` exits 1 when the files differ, which they always do here.)
            Ok(d) if d.code == Some(0) || d.code == Some(1) => out.extend_from_slice(&d.stdout),
            Ok(d) => {
                return Err(format!(
                    "git diff --no-index failed for {shown}: {}",
                    d.stderr.trim()
                ))
            }
            Err(e) => out.extend_from_slice(
                format!(
                    "(untracked file {shown} could not be read whole: {})\n",
                    e.message
                )
                .as_bytes(),
            ),
        }
        if out.len() > MAX_DIFF_BYTES {
            break;
        }
    }
    let whole = out.len().max(whole_extra);
    let truncated = if out.len() > MAX_DIFF_BYTES {
        let mut cut = MAX_DIFF_BYTES;
        while cut > 0 && out[cut - 1] != b'\n' {
            cut -= 1;
        }
        out.truncate(cut);
        Some((cut, whole))
    } else if whole_extra > 0 || left_out > 0 {
        Some((out.len(), whole))
    } else {
        None
    };
    if let Some((kept, whole)) = truncated {
        out.extend_from_slice(
            format!(
                "\n[twr: THIS DIFF IS TRUNCATED: {kept} of {whole} bytes shown{}; read the rest \
                 from the repository on disk: git diff {base}]\n",
                if left_out > 0 {
                    format!(", {left_out} untracked files not shown")
                } else {
                    String::new()
                }
            )
            .as_bytes(),
        );
    }
    Ok(RoundDiff {
        patch: out,
        truncated,
    })
}

/// The most paths a round's diff leaves out ([`round_diff`]'s `skip`); past it, none.
pub const MAX_SKIPPED: usize = 1000;

/// The most changed paths [`worktree_paths`] digests; past it the rest are counted as changed
/// with no digest (never the same as the baseline's).
pub const MAX_PATHS: usize = 20_000;

/// Per path the worktree of `dir` has changed beyond `HEAD` (tracked changes, staged or not,
/// and untracked files, not ignored), relative to the repository's top: a digest of what is
/// there now (`None`: the path is gone). A symbolic link digests its target, a directory (a
/// submodule, an embedded repository) reads `dir`. Through [`LocalGit::read`] only; the
/// files themselves are read here.
pub async fn worktree_paths(
    git: &LocalGit,
    dir: &Path,
) -> Result<BTreeMap<String, Option<String>>, String> {
    let fail = |what: &str, e: RpcError| format!("cannot read {what}: {}", e.message);
    let top = read(
        git,
        dir,
        "the repository",
        &["rev-parse", "--show-toplevel"],
    )
    .await
    .map_err(|e| fail("the repository", e))?;
    if !top.ok {
        return Err(format!(
            "{} is no git repository: {}",
            dir.display(),
            top.stderr.trim()
        ));
    }
    let top = PathBuf::from(String::from_utf8_lossy(&top.stdout).trim());
    let out = read(
        git,
        &top,
        "the status",
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    )
    .await
    .map_err(|e| fail("the status", e))?;
    if !out.ok {
        return Err(format!("git status failed: {}", out.stderr.trim()));
    }
    let mut paths = BTreeMap::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| e.len() > 3) {
        let name = String::from_utf8_lossy(&entry[3..]).into_owned();
        if paths.len() >= MAX_PATHS {
            paths.insert(name, Some("unread".to_string()));
            continue;
        }
        let digest = content_digest(&top.join(&name));
        paths.insert(name, digest);
    }
    Ok(paths)
}

/// What a working tree holds, as far as git sees it: its `HEAD` and every changed or untracked
/// path with a digest of its content ([`worktree_paths`]). An ACP review run reads it before
/// and after the agent runs: a reviewer in a mode that is read-only by the agent's word alone
/// must leave it as it was ([`TreeState::changed_paths`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    /// `HEAD`, or `None` before the first commit.
    pub head: Option<String>,
    /// [`worktree_paths`] of the repository.
    pub paths: BTreeMap<String, Option<String>>,
}

impl TreeState {
    /// The paths that differ between `self` (before) and `after`, sorted, with `HEAD` named
    /// as such when it moved; empty when nothing changed.
    pub fn changed_paths(&self, after: &TreeState) -> Vec<String> {
        let mut changed: Vec<String> = Vec::new();
        if self.head != after.head {
            changed.push("HEAD".into());
        }
        let keys: std::collections::BTreeSet<&String> =
            self.paths.keys().chain(after.paths.keys()).collect();
        for k in keys {
            if self.paths.get(k) != after.paths.get(k) {
                changed.push(k.clone());
            }
        }
        changed
    }
}

/// The [`TreeState`] of the repository holding `dir`.
pub async fn tree_state(git: &LocalGit, dir: &Path) -> Result<TreeState, String> {
    let head = commit(git, dir, "HEAD")
        .await
        .map_err(|e| format!("cannot read HEAD: {}", e.message))?;
    let paths = worktree_paths(git, dir).await?;
    Ok(TreeState { head, paths })
}

/// A digest of what `path` holds ([`worktree_paths`]).
fn content_digest(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path).ok()?;
        return Some(format!("link:{}", target.display()));
    }
    if meta.is_dir() {
        return Some("dir".into());
    }
    match std::fs::read(path) {
        Ok(bytes) => Some(twr_core::blob::blob_ref(&bytes)),
        Err(_) => Some("unreadable".into()),
    }
}

/// The paths of `baseline` (a [`worktree_paths`] read when an implementation loop began) that
/// `now` still has as they were: the loop has not touched them.
pub fn unchanged_since(
    baseline: &BTreeMap<String, Option<String>>,
    now: &BTreeMap<String, Option<String>>,
) -> Vec<String> {
    baseline
        .iter()
        .filter(|(p, d)| now.get(*p) == Some(*d) && d.as_deref() != Some("unread"))
        .map(|(p, _)| p.clone())
        .collect()
}

/// An implementation loop's baseline of one repository: `HEAD`, the patch of what the
/// worktree already held, and the digests of the paths it had changed.
#[derive(Debug, Clone)]
pub struct Baseline {
    /// `HEAD`.
    pub head: String,
    /// The patch ([`round_diff`] from `HEAD`).
    pub patch: Vec<u8>,
    /// Per changed path, its content's digest ([`worktree_paths`]).
    pub paths: BTreeMap<String, Option<String>>,
}

impl Baseline {
    /// The patch's digest: `loop.started.baseline_digests`.
    pub fn digest(&self) -> String {
        twr_core::blob::blob_ref(&self.patch)
    }

    /// The baseline in a line, for the setup card and the reviewer's prompt: `clean`, or
    /// `3 paths already changed: a.rs, b.rs, c.rs`.
    pub fn summary(&self) -> String {
        baseline_summary(&self.paths)
    }
}

/// [`Baseline::summary`] of `paths`.
pub fn baseline_summary(paths: &BTreeMap<String, Option<String>>) -> String {
    if paths.is_empty() {
        return "clean".into();
    }
    let shown: Vec<&str> = paths.keys().take(5).map(String::as_str).collect();
    let more = paths.len().saturating_sub(shown.len());
    format!(
        "{} path{} already changed: {}{}",
        paths.len(),
        if paths.len() == 1 { "" } else { "s" },
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    )
}

/// The baseline of `target`'s worktree ([`Baseline`]): `unsupported` when it has no commit.
pub async fn baseline(git: &LocalGit, target: &Target) -> Result<Baseline, RpcError> {
    let dir = target.dir.as_path();
    let Some(head) = commit(git, dir, "HEAD").await? else {
        return Err(rpc(
            codes::UNSUPPORTED,
            format!(
                "{} has no git history in {} (no repository, or no commit yet): an \
                 implementation loop starts from a commit",
                target.name,
                dir.display()
            ),
        ));
    };
    let paths = worktree_paths(git, dir)
        .await
        .map_err(|e| rpc(codes::INTERNAL, e))?;
    let patch = round_diff(git, dir, &head, &[])
        .await
        .map_err(|e| rpc(codes::INTERNAL, e))?
        .patch;
    Ok(Baseline { head, patch, paths })
}

/// What a commit check finds of `repo` ([`twr_core::loops::contract::check_commit`]): `HEAD`
/// now, against `from`, whether its tree is `from`'s, the paths changed beyond `baseline` (the
/// paths the worktree had changed when the loop began, with their digests; empty for a
/// worktree that was clean), and the baseline's paths whose change is no longer in the
/// worktree: committed (`HEAD` holds it) or undone (`HEAD` holds what `from` did). A baseline
/// path whose content in `HEAD` is neither was changed further by the loop and committed with
/// its work. `work`: the loop left work there to commit.
pub async fn repo_after(
    git: &LocalGit,
    repo: &Path,
    from: Option<&str>,
    baseline: &BTreeMap<String, Option<String>>,
    work: bool,
) -> twr_core::loops::contract::RepoAfter {
    let shown = repo.display().to_string();
    let head = commit(git, repo, "HEAD").await.ok().flatten();
    let now = worktree_paths(git, repo).await;
    let left = match &now {
        Ok(now) => now
            .iter()
            .filter(|(p, d)| baseline.get(*p) != Some(*d))
            .map(|(p, _)| p.clone())
            .collect(),
        Err(e) => vec![format!("(the worktree could not be read: {e})")],
    };
    let same_tree = match (from, head.as_deref()) {
        (Some(f), Some(h)) if f != h => {
            let a = tree_of(git, repo, f).await;
            a.is_some() && a == tree_of(git, repo, h).await
        }
        _ => false,
    };
    let mut baseline_committed = Vec::new();
    let mut baseline_undone = Vec::new();
    if let (Ok(now), Some(head), Some(from)) = (&now, head.as_deref(), from) {
        let top = top_of(git, repo)
            .await
            .unwrap_or_else(|| repo.to_path_buf());
        for (path, was) in baseline {
            // Still changed in the worktree: as the baseline had it, or `left` names it.
            if now.contains_key(path) || was.as_deref() == Some("unread") {
                continue;
            }
            let Some(in_head) = digest_at(git, &top, head, path).await else {
                continue;
            };
            if in_head == *was {
                baseline_committed.push(path.clone());
            } else if digest_at(git, &top, from, path).await == Some(in_head) {
                baseline_undone.push(path.clone());
            }
        }
    }
    twr_core::loops::contract::RepoAfter {
        repo: shown,
        from: from.map(str::to_string),
        head,
        work,
        left,
        same_tree,
        baseline_committed,
        baseline_undone,
    }
}

/// The top of the repository `dir` is in.
async fn top_of(git: &LocalGit, dir: &Path) -> Option<PathBuf> {
    let out = read(
        git,
        dir,
        "the repository",
        &["rev-parse", "--show-toplevel"],
    )
    .await
    .ok()?;
    let top = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.ok && !top.is_empty()).then(|| PathBuf::from(top))
}

/// The tree of commit `rev` in `dir`.
async fn tree_of(git: &LocalGit, dir: &Path, rev: &str) -> Option<String> {
    let spec = format!("{rev}^{{tree}}");
    let out = read(
        git,
        dir,
        rev,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
    )
    .await
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.ok && !text.is_empty()).then_some(text)
}

/// The digest of `path` (relative to the repository's top `top`) as commit `rev` holds it, as
/// [`worktree_paths`] digests the worktree's: `Some(None)` when `rev` has no such path, `None`
/// when git could not say (a blob too large to read, say).
async fn digest_at(git: &LocalGit, top: &Path, rev: &str, path: &str) -> Option<Option<String>> {
    let out = read(
        git,
        top,
        "a committed path",
        &["ls-tree", "-z", "--full-tree", rev, "--", path],
    )
    .await
    .ok()?;
    if !out.ok {
        return None;
    }
    let entry = out.stdout.split(|b| *b == 0).find(|e| !e.is_empty());
    let Some(entry) = entry else {
        return Some(None);
    };
    let entry = String::from_utf8_lossy(entry);
    let (meta, _) = entry.split_once('\t')?;
    let mut parts = meta.split_whitespace();
    let (mode, kind, sha) = (parts.next()?, parts.next()?, parts.next()?);
    if kind != "blob" {
        return Some(Some("dir".into()));
    }
    let blob = read(git, top, "a committed file", &["cat-file", "blob", sha])
        .await
        .ok()
        .filter(|o| o.ok)?;
    Some(Some(if mode == "120000" {
        format!("link:{}", String::from_utf8_lossy(&blob.stdout))
    } else {
        twr_core::blob::blob_ref(&blob.stdout)
    }))
}

/// A repository slug for the loop folder (`diff-<slug>-<round>.patch`): `name`'s letters,
/// digits, `-`, `_` and `.`, others as `-`; `repo` when nothing is left.
pub fn slug(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .take(64)
        .collect();
    let s = s.trim_matches(|c| c == '-' || c == '.').to_string();
    if s.is_empty() {
        "repo".into()
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::testkit::git as tg;

    #[test]
    fn slugs() {
        assert_eq!(slug("api"), "api");
        assert_eq!(slug("my app/x"), "my-app-x");
        assert_eq!(slug(".."), "repo");
    }

    /// What each scope covers in a repository with a branch two commits beyond its base, a
    /// changed file and an untracked one.
    #[tokio::test]
    async fn scopes_cover_what_git_says() {
        let dir = tempfile::tempdir().unwrap();
        let repo = tg::init_repo(&dir.path().join("r"));
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        tg::commit_all(&repo, "base");
        tg::run(&repo, &["checkout", "-q", "-b", "work"]);
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        tg::commit_all(&repo, "one");
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        tg::commit_all(&repo, "two");
        std::fs::write(repo.join("a.txt"), "changed\n").unwrap();
        std::fs::write(repo.join("new.txt"), "new\n").unwrap();
        let git = LocalGit::default();
        let main = "main".to_string();
        let target = Target {
            name: "api".into(),
            dir: repo.clone(),
            base_ref: Some(main.clone()),
            slug: "api".into(),
        };
        let info = scope(&git, &target, None).await.unwrap();
        assert_eq!(info.scope, Scope::Branch);
        assert_eq!((info.commits, info.files), (2, 4));
        assert_eq!(info.summary(), "branch · 2 commits, 4 files");
        let u = scope(&git, &target, Some(Scope::Uncommitted))
            .await
            .unwrap();
        assert_eq!(u.base, u.head);
        assert_eq!(u.summary(), "uncommitted · 2 files");
        let l = scope(&git, &target, Some(Scope::Last(1))).await.unwrap();
        assert_eq!((l.commits, l.files), (1, 3));
        let s = scope(&git, &target, Some(Scope::Since(main.clone())))
            .await
            .unwrap();
        assert_eq!(s.base, info.base);
        // The digest follows the working tree.
        assert_eq!(u.dirty_digest, info.dirty_digest);
        std::fs::write(repo.join("a.txt"), "changed again\n").unwrap();
        let again = scope(&git, &target, None).await.unwrap();
        assert_ne!(again.dirty_digest, info.dirty_digest);
        // No base recorded: uncommitted by default, and no branch scope.
        let none = Target {
            base_ref: None,
            ..target.clone()
        };
        assert_eq!(
            scope(&git, &none, None).await.unwrap().scope,
            Scope::Uncommitted
        );
        let e = scope(&git, &none, Some(Scope::Branch)).await.unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST);
        let e = scope(&git, &target, Some(Scope::Last(9)))
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST);
        let e = scope(&git, &target, Some(Scope::Since("nope".into())))
            .await
            .unwrap_err();
        assert!(e.message.contains("names no commit"), "{e:?}");
        // No repository.
        let plain = Target {
            dir: dir.path().join("plain"),
            ..target
        };
        std::fs::create_dir_all(&plain.dir).unwrap();
        let e = scope(&git, &plain, None).await.unwrap_err();
        assert_eq!(e.code, codes::UNSUPPORTED);
    }
}

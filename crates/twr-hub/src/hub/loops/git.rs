//! The git a review loop reads, on the hub's own box only (milestone 1: no runner protocol
//! op): what a scope covers, for the setup card (`branch · 7 commits, 23 files`), and the
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

use std::path::{Path, PathBuf};

use twr_core::loops::settings::Scope;
use twr_core::proto::{codes, RpcError};
use twr_runner::local::{GitRead, LocalGit};

use super::super::rpc;

/// Where a loop's thread works: what [`scope`] reads.
#[derive(Debug, Clone)]
pub(in crate::hub) struct Target {
    /// The thread's name (for messages).
    pub(in crate::hub) name: String,
    /// Its working directory (its worktree, or the repository it works in).
    pub(in crate::hub) dir: PathBuf,
    /// The ref its work merges back into (`thread.created` `ext.twr.base_ref`), if recorded.
    pub(in crate::hub) base_ref: Option<String>,
    /// Its repository's slug in the loop folder.
    pub(in crate::hub) slug: String,
}

/// What a scope covers, as git says now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::hub) struct ScopeInfo {
    /// The scope (the request's, else the default rule's).
    pub(in crate::hub) scope: Scope,
    /// The commit the diffs start from.
    pub(in crate::hub) base: String,
    /// `HEAD`.
    pub(in crate::hub) head: String,
    /// Commits from the base to `HEAD`.
    pub(in crate::hub) commits: u64,
    /// Files changed from the base to the working tree, untracked ones included.
    pub(in crate::hub) files: u64,
    /// The digest of the changes in the working tree beyond `HEAD` (`sha256:…`).
    pub(in crate::hub) dirty_digest: String,
}

impl ScopeInfo {
    /// What the setup card says it covers: `branch · 7 commits, 23 files`, `uncommitted · 3
    /// files`.
    pub(in crate::hub) fn summary(&self) -> String {
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
pub(in crate::hub) async fn scope(
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
    let dirty_digest = dirty_digest(git, dir, &untracked).await?;
    Ok(ScopeInfo {
        scope,
        base,
        head,
        commits,
        files: files.len() as u64,
        dirty_digest,
    })
}

/// The digest of the working tree's changes beyond `HEAD`: its binary diff (or, when that is
/// too large to read, its status) and the untracked files' names.
async fn dirty_digest(
    git: &LocalGit,
    dir: &Path,
    untracked: &[Vec<u8>],
) -> Result<String, RpcError> {
    let mut buf = Vec::new();
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
            buf.extend_from_slice(b"status\0");
            buf.extend_from_slice(&out.stdout);
        }
    }
    buf.extend_from_slice(b"\0untracked\0");
    for n in untracked {
        buf.extend_from_slice(n);
        buf.push(0);
    }
    Ok(twr_core::blob::blob_ref(&buf))
}

/// The most bytes of a round's diff the reviewer gets per repository; past it the patch is cut
/// at a line and says so, and the prompt says it loudly ([`RoundDiff::truncated`]).
pub(in crate::hub) const MAX_DIFF_BYTES: usize = 8 << 20;

/// The most untracked files a round's diff shows whole; the others are named.
pub(in crate::hub) const MAX_UNTRACKED: usize = 500;

/// A round's diff of one repository ([`round_diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::hub) struct RoundDiff {
    /// The patch, as the reviewer reads it.
    pub(in crate::hub) patch: Vec<u8>,
    /// `(kept, whole)` bytes when it was cut ([`MAX_DIFF_BYTES`]), or untracked files were
    /// left out ([`MAX_UNTRACKED`]: `whole` counts them as 0 bytes then).
    pub(in crate::hub) truncated: Option<(usize, usize)>,
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
pub(in crate::hub) async fn round_diff(
    git: &LocalGit,
    repo: &Path,
    base: &str,
) -> Result<RoundDiff, String> {
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
    let tracked = read(
        git,
        dir,
        "the changes",
        &[
            "--no-optional-locks",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &base,
            "--",
        ],
    )
    .await;
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

/// A repository slug for the loop folder (`diff-<slug>-<round>.patch`): `name`'s letters,
/// digits, `-`, `_` and `.`, others as `-`; `repo` when nothing is left.
pub(in crate::hub) fn slug(name: &str) -> String {
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

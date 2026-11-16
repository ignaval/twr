//! Loops on a box (docs/design/loops.md, "Milestone 4: implementation notes › Remote boxes"):
//! what a loop does on the machine its thread runs on, shared by the hub's own box and every
//! runner, so the two differ only in transport.
//!
//! - [`git`]: a loop's git reads: what a scope covers, an implementation loop's baseline, the
//!   round diffs, the commit check's read of each repository.
//! - [`review`]: one review run (the reviewer's CLI, read-only), and [`acp_review`], one on an
//!   ACP preset's agent.
//! - [`folder`]: a runner's copy of a loop's folder (`<runner data>/loops/<id>/`), which the hub
//!   (re)creates as needed ([`twr_core::runner::Op::LoopFiles`]).
//! - [`runs`]: a runner's review runs, in the background ([`twr_core::runner::Op::LoopRun`]):
//!   each ends with its connection to the hub, and one a dead runner left behind is reaped
//!   when the next one opens the box.
//!
//! [`git_answer`] answers [`twr_core::runner::Op::LoopGit`] with the same functions the hub
//! calls for its own box.

pub mod acp_review;
pub mod folder;
pub mod git;
pub mod review;
pub mod runs;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use twr_core::proto::{codes, RpcError};
use twr_core::runner::{cmd_errors, CmdError, LoopGitQuery};

use crate::local::LocalGit;

/// An error with `code` and `message`.
pub(crate) fn rpc(code: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: code.into(),
        message: message.into(),
    }
}

/// `s` on one line, control characters removed and whitespace collapsed.
pub fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// An implementation loop's baseline on the wire ([`LoopGitQuery::Baseline`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WireBaseline {
    /// `HEAD`.
    pub head: String,
    /// The patch of what the worktree held (base64).
    pub patch: String,
    /// The paths it had changed, with their content's digests.
    pub paths: BTreeMap<String, Option<String>>,
}

/// A round's diff on the wire ([`LoopGitQuery::Diff`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WireDiff {
    /// The patch (base64), unless only its digest was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Its digest ([`twr_core::blob::blob_ref`]).
    pub digest: String,
    /// Its size in bytes.
    pub bytes: usize,
    /// `(kept, whole)` when it was cut ([`git::RoundDiff::truncated`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<(usize, usize)>,
}

/// The round diff of `repo` from `base` ([`git::round_diff`]), leaving out the paths of
/// `baseline` (an implementation loop's, with their digests) the loop has not touched since
/// ([`git::unchanged_since`]).
pub async fn loop_diff(
    git: &LocalGit,
    repo: &Path,
    base: &str,
    baseline: &BTreeMap<String, Option<String>>,
) -> Result<git::RoundDiff, String> {
    let skip = if baseline.is_empty() {
        Vec::new()
    } else {
        match git::worktree_paths(git, repo).await {
            Ok(now) => git::unchanged_since(baseline, &now),
            Err(_) => Vec::new(),
        }
    };
    git::round_diff(git, repo, base, &skip).await
}

/// A git error as a command's: its code (`unsupported`, `bad_request`, `internal`) kept, so
/// the hub gives the same refusal as for its own box.
fn git_err(e: RpcError) -> CmdError {
    CmdError::new(&e.code, e.message)
}

/// Answers [`LoopGitQuery`] `query` with the box's `git` (`loops`: where it keeps loop
/// folders, for a diff it saves).
pub async fn git_answer(
    git: &LocalGit,
    loops: &Path,
    query: LoopGitQuery,
) -> Result<Value, CmdError> {
    let to_value = |v: Result<Value, serde_json::Error>| {
        v.map_err(|e| CmdError::new(cmd_errors::FAILED, format!("cannot encode: {e}")))
    };
    match query {
        LoopGitQuery::Scope {
            dir,
            name,
            base_ref,
            scope,
        } => {
            let target = git::Target {
                name,
                dir,
                base_ref,
                slug: String::new(),
            };
            let info = git::scope(git, &target, scope).await.map_err(git_err)?;
            to_value(serde_json::to_value(info))
        }
        LoopGitQuery::Baseline { dir, name } => {
            let target = git::Target {
                name,
                dir,
                base_ref: None,
                slug: String::new(),
            };
            let b = git::baseline(git, &target).await.map_err(git_err)?;
            to_value(serde_json::to_value(WireBaseline {
                head: b.head,
                patch: twr_core::b64::encode(&b.patch),
                paths: b.paths,
            }))
        }
        LoopGitQuery::Diff {
            repo,
            base,
            baseline,
            save,
            digest_only,
        } => {
            let d = loop_diff(git, &repo, &base, &baseline)
                .await
                .map_err(|e| CmdError::new(codes::INTERNAL, e))?;
            if let Some(save) = save {
                folder::write_file(loops, &save.loop_id, &save.name, &d.patch)
                    .map_err(|e| CmdError::new(cmd_errors::FAILED, format!("{e:#}")))?;
            }
            to_value(serde_json::to_value(WireDiff {
                digest: twr_core::blob::blob_ref(&d.patch),
                bytes: d.patch.len(),
                truncated: d.truncated,
                patch: (!digest_only).then(|| twr_core::b64::encode(&d.patch)),
            }))
        }
        LoopGitQuery::After {
            repo,
            from,
            baseline,
            work,
        } => {
            let after = git::repo_after(git, &repo, from.as_deref(), &baseline, work).await;
            to_value(serde_json::to_value(after))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use twr_core::runner::LoopFileName;

    #[tokio::test]
    async fn git_answers_build_and_keep_a_round_diff() {
        let dir = tempfile::tempdir().unwrap();
        let repo = twr_core::testkit::git::init_repo(&dir.path().join("api"));
        std::fs::write(repo.join("new.rs"), "fn new() {}\n").unwrap();
        let git = LocalGit::default();
        let loops = dir.path().join("loops");
        let head = twr_core::testkit::git::run(&repo, &["rev-parse", "HEAD"]);
        let v = git_answer(
            &git,
            &loops,
            LoopGitQuery::Diff {
                repo: repo.clone(),
                base: head.clone(),
                baseline: BTreeMap::new(),
                save: Some(LoopFileName {
                    loop_id: "L1".into(),
                    name: "diff-api-m1.patch".into(),
                }),
                digest_only: false,
            },
        )
        .await
        .unwrap();
        let d: WireDiff = serde_json::from_value(v).unwrap();
        let patch = twr_core::b64::decode(d.patch.as_deref().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&patch).contains("fn new() {}"));
        assert_eq!(d.digest, twr_core::blob::blob_ref(&patch));
        assert_eq!(
            std::fs::read(loops.join("L1").join("diff-api-m1.patch")).unwrap(),
            patch
        );
        // Its digest only.
        let v = git_answer(
            &git,
            &loops,
            LoopGitQuery::Diff {
                repo: repo.clone(),
                base: head.clone(),
                baseline: BTreeMap::new(),
                save: None,
                digest_only: true,
            },
        )
        .await
        .unwrap();
        let d2: WireDiff = serde_json::from_value(v).unwrap();
        assert_eq!((d2.patch, d2.digest), (None, d.digest));
        // A scope, a baseline and a commit check's read, as on the hub's own box.
        let v = git_answer(
            &git,
            &loops,
            LoopGitQuery::Scope {
                dir: repo.clone(),
                name: "api".into(),
                base_ref: None,
                scope: None,
            },
        )
        .await
        .unwrap();
        let s: git::ScopeInfo = serde_json::from_value(v).unwrap();
        assert_eq!(s.head, head);
        assert!(!s.clean);
        let v = git_answer(
            &git,
            &loops,
            LoopGitQuery::Baseline {
                dir: repo.clone(),
                name: "api".into(),
            },
        )
        .await
        .unwrap();
        let b: WireBaseline = serde_json::from_value(v).unwrap();
        assert!(b.paths.contains_key("new.rs"));
        let v = git_answer(
            &git,
            &loops,
            LoopGitQuery::After {
                repo: repo.clone(),
                from: Some(head.clone()),
                baseline: b.paths.clone(),
                work: false,
            },
        )
        .await
        .unwrap();
        let a: twr_core::loops::contract::RepoAfter = serde_json::from_value(v).unwrap();
        assert!(a.left.is_empty(), "{a:?}");
        // A scope with no history to review keeps its code.
        let empty = dir.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let e = git_answer(
            &git,
            &loops,
            LoopGitQuery::Scope {
                dir: empty,
                name: "x".into(),
                base_ref: None,
                scope: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, codes::UNSUPPORTED);
        // A diff saved under a name that is no file of the folder is refused.
        let e = git_answer(
            &git,
            &loops,
            LoopGitQuery::Diff {
                repo,
                base: head,
                baseline: BTreeMap::new(),
                save: Some(LoopFileName {
                    loop_id: "L1".into(),
                    name: "../escape".into(),
                }),
                digest_only: true,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(e.code, cmd_errors::FAILED);
        assert!(!loops.join("escape").exists());
    }
}

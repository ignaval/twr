//! The loop's box (docs/design/loops.md, "Milestone 4: implementation notes › Remote boxes"):
//! everything a loop does on a machine goes through the box its thread runs on, [`Place`].
//! For the hub's own box that is its own git and its own loop folder; for a remote box it is
//! the `loop_*` runner ops, which run the same code there (`twr_runner::loops`). The two differ
//! only in transport.
//!
//! - **Git reads** (a scope, a baseline, a round diff, a commit check's read): the box's git.
//! - **The loop folder.** The hub's folder (`<data>/loops/<id>/`) is the record: answers,
//!   usage, logs, plans and the report are kept there, so `twr loop show`, `loop.get`,
//!   `loop.report` and the apps work with the box offline. A remote box has a copy at
//!   `<runner data>/loops/<id>/` ([`Remote::dir`]) that the thread and the reviewer read by its
//!   paths there; the hub writes what they read into it before they read it ([`Place::sync`])
//!   and reads a run's answer and log back into its own.
//! - **Review runs** run on the box with its own reviewer CLI and login ([`Place::review`]).
//!
//! A remote box that is away makes each of these a [`Trouble::Away`]: the driver counts a run
//! that was going as failed (`box <name> disconnected`) and starts nothing more on the box
//! until it is back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use twr_core::loops::contract::{parse_findings, RepoAfter};
use twr_core::proto::{codes, RpcError};
use twr_core::runner::{
    CmdError, LoopAcp, LoopFile, LoopFileName, LoopGitQuery, LoopRunEnd, LoopRunSpec,
    LoopRunStatus, Op, LOOPS_VERSION, LOOP_FILES_MAX,
};
use twr_runner::local::LocalGit;
use twr_runner::loops::{git, review, WireBaseline, WireDiff};

use super::super::{rpc, Inner, LOCAL_BOX};
use crate::remote::RemoteBox;

/// How long one wait for a run on a box lasts before the hub asks again.
const WAIT_SLICE_MS: u64 = 20_000;

/// The most bytes of a run's log read back from a box (its end).
const LOG_BACK_MAX: u64 = 8 << 20;

/// A file up to this size goes to a box with its data at once; a larger one is checked by its
/// digest first and sent only when the box lacks it.
const SEND_AT_ONCE: usize = 256 * 1024;

/// The box a loop's thread runs on.
#[derive(Clone)]
pub(in crate::hub) enum Place {
    /// The hub's own.
    Local(Arc<LocalGit>),
    /// A runner's.
    Remote(Remote),
}

/// A remote box that runs loops.
#[derive(Clone)]
pub(in crate::hub) struct Remote {
    /// The box.
    pub(in crate::hub) rbox: RemoteBox,
    /// Where it keeps loop folders (its inventory's).
    pub(in crate::hub) dir: PathBuf,
}

/// Why something could not be done on a loop's box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::hub) enum Trouble {
    /// The box is away (offline, or its connection dropped): `box <name> disconnected`.
    Away(String),
    /// It failed there.
    Failed(String),
}

impl Trouble {
    /// What it says.
    pub(in crate::hub) fn message(&self) -> &str {
        match self {
            Trouble::Away(m) | Trouble::Failed(m) => m,
        }
    }
}

/// `box <name> disconnected`: the reason of a run the box's going away ended.
pub(in crate::hub) fn disconnected(name: &str) -> String {
    format!("box {name} disconnected")
}

/// Why box `name`'s runner can't run loops (none in its inventory, or an older version), if
/// it can't: update it.
pub(in crate::hub) fn too_old(name: &str, version: &str) -> String {
    format!(
        "box {name} runs twr {version}, whose runner can't run loops: update twr on the box \
         (`twr box install {name}` for an SSH or container box; `twr update` there, then \
         restart the runner, for a dial-in one)"
    )
}

impl Place {
    /// The box `box_name` (a thread's), ready for a loop: the hub's own, or a remote box that
    /// is connected and runs loops. `box_offline` while it is away; `unsupported` when its
    /// runner is too old ([`too_old`]).
    pub(in crate::hub) fn of(hub: &Inner, box_name: &str) -> Result<Place, RpcError> {
        if box_name == LOCAL_BOX {
            return hub
                .host(LOCAL_BOX)
                .and_then(|h| h.local_git())
                .map(Place::Local)
                .ok_or_else(|| rpc(codes::UNSUPPORTED, "the hub's own box has no git"));
        }
        let rbox = hub
            .remote(box_name)
            .ok_or_else(|| rpc(codes::NOT_FOUND, format!("no box {box_name}")))?;
        let info = rbox.info();
        if !rbox.is_ready() || rbox.legacy().is_some() {
            return Err(rpc(
                codes::BOX_OFFLINE,
                format!("box {box_name} is offline (its runner is not connected)"),
            ));
        }
        let info = info.ok_or_else(|| {
            rpc(
                codes::BOX_OFFLINE,
                format!("box {box_name} is offline (its runner is not connected)"),
            )
        })?;
        match info.loops {
            Some(l) if l.version >= LOOPS_VERSION => Ok(Place::Remote(Remote { rbox, dir: l.dir })),
            _ => Err(rpc(codes::UNSUPPORTED, too_old(box_name, &info.version))),
        }
    }

    /// Whether `box_name` is away now: a remote box that is not connected and ready (`None`
    /// for the hub's own box, or a box the hub does not know).
    pub(in crate::hub) fn away(hub: &Inner, box_name: &str) -> bool {
        box_name != LOCAL_BOX
            && hub
                .remote(box_name)
                .is_some_and(|r| !r.is_ready() || r.legacy().is_some())
    }

    /// Where box `box_name` keeps loop folders, as its last inventory said (kept while it is
    /// away); `None` for the hub's own box or a runner that runs no loops.
    pub(in crate::hub) fn loops_dir(hub: &Inner, box_name: &str) -> Option<PathBuf> {
        if box_name == LOCAL_BOX {
            return None;
        }
        hub.remote(box_name)?.info()?.loops.map(|l| l.dir)
    }

    /// Whether it is a remote box.
    pub(in crate::hub) fn is_remote(&self) -> bool {
        matches!(self, Place::Remote(_))
    }

    /// Its name.
    pub(in crate::hub) fn name(&self) -> &str {
        match self {
            Place::Local(_) => LOCAL_BOX,
            Place::Remote(r) => r.rbox.name(),
        }
    }

    /// What a remote command's error is to a loop.
    fn trouble(&self, e: CmdError) -> Trouble {
        if e.code == codes::BOX_OFFLINE {
            Trouble::Away(disconnected(self.name()))
        } else {
            Trouble::Failed(e.message)
        }
    }

    /// Runs request `op` on the remote box `r`.
    async fn ask(&self, r: &Remote, op: Op) -> Result<Value, Trouble> {
        r.rbox.request(op).await.map_err(|e| self.trouble(e))
    }

    /// What `scope` covers in `target` ([`git::scope`]).
    pub(in crate::hub) async fn scope(
        &self,
        target: &git::Target,
        scope: Option<twr_core::loops::settings::Scope>,
    ) -> Result<git::ScopeInfo, RpcError> {
        match self {
            Place::Local(g) => git::scope(g, target, scope).await,
            Place::Remote(r) => {
                let v = r
                    .rbox
                    .request(Op::LoopGit {
                        query: LoopGitQuery::Scope {
                            dir: target.dir.clone(),
                            name: target.name.clone(),
                            base_ref: target.base_ref.clone(),
                            scope,
                        },
                    })
                    .await
                    .map_err(git_error)?;
                decode(v)
            }
        }
    }

    /// `target`'s baseline ([`git::baseline`]).
    pub(in crate::hub) async fn baseline(
        &self,
        target: &git::Target,
    ) -> Result<git::Baseline, RpcError> {
        match self {
            Place::Local(g) => git::baseline(g, target).await,
            Place::Remote(r) => {
                let v = r
                    .rbox
                    .request(Op::LoopGit {
                        query: LoopGitQuery::Baseline {
                            dir: target.dir.clone(),
                            name: target.name.clone(),
                        },
                    })
                    .await
                    .map_err(git_error)?;
                let b: WireBaseline = decode(v)?;
                let patch = twr_core::b64::decode(&b.patch).map_err(|e| {
                    rpc(
                        codes::INTERNAL,
                        format!("the box's baseline is no base64: {e}"),
                    )
                })?;
                Ok(git::Baseline {
                    head: b.head,
                    patch,
                    paths: b.paths,
                })
            }
        }
    }

    /// The round diff of `repo` from `base` without the `baseline` paths the loop has not
    /// touched since (`twr_runner::loops::loop_diff`); on a remote box also kept there as file
    /// `save` of loop `loop_id`'s folder. `digest_only`: only its digest is wanted (a remote
    /// box sends no patch).
    pub(in crate::hub) async fn diff(
        &self,
        repo: &Path,
        base: &str,
        baseline: &BTreeMap<String, Option<String>>,
        save: Option<(&str, &str)>,
        digest_only: bool,
    ) -> Result<Diff, Trouble> {
        match self {
            Place::Local(g) => {
                let d = twr_runner::loops::loop_diff(g, repo, base, baseline)
                    .await
                    .map_err(Trouble::Failed)?;
                Ok(Diff {
                    digest: twr_core::blob::blob_ref(&d.patch),
                    patch: Some(d.patch),
                    truncated: d.truncated,
                })
            }
            Place::Remote(r) => {
                let v = self
                    .ask(
                        r,
                        Op::LoopGit {
                            query: LoopGitQuery::Diff {
                                repo: repo.to_path_buf(),
                                base: base.to_string(),
                                baseline: baseline.clone(),
                                save: save.map(|(loop_id, name)| LoopFileName {
                                    loop_id: loop_id.into(),
                                    name: name.into(),
                                }),
                                digest_only,
                            },
                        },
                    )
                    .await?;
                let d: WireDiff = decode(v).map_err(|e| Trouble::Failed(e.message))?;
                let patch = match d.patch {
                    Some(p) => Some(twr_core::b64::decode(&p).map_err(|e| {
                        Trouble::Failed(format!("the box's diff is no base64: {e}"))
                    })?),
                    None => None,
                };
                if let Some(p) = &patch {
                    if twr_core::blob::blob_ref(p) != d.digest {
                        return Err(Trouble::Failed(
                            "the box's diff does not match its digest".into(),
                        ));
                    }
                }
                Ok(Diff {
                    patch,
                    digest: d.digest,
                    truncated: d.truncated,
                })
            }
        }
    }

    /// A commit check's read of `repo` ([`git::repo_after`]).
    pub(in crate::hub) async fn after(
        &self,
        repo: &Path,
        from: Option<&str>,
        baseline: &BTreeMap<String, Option<String>>,
        work: bool,
    ) -> Result<RepoAfter, Trouble> {
        match self {
            Place::Local(g) => Ok(git::repo_after(g, repo, from, baseline, work).await),
            Place::Remote(r) => {
                let v = self
                    .ask(
                        r,
                        Op::LoopGit {
                            query: LoopGitQuery::After {
                                repo: repo.to_path_buf(),
                                from: from.map(str::to_string),
                                baseline: baseline.clone(),
                                work,
                            },
                        },
                    )
                    .await?;
                decode(v).map_err(|e| Trouble::Failed(e.message))
            }
        }
    }

    /// Where loop `loop_id`'s thread and reviewer read its folder: the hub's own `record` on
    /// the hub's box, the box's copy on a remote one.
    pub(in crate::hub) fn folder(&self, record: &Path, loop_id: &str) -> PathBuf {
        match self {
            Place::Local(_) => record.to_path_buf(),
            Place::Remote(r) => r.dir.join(loop_id),
        }
    }

    /// Writes files `names` of the hub's folder `record` into loop `loop_id`'s folder on a
    /// remote box (nothing on the hub's own): each small one with its data, each larger one
    /// only when the box lacks it with that digest (a box that lost its folder gets it all
    /// again). A name the hub's folder lacks is skipped.
    pub(in crate::hub) async fn sync(
        &self,
        loop_id: &str,
        record: &Path,
        names: &[String],
    ) -> Result<(), Trouble> {
        let Place::Remote(r) = self else {
            return Ok(());
        };
        let mut first = Vec::new();
        let mut big: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for n in names {
            let Ok(data) = std::fs::read(record.join(n)) else {
                continue;
            };
            let digest = twr_core::blob::blob_ref(&data);
            if data.len() <= SEND_AT_ONCE {
                first.push(LoopFile {
                    name: n.clone(),
                    digest,
                    data: Some(twr_core::b64::encode(&data)),
                });
            } else {
                first.push(LoopFile {
                    name: n.clone(),
                    digest,
                    data: None,
                });
                big.insert(n.clone(), data);
            }
        }
        let missing = self.put(r, loop_id, first).await?;
        let mut batch = Vec::new();
        let mut size = 0;
        for n in missing {
            let Some(data) = big.remove(&n) else {
                continue;
            };
            if size + data.len() > LOOP_FILES_MAX && !batch.is_empty() {
                self.put(r, loop_id, std::mem::take(&mut batch)).await?;
                size = 0;
            }
            size += data.len();
            batch.push(LoopFile {
                name: n,
                digest: twr_core::blob::blob_ref(&data),
                data: Some(twr_core::b64::encode(&data)),
            });
        }
        if !batch.is_empty() {
            self.put(r, loop_id, batch).await?;
        }
        Ok(())
    }

    /// `loop_files` with `files`: the names the box lacks.
    async fn put(
        &self,
        r: &Remote,
        loop_id: &str,
        files: Vec<LoopFile>,
    ) -> Result<Vec<String>, Trouble> {
        let v = self
            .ask(
                r,
                Op::LoopFiles {
                    loop_id: loop_id.to_string(),
                    files,
                    read: Vec::new(),
                    max: 0,
                },
            )
            .await?;
        Ok(v["missing"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Reads files `names` of loop `loop_id`'s folder on a remote box back into the hub's
    /// `record` (each at most `max` bytes, its end); nothing on the hub's own box. A file the
    /// box lacks is left as the hub has it.
    pub(in crate::hub) async fn fetch(
        &self,
        loop_id: &str,
        record: &Path,
        names: &[String],
        max: u64,
    ) -> Result<(), Trouble> {
        let Place::Remote(r) = self else {
            return Ok(());
        };
        let v = self
            .ask(
                r,
                Op::LoopFiles {
                    loop_id: loop_id.to_string(),
                    files: Vec::new(),
                    read: names.to_vec(),
                    max,
                },
            )
            .await?;
        for f in v["read"].as_array().into_iter().flatten() {
            let (Some(name), Some(data)) = (f["name"].as_str(), f["data"].as_str()) else {
                continue;
            };
            if !names.iter().any(|n| n == name) {
                continue;
            }
            let Ok(mut bytes) = twr_core::b64::decode(data) else {
                continue;
            };
            if f["truncated"] == true {
                let mut head = format!(
                    "[twr: the first {} bytes of this file stayed on box {}]\n",
                    f["bytes"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(bytes.len() as u64),
                    self.name()
                )
                .into_bytes();
                head.append(&mut bytes);
                bytes = head;
            }
            let _ = std::fs::write(record.join(name), bytes);
        }
        Ok(())
    }

    /// Ends loop `loop_id`'s runs on a remote box and removes its folder there (nothing on the
    /// hub's own box, whose folder is the record). Best effort.
    pub(in crate::hub) async fn forget(&self, loop_id: &str) {
        if let Place::Remote(r) = self {
            let _ = r
                .rbox
                .request(Op::LoopForget {
                    loop_id: loop_id.to_string(),
                })
                .await;
        }
    }

    /// Runs review run `run` (`call`, its paths the box's) of loop `loop_id` on the box, with
    /// the box's own reviewer CLI and login, and, on a remote box, reads its answer and log
    /// back into the hub's `record`; `spawned` is told the process group of a run on the hub's
    /// own box. A box that goes away ends it as a failed run (`box <name> disconnected`).
    pub(in crate::hub) async fn review(
        &self,
        loop_id: &str,
        run: &str,
        call: review::Call,
        record: &Path,
        spawned: impl FnOnce(u32),
        mut cancel: tokio::sync::watch::Receiver<Option<String>>,
    ) -> review::Ended {
        let Place::Remote(r) = self else {
            return review::run(call, spawned, cancel).await;
        };
        let acp = match call.acp.clone() {
            None => None,
            Some(Ok(a)) => Some(LoopAcp {
                preset: a.preset,
                argv: a.argv,
                mode: a.mode,
                efforts: a.efforts,
            }),
            Some(Err(why)) => return review::Ended::Failed(why),
        };
        let stem = call
            .answer_file
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".answer.json"))
            .unwrap_or_default()
            .to_string();
        let spec = LoopRunSpec {
            runtime: call.runtime.clone(),
            model: call.model.clone(),
            effort: call.effort.clone(),
            cwd: call.cwd.clone(),
            stem: stem.clone(),
            marker: call.marker.clone(),
            timeout_secs: call.timeout.as_secs(),
            acp,
        };
        let back = [format!("{stem}.log")];
        let status = self
            .ask(
                r,
                Op::LoopRun {
                    loop_id: loop_id.to_string(),
                    run: run.to_string(),
                    spec,
                },
            )
            .await;
        let mut status: LoopRunStatus = match status.map(decode) {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return review::Ended::Failed(e.message),
            Err(t) => return review::Ended::Failed(t.message().to_string()),
        };
        let cancelled = loop {
            if status.unknown {
                return review::Ended::Failed(format!(
                    "box {} lost the run (its runner restarted)",
                    self.name()
                ));
            }
            if let Some(end) = status.ended.take() {
                let _ = self.fetch(loop_id, record, &back, LOG_BACK_MAX).await;
                return ended(end, &call, record);
            }
            let wait = self.ask(
                r,
                Op::LoopRunWait {
                    run: run.to_string(),
                    wait_ms: WAIT_SLICE_MS,
                },
            );
            let why = tokio::select! {
                v = wait => {
                    match v.map(decode::<LoopRunStatus>) {
                        Ok(Ok(s)) => { status = s; continue; }
                        Ok(Err(e)) => return review::Ended::Failed(e.message),
                        Err(Trouble::Away(why)) => return review::Ended::Failed(why),
                        Err(Trouble::Failed(why)) => {
                            // (The box did not answer: end the run there, then count it failed.)
                            let _ = r.rbox.request(Op::LoopRunCancel { run: run.to_string(), why: why.clone() }).await;
                            return review::Ended::Failed(why);
                        }
                    }
                }
                why = cancelled(&mut cancel) => why,
            };
            break why;
        };
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            r.rbox.request(Op::LoopRunCancel {
                run: run.to_string(),
                why: cancelled.clone(),
            }),
        )
        .await;
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            self.fetch(loop_id, record, &back, LOG_BACK_MAX),
        )
        .await;
        review::Ended::Cancelled(cancelled)
    }
}

/// The reason `cancel` holds once it holds one.
async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<Option<String>>) -> String {
    loop {
        if let Some(why) = cancel.borrow().clone() {
            return why;
        }
        if cancel.changed().await.is_err() {
            return "the hub let go of it".to_string();
        }
    }
}

/// How a run on a remote box ended, as the hub's own box's run would say it: its answer read
/// by the hub ([`parse_findings`]) and written into the hub's `record` as the run's answer
/// file.
fn ended(end: LoopRunEnd, call: &review::Call, record: &Path) -> review::Ended {
    match end {
        LoopRunEnd::Answered { answer, usage } => {
            if let Some(name) = call.answer_file.file_name() {
                let _ = std::fs::write(record.join(name), &answer);
            }
            match parse_findings(&answer) {
                Ok(findings) => review::Ended::Answered { findings, usage },
                Err(e) => review::Ended::Failed(e.to_string()),
            }
        }
        LoopRunEnd::Failed { reason } => review::Ended::Failed(reason),
        LoopRunEnd::Cancelled { reason } => review::Ended::Cancelled(reason),
    }
}

/// A round diff as the hub keeps it ([`Place::diff`]).
#[derive(Debug, Clone)]
pub(in crate::hub) struct Diff {
    /// The patch (none when only its digest was asked of a remote box).
    pub(in crate::hub) patch: Option<Vec<u8>>,
    /// Its digest.
    pub(in crate::hub) digest: String,
    /// `(kept, whole)` when it was cut.
    pub(in crate::hub) truncated: Option<(usize, usize)>,
}

/// A remote git read's error as the hub's own box's would be: its code kept (a box that is
/// away: `box_offline`; a runner's own failure: `internal`).
fn git_error(e: CmdError) -> RpcError {
    let code = match e.code.as_str() {
        codes::BOX_OFFLINE | codes::UNSUPPORTED | codes::BAD_REQUEST | codes::INTERNAL => e.code,
        _ => codes::INTERNAL.to_string(),
    };
    rpc(&code, e.message)
}

/// `v` as a `T`, or an `internal` error naming what did not decode.
fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| {
        rpc(
            codes::INTERNAL,
            format!("malformed answer from the box: {e}"),
        )
    })
}

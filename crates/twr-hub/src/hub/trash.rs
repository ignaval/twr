//! The trash: where deleting a thread moves its worktree, its administrative record and its
//! branch (`hub::delete`, [`GitOps::worktree_trash`] on the thread's box), until it is purged
//! or the thread is put back.
//!
//! - `trash.list` (users and the orchestrator): every item of every box that is online, from
//!   the boxes' own trash directories (their manifests), merged with what the log says of the
//!   items of boxes that are not (`thread.deleted` `trash`, shown with `box_offline`). Each
//!   with its thread, project, box, when it was deleted and when it is purged, its size on disk
//!   and what the delete card said it loses.
//! - `trash.empty {items?, all?}` (users only): purges the items named (by id, a unique end of
//!   one of at least 4 characters, or the thread's name when one item has it), or every item of
//!   every online box: their refs (`refs/twr-trash/<item>/`) and directories go. With
//!   `dry_run`, it purges nothing and says which items it would purge
//!   ([`Hub::trash_empty_preview`]): a client that asks the user first (`twr trash empty`, the
//!   TUI's `/trash empty`) shows those and then purges exactly them, by id, never `all` or a
//!   name again, so the answer covers only what it showed.
//! - `trash.restore {item, as?}` (users only): brings the thread back from an item on its box,
//!   copying, never moving or changing the item: its branch is made again from the item's
//!   refs, git adds a worktree on it (at its old path or, when that is taken, a new one) and the
//!   saved files are copied into it; then `thread.restored` is recorded: the thread is back in
//!   every listing as it was when deleted, parked with its session (resuming it resumes the
//!   session), under its name or `as` (needed when a thread took the name meanwhile, or its
//!   branch exists). Its earlier events never left the log, so its history is back with it.
//!   Staged changes come back as unstaged ones, and a `HEAD` that was detached or on another
//!   branch is not brought back (its commit stays in the item's refs; the result says so).
//!   When `thread.restored` cannot be recorded, the box removes what it made (the item is as
//!   it was), so the restore can be tried again. The item stays in the trash, listed as
//!   restored, until it is purged or emptied; it is not restored twice.
//! - The purge: items deleted more than `[trash] keep_days` ago go, on every online box, once
//!   right after the hub starts and then every [`TRASH_TICK`] ([`Hub::purge_trash`]). The hub
//!   drives it (a runner does not know the hub's config); a box offline then is purged at the
//!   next tick it is online. Every item purged, here or by `trash.empty`, is recorded
//!   (`trash.purged {item, thread}` on the hub's thread), and so is an item of the hub's own
//!   box the log knows that is past its purge time and whose directory is not there
//!   (`gone: true`: purged before purges were recorded, or removed by hand; an item the box
//!   merely could not list is not). A remote box's item is recorded purged only from that
//!   box's own answer to the purge, for the ids it was asked about. Known limitation: when
//!   that answer is lost (a crash, a failed append), nothing is recorded: the item is still
//!   listed for the box while it is offline, and its thread's agent events are not archived
//!   as a purged thread's (only the per-turn retention applies). A deleted thread whose item is
//!   purged is never restored, and comes back only from the item of its latest deletion; its
//!   agent events are archived (`hub::prune`).
//! - Reconciliation ([`Hub::reconcile_boxes`]): when the hub starts, when a runner's box comes
//!   online and before every purge, what a crash or a failed append left the log and a box
//!   disagreeing on is settled (what an unrecorded restore made removed, a deletion recorded).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{TimeZone, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::git::{
    human_bytes, thread_branch, valid_branch_component, valid_trash_id, RestoreRequest, TrashItem,
    TrashPurge, TRASH_BIG_BYTES,
};
use twr_core::proto::{codes, RpcError};

use super::git::Workspace;
use super::{rpc, Caller, Core, Hub, HubState, Inner, LOCAL_BOX};
use crate::host::ThreadHost;

/// How often the hub purges expired trash items.
pub const TRASH_TICK: Duration = Duration::from_secs(3600);

/// How many fresh worktree paths a restore tries when the old one is taken
/// (`<name>-2` .. `<name>-<N>`).
const FRESH_PATHS: usize = 9;

/// A trash item as the log knows it (`thread.deleted` `trash`): shown for a box that is not
/// online.
#[derive(PartialEq, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Seen {
    pub(super) thread: String,
    pub(super) name: String,
    pub(super) project: Option<String>,
    pub(super) box_name: String,
    pub(super) deleted_at_ms: u64,
    pub(super) bytes: u64,
    pub(super) losses: Option<String>,
    pub(super) worktree: Option<String>,
    pub(super) branch: Option<String>,
    pub(super) dir: Option<String>,
}

/// A trash item a `thread.restored` named (from the log): the item stays in the trash, listed
/// as restored, until it is purged, and is not restored again.
#[derive(PartialEq, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct Restored {
    /// The name the thread was restored under.
    pub(super) name: String,
    /// When (ms since the epoch, the hub's clock).
    pub(super) at_ms: u64,
}

/// What a deleted thread's runtime bookkeeping ([`ThreadRt`]) held that its events do not say
/// again on a restore: rebuilt from the events before `thread.deleted` (on replay too) and
/// put back by `thread.restored`.
///
/// Of [`ThreadRt`]: `workspace`, `pty_command`, `acp_env` and `account_dir` (from
/// `thread.created` and runtime or account moves) are where and how the thread runs; `session_settings` are what its session
/// last ran in (an attach that cannot apply the thread's model or effort is refused while they
/// differ, `hub::attach`); `closed_runtimes` numbers its runtimes, the middle part of their
/// attention ids (a restored thread's next runtime must not reuse an earlier one's). The rest
/// is a live runtime's (`live`, `process_live`, `pid`, `start_time`, `exe`,
/// `runtime_settings`, `final_text`, `exiting`, `stop_requested`, `reaping`, `after_reap`,
/// `restart_reason`), an operation's (`attach`, `park`, `resuming`, `deleting`) or memory
/// only (`queue_held`): a deleted thread has none of them (its deletion waited until its
/// runtime was over and recorded), and a restored one starts parked.
///
/// [`ThreadRt`]: super::ThreadRt
#[derive(PartialEq, Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(super) struct DeletedRt {
    workspace: Workspace,
    pty_command: Option<String>,
    acp_env: Vec<(String, String)>,
    account_dir: Option<std::path::PathBuf>,
    account_env_dir: Option<std::path::PathBuf>,
    session_settings: Option<super::restart::RunSettings>,
    closed_runtimes: u64,
    /// The trash item of its deletion (`thread.deleted` `trash`), if it has one.
    #[serde(default)]
    pub(super) item: Option<String>,
    /// That item is purged (`trash.purged`): the thread cannot be restored any more.
    #[serde(default)]
    pub(super) purged: bool,
}

impl DeletedRt {
    /// What of `rt` (a thread being deleted) a restore puts back.
    pub(super) fn of(rt: super::ThreadRt) -> DeletedRt {
        DeletedRt {
            workspace: rt.workspace,
            pty_command: rt.pty_command,
            acp_env: rt.acp_env,
            account_dir: rt.account_dir,
            account_env_dir: rt.account_env_dir,
            session_settings: rt.session_settings,
            closed_runtimes: rt.closed_runtimes,
            item: None,
            purged: false,
        }
    }

    /// Puts it back into `rt` (the thread restored by `p`), its branch as restored.
    pub(super) fn restore(self, rt: &mut super::ThreadRt, p: &ThreadRestored) {
        rt.workspace = match (self.workspace, &p.branch) {
            (Workspace::Worktree { .. }, Some(b)) => Workspace::Worktree {
                branch: b.clone(),
                existing_branch: p.existing_branch,
            },
            (w, _) => w,
        };
        rt.pty_command = self.pty_command;
        rt.acp_env = self.acp_env;
        rt.account_dir = self.account_dir;
        rt.account_env_dir = self.account_env_dir;
        rt.session_settings = self.session_settings;
        rt.closed_runtimes = self.closed_runtimes;
    }

    /// Its workspace.
    pub(super) fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

/// Milliseconds since the epoch, now.
fn now_ms() -> u64 {
    u64::try_from(Utc::now().timestamp_millis()).unwrap_or(0)
}

/// `ms` (since the epoch) as RFC 3339.
fn rfc3339(ms: u64) -> String {
    Utc.timestamp_millis_opt(i64::try_from(ms).unwrap_or(i64::MAX))
        .single()
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// The short id of item `id` people see and type: its last 8 characters.
fn short(id: &str) -> &str {
    &id[id.len().saturating_sub(8)..]
}

/// One listed item.
#[derive(Debug, Clone)]
struct Entry {
    item: String,
    seen: Seen,
    complete: bool,
    /// Why an incomplete item is never purged ([`TrashItem::kept`]).
    kept: Option<String>,
    offline: bool,
    /// The log's `thread.restored` of it.
    restored: Option<Restored>,
}

impl Entry {
    fn json(&self, keep_ms: u64) -> Value {
        let s = &self.seen;
        let purge_at = s.deleted_at_ms.saturating_add(keep_ms);
        let mut v = json!({
            "item": self.item,
            "short": short(&self.item),
            "thread": s.thread,
            "thread_name": s.name,
            "project": s.project,
            "box": s.box_name,
            "deleted_at": rfc3339(s.deleted_at_ms),
            "deleted_at_ms": s.deleted_at_ms,
            "purge_at": rfc3339(purge_at),
            "purge_at_ms": purge_at,
            "bytes": s.bytes,
            "size": human_bytes(s.bytes),
            "losses": s.losses,
            "worktree": s.worktree,
            "branch": s.branch,
            "dir": s.dir,
        });
        if !self.complete {
            v["incomplete"] = json!(true);
            if let Some(k) = &self.kept {
                v["kept"] = json!(k);
            }
        }
        if let Some(r) = &self.restored {
            v["restored_as"] = json!(r.name);
            v["restored_at"] = json!(rfc3339(r.at_ms));
            v["restored_at_ms"] = json!(r.at_ms);
        }
        if self.offline {
            v["box_offline"] = json!(true);
        }
        v
    }
}

impl Entry {
    /// Item `i` as box `box_name` listed it: the box is the one that listed it, whatever its
    /// manifest says (a box's runner writes the manifest; `None` for an item whose manifest
    /// names another box, which is left out, see [`Hub::trash_entries`]).
    fn listed(i: &TrashItem, box_name: &str) -> Option<Entry> {
        let m = &i.manifest;
        if m.info.box_name != box_name {
            tracing::warn!(
                item = %m.item,
                box_name,
                named = %m.info.box_name,
                "leaving out a trash item whose manifest names another box"
            );
            return None;
        }
        Some(Entry {
            item: m.item.clone(),
            seen: Seen {
                thread: m.info.thread.clone(),
                name: m.info.thread_name.clone(),
                project: m.info.project.clone(),
                box_name: box_name.to_string(),
                deleted_at_ms: m.info.deleted_at_ms,
                bytes: m.bytes,
                losses: Some(m.info.losses.clone()).filter(|l| !l.is_empty()),
                worktree: Some(m.worktree.display().to_string()),
                branch: m.branch.clone(),
                dir: Some(i.dir.display().to_string()),
            },
            complete: m.complete,
            kept: i.kept.clone(),
            offline: false,
            restored: None,
        })
    }
}

/// What the boxes listed of their trash ([`Hub::trash_listing`]).
#[derive(Debug, Default)]
struct Listed {
    /// The boxes that listed it.
    boxes: HashSet<String>,
    /// Every item they listed.
    ids: HashSet<String>,
    /// When the listing began (ms since the epoch): an item whose deletion was recorded before
    /// it was in its box's trash then.
    at_ms: u64,
}

impl Core {
    /// Records that trash item `item` of thread `thread` is purged (`trash.purged` on the hub's
    /// thread, `gone`: found gone): its entry goes (the fold drops it). A failure is logged
    /// and the entry dropped all the same (the item is gone; the next purge after its purge
    /// time records it). Returns whether it was recorded.
    pub(super) fn record_purged(
        &mut self,
        hub: &Inner,
        item: &str,
        thread: &str,
        gone: bool,
    ) -> bool {
        let body = EventBody::TrashPurged(TrashPurged {
            item: item.to_string(),
            thread: thread.to_string(),
            gone,
            ..Default::default()
        });
        match self.append(hub, HUB_THREAD, None, Source::Hub, body) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(item, thread, error = %format!("{e:#}"), "cannot record a purged trash item");
                self.trash.remove(item);
                false
            }
        }
    }

    /// Notes trash item `item` of `thread.deleted` `d` (thread `thread`, recorded at `at_ms`).
    pub(super) fn trash_seen(&mut self, thread: &str, d: &ThreadDeleted, at_ms: u64) {
        let Some(item) = &d.trash else {
            return;
        };
        let view = self
            .registry
            .get(thread)
            .or(self.registry.deleted_view(thread));
        self.trash.insert(
            item.clone(),
            Seen {
                thread: thread.to_string(),
                name: d.name.clone(),
                project: view.and_then(|v| v.project.clone()),
                box_name: view.map(|v| v.box_name.clone()).unwrap_or_default(),
                deleted_at_ms: at_ms,
                bytes: d.trash_bytes.unwrap_or(0),
                losses: d.losses.clone(),
                worktree: d.worktree.clone().or(view.and_then(|v| v.worktree.clone())),
                branch: d.branch.clone(),
                dir: d.trash_dir.clone(),
            },
        );
    }
}

impl Hub {
    /// The hosts of every box that is online (the local box always), by name.
    fn online_hosts(&self) -> Vec<(String, Arc<dyn ThreadHost>)> {
        let hosts = self.inner.hosts_list();
        let core = self.inner.core();
        let mut out: Vec<(String, Arc<dyn ThreadHost>)> = hosts
            .into_iter()
            .filter(|(name, _)| name.as_str() == LOCAL_BOX || core.online_boxes.contains(name))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Every trash item: those of the online boxes as they list them, then those the log knows
    /// of the other boxes; and the boxes that could not be asked, with why. A box's item is
    /// that box's: one whose manifest names another box is left out (and logged), so a
    /// runner's listing never stands for another box's items.
    async fn trash_entries(&self) -> (Vec<Entry>, Vec<(String, String)>) {
        let (entries, failed, _) = self.trash_listing().await;
        (entries, failed)
    }

    /// [`Hub::trash_entries`], and what the boxes listed: the boxes that listed their trash,
    /// and the id of every item they listed (one whose manifest names another box too).
    async fn trash_listing(&self) -> (Vec<Entry>, Vec<(String, String)>, Listed) {
        let at_ms = now_ms();
        let hosts = self.online_hosts();
        let listed =
            futures::future::join_all(hosts.iter().map(|(_, h)| h.git().trash_list())).await;
        let mut entries = Vec::new();
        let mut asked: HashSet<String> = HashSet::new();
        let mut ids: HashSet<String> = HashSet::new();
        let mut failed = Vec::new();
        for ((name, _), r) in hosts.iter().zip(listed) {
            match r {
                Ok(items) => {
                    asked.insert(name.clone());
                    ids.extend(items.iter().map(|i| i.manifest.item.clone()));
                    entries.extend(items.iter().filter_map(|i| Entry::listed(i, name)));
                }
                Err(e) => failed.push((name.clone(), format!("{e:#}"))),
            }
        }
        let core = self.inner.core();
        for (item, seen) in &core.trash {
            if !asked.contains(&seen.box_name) && !entries.iter().any(|e| e.item == *item) {
                entries.push(Entry {
                    item: item.clone(),
                    seen: seen.clone(),
                    complete: true,
                    kept: None,
                    offline: true,
                    restored: None,
                });
            }
        }
        for e in &mut entries {
            e.restored = core.items_restored.get(&e.item).cloned();
        }
        entries.sort_by(|a, b| {
            a.seen
                .deleted_at_ms
                .cmp(&b.seen.deleted_at_ms)
                .then(a.item.cmp(&b.item))
        });
        (
            entries,
            failed,
            Listed {
                boxes: asked,
                ids,
                at_ms,
            },
        )
    }

    /// `trash.list` (see the module docs): `{items: [{item, short, thread, thread_name,
    /// project, box, deleted_at, deleted_at_ms, purge_at, purge_at_ms, bytes, size, losses,
    /// worktree, branch, dir, incomplete?, kept?, box_offline?, restored_as?, restored_at?,
    /// restored_at_ms?}], keep_days, bytes, size,
    /// unavailable: [{box, error}], message}`, oldest first.
    pub async fn trash_list(&self) -> Result<Value, RpcError> {
        let keep_ms = self.inner.config.trash.keep_ms();
        let (entries, failed) = self.trash_entries().await;
        let bytes: u64 = entries.iter().map(|e| e.seen.bytes).sum();
        let mut message = match entries.len() {
            0 => "The trash is empty.".to_string(),
            n => format!(
                "{n} item{} in the trash ({}). Items are purged {} days after their deletion; \
                 `twr trash restore <item>` brings a thread back (the item stays until it is \
                 purged), `twr trash empty <item>` purges one now.",
                if n == 1 { "" } else { "s" },
                human_bytes(bytes),
                self.inner.config.trash.keep_days
            ),
        };
        if bytes > TRASH_BIG_BYTES {
            message.push_str(" `twr trash empty --all` frees all of it.");
        }
        Ok(json!({
            "items": entries.iter().map(|e| e.json(keep_ms)).collect::<Vec<_>>(),
            "keep_days": self.inner.config.trash.keep_days,
            "bytes": bytes,
            "size": human_bytes(bytes),
            "unavailable": failed
                .iter()
                .map(|(b, e)| json!({ "box": b, "error": e }))
                .collect::<Vec<_>>(),
            "message": message,
        }))
    }

    /// The item `query` names among `entries`: its id, a unique end of an id (at least 4
    /// characters, any case), or the name of the thread of exactly one item.
    fn find_entry<'a>(entries: &'a [Entry], query: &str) -> Result<&'a Entry, RpcError> {
        if let Some(e) = entries.iter().find(|e| e.item == query) {
            return Ok(e);
        }
        let q = query.to_ascii_uppercase();
        let by_id: Vec<&Entry> = if q.len() >= 4 {
            entries.iter().filter(|e| e.item.ends_with(&q)).collect()
        } else {
            Vec::new()
        };
        let found: Vec<&Entry> = if by_id.is_empty() {
            entries.iter().filter(|e| e.seen.name == query).collect()
        } else {
            by_id
        };
        match found.as_slice() {
            [one] => Ok(one),
            [] => Err(rpc(
                codes::NOT_FOUND,
                format!("no item {query:?} in the trash (`twr trash list` shows them)"),
            )),
            many => Err(rpc(
                codes::AMBIGUOUS,
                format!(
                    "{query:?} names {} items of the trash: {}; give its id",
                    many.len(),
                    many.iter()
                        .map(|e| format!("{} ({})", short(&e.item), e.seen.name))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )),
        }
    }

    /// The items `trash.empty {items, all}` names (see the module docs), by id: every item of
    /// every online box with `all`, and each of `items` ([`Hub::find_entry`]).
    async fn trash_chosen(
        &self,
        items: &[String],
        all: bool,
        caller: &Caller,
    ) -> Result<BTreeMap<String, Entry>, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user empties the trash; tell the user what is in it (list_trash)",
            ));
        }
        if items.is_empty() && !all {
            return Err(rpc(
                codes::BAD_REQUEST,
                "name the items to purge, or all of them (`--all`)",
            ));
        }
        let (entries, _) = self.trash_entries().await;
        let mut chosen: BTreeMap<String, Entry> = BTreeMap::new();
        if all {
            for e in entries.iter().filter(|e| !e.offline) {
                chosen.insert(e.item.clone(), e.clone());
            }
        }
        for q in items {
            let e = Self::find_entry(&entries, q)?;
            if e.offline {
                return Err(rpc(
                    codes::BOX_OFFLINE,
                    format!(
                        "item {} ({}) is on box {:?}, which is offline",
                        short(&e.item),
                        e.seen.name,
                        e.seen.box_name
                    ),
                ));
            }
            chosen.insert(e.item.clone(), e.clone());
        }
        Ok(chosen)
    }

    /// `trash.empty {items?, all?, dry_run: true}` (users only): what `trash.empty {items,
    /// all}` would purge now, purging nothing: `{dry_run: true, items: [{item, short, thread,
    /// thread_name, project, box, deleted_at, deleted_at_ms, purge_at, purge_at_ms, bytes, size,
    /// losses, worktree, branch, dir, incomplete?, kept?, restored_as?, restored_at?,
    /// restored_at_ms?}], bytes, size, message}`, oldest first. A client that asks the user
    /// first shows these, then purges exactly them by id (`trash.empty {items: [ids]}`), so
    /// an item that arrived, or a name that came to mean another item, while the user was
    /// asked is never purged by that answer.
    pub async fn trash_empty_preview(
        &self,
        items: &[String],
        all: bool,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let chosen = self.trash_chosen(items, all, caller).await?;
        let keep_ms = self.inner.config.trash.keep_ms();
        let mut listed: Vec<&Entry> = chosen.values().collect();
        listed.sort_by(|a, b| {
            a.seen
                .deleted_at_ms
                .cmp(&b.seen.deleted_at_ms)
                .then(a.item.cmp(&b.item))
        });
        let bytes: u64 = listed.iter().map(|e| e.seen.bytes).sum();
        let message = match listed.len() {
            0 => "The trash is empty: nothing to purge.".to_string(),
            n => format!(
                "{n} item{} to purge ({}).",
                if n == 1 { "" } else { "s" },
                human_bytes(bytes)
            ),
        };
        Ok(json!({
            "dry_run": true,
            "items": listed.iter().map(|e| e.json(keep_ms)).collect::<Vec<_>>(),
            "bytes": bytes,
            "size": human_bytes(bytes),
            "message": message,
        }))
    }

    /// `trash.empty {items?, all?}` (users only; see the module docs): `{purged: [{item,
    /// thread_name, box, bytes}], failed: [{item, box, error}], bytes, size, message}`. Every
    /// client purges by the ids it showed the user ([`Hub::trash_empty_preview`]); `all` stays
    /// for callers that ask nobody.
    pub async fn trash_empty(
        &self,
        items: &[String],
        all: bool,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let chosen = self.trash_chosen(items, all, caller).await?;
        let mut by_box: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for e in chosen.values() {
            by_box
                .entry(e.seen.box_name.clone())
                .or_default()
                .push((e.item.clone(), e.seen.thread.clone()));
        }
        let (purged, failed) = self.purge_on_boxes(by_box).await;
        let purged: Vec<&Entry> = purged.iter().filter_map(|id| chosen.get(id)).collect();
        let bytes: u64 = purged.iter().map(|e| e.seen.bytes).sum();
        let message = match (purged.len(), failed.len()) {
            (0, 0) => "The trash is empty: nothing to purge.".to_string(),
            (n, 0) => format!(
                "Purged {n} item{} ({} freed).",
                if n == 1 { "" } else { "s" },
                human_bytes(bytes)
            ),
            (n, f) => format!(
                "Purged {n} item{} ({} freed); {f} could not be purged.",
                if n == 1 { "" } else { "s" },
                human_bytes(bytes)
            ),
        };
        Ok(json!({
            "purged": purged
                .iter()
                .map(|e| json!({
                    "item": e.item,
                    "short": short(&e.item),
                    "thread_name": e.seen.name,
                    "box": e.seen.box_name,
                    "bytes": e.seen.bytes,
                }))
                .collect::<Vec<_>>(),
            "failed": failed
                .iter()
                .map(|(item, b, error)| json!({ "item": item, "box": b, "error": error }))
                .collect::<Vec<_>>(),
            "bytes": bytes,
            "size": human_bytes(bytes),
            "message": message,
        }))
    }

    /// Purges `items` (`(item, its thread)`) on each box (by box name), recording each one
    /// purged (`trash.purged`): `(purged ids, [(item, box, why)])`. An item being restored or
    /// purged meanwhile is left alone. A box's answer counts only for the ids it was asked to
    /// purge, and what is recorded of an item (its thread, its box) is what the log says of
    /// it, never what a box says: an item the log does not know as that box's is not recorded.
    /// A remote box's answer that is lost (a crash, a failed append) records nothing: the item
    /// stays as the log knows it (listed for the box while it is offline), and its thread is
    /// not taken as purged (the hub never infers a remote purge).
    async fn purge_on_boxes(
        &self,
        items: BTreeMap<String, Vec<(String, String)>>,
    ) -> (Vec<String>, Vec<(String, String, String)>) {
        let mut purged = Vec::new();
        let mut failed = Vec::new();
        let mut held: Vec<String> = Vec::new();
        let mut done: Vec<(String, String)> = Vec::new();
        let items: BTreeMap<String, Vec<String>> = {
            let mut core = self.inner.core();
            items
                .into_iter()
                .map(|(box_name, ids)| {
                    let mine = ids
                        .into_iter()
                        .map(|(id, _)| id)
                        .filter(|id| {
                            let free = core.trash_busy.insert(id.clone());
                            if free {
                                held.push(id.clone());
                            } else {
                                failed.push((
                                    id.clone(),
                                    box_name.clone(),
                                    "the hub is at work on it (moving a thread into it, \
                                     restoring or purging it)"
                                        .into(),
                                ));
                            }
                            free
                        })
                        .collect();
                    (box_name, mine)
                })
                .collect()
        };
        for (box_name, ids) in items {
            if ids.is_empty() {
                continue;
            }
            let Some(host) = self.inner.host(&box_name) else {
                for id in ids {
                    failed.push((id, box_name.clone(), "no such box".to_string()));
                }
                continue;
            };
            let asked: HashSet<&String> = ids.iter().collect();
            match host
                .git()
                .trash_purge(TrashPurge {
                    items: ids.clone(),
                    before_ms: None,
                })
                .await
            {
                Ok(out) => {
                    let (mine, other): (Vec<String>, Vec<String>) =
                        out.purged.into_iter().partition(|id| asked.contains(id));
                    if !other.is_empty() {
                        tracing::warn!(box_name = %box_name, items = ?other, "a box said it purged trash items it was not asked to; ignored");
                    }
                    if !mine.is_empty() {
                        tracing::info!(box_name = %box_name, items = ?mine, "purged trash items");
                    }
                    done.extend(mine.iter().map(|id| (id.clone(), box_name.clone())));
                    purged.extend(mine);
                    for f in out.failed {
                        failed.push((f.item, box_name.clone(), f.error));
                    }
                    for u in out.unknown {
                        failed.push((u, box_name.clone(), "not in the trash any more".into()));
                    }
                }
                Err(e) => {
                    for id in ids {
                        failed.push((id, box_name.clone(), format!("{e:#}")));
                    }
                }
            }
        }
        let mut core = self.inner.core();
        for id in &held {
            core.trash_busy.remove(id);
        }
        for (id, box_name) in &done {
            match core.trash.get(id) {
                Some(seen) if seen.box_name == *box_name => {
                    let thread = seen.thread.clone();
                    core.record_purged(&self.inner, id, &thread, false);
                }
                _ => {
                    tracing::debug!(item = %id, box_name = %box_name, "purged a trash item the log does not know as that box's; nothing recorded")
                }
            }
        }
        (purged, failed)
    }

    /// Purges every item of every online box deleted more than `[trash] keep_days` before
    /// `now_ms` (ms since the epoch), restored ones too. Returns `{purged: [item],
    /// failed: [{item, box, error}], unavailable: [{box, error}]}`.
    pub async fn purge_trash(&self, now_ms: u64) -> Value {
        // What the log and the boxes disagree on is settled first.
        let reconciled = self.reconcile_boxes(None).await;
        let cutoff = now_ms.saturating_sub(self.inner.config.trash.keep_ms());
        let (entries, unavailable, listed) = self.trash_listing().await;
        let mut by_box: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
        for e in entries
            .iter()
            .filter(|e| !e.offline && e.seen.deleted_at_ms < cutoff)
        {
            by_box
                .entry(e.seen.box_name.clone())
                .or_default()
                .push((e.item.clone(), e.seen.thread.clone()));
        }
        let (purged, failed) = self.purge_on_boxes(by_box).await;
        let gone = self.record_gone(&listed, cutoff);
        for (box_name, e) in &unavailable {
            tracing::debug!(box_name = %box_name, error = %e, "cannot list the trash to purge it");
        }
        json!({
            "purged": purged,
            "failed": failed
                .iter()
                .map(|(item, b, error)| json!({ "item": item, "box": b, "error": error }))
                .collect::<Vec<_>>(),
            "unavailable": unavailable
                .iter()
                .map(|(b, e)| json!({ "box": b, "error": e }))
                .collect::<Vec<_>>(),
            "reconciled": reconciled,
            "gone": gone,
        })
    }

    /// Records as purged (`trash.purged` `gone`) each item of the hub's own box (`local`) the
    /// log knows that is past its purge time (deleted before `cutoff`, ms since the epoch), that
    /// the box's listing `listed` does not have although its deletion was recorded before the
    /// listing, and whose directory is not there (`lstat` says so: not merely unreadable): a
    /// purge from before purges were recorded, or one removed by hand. Nothing else is taken
    /// as gone: an item a listing skipped (a manifest it cannot read for a while), one not past
    /// its purge time, one the hub is at work on, and any item of a remote box, whose files the
    /// hub cannot look at (only that box's own answer to a purge records it purged). A missed
    /// record only delays archiving; a wrong one would make a thread that can still come back
    /// unrestorable. Returns the items recorded.
    fn record_gone(&self, listed: &Listed, cutoff: u64) -> Vec<String> {
        let root = super::lifecycle::data_dir_of(&self.inner.db_path).join("trash");
        let candidates: Vec<(String, String)> = {
            let core = self.inner.core();
            core.trash
                .iter()
                .filter(|(item, seen)| {
                    seen.box_name == LOCAL_BOX
                        && listed.boxes.contains(LOCAL_BOX)
                        && !listed.ids.contains(*item)
                        && seen.deleted_at_ms < cutoff
                        && seen.deleted_at_ms < listed.at_ms
                        && !core.trash_busy.contains(*item)
                        && valid_trash_id(item)
                })
                .map(|(item, seen)| (item.clone(), seen.thread.clone()))
                .collect()
        };
        // Gone for certain: its directory is not there.
        let gone: Vec<(String, String)> = candidates
            .into_iter()
            .filter(|(item, _)| {
                matches!(
                    std::fs::symlink_metadata(root.join(item)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound
                )
            })
            .collect();
        let mut core = self.inner.core();
        gone.into_iter()
            .filter(|(item, thread)| {
                !core.trash_busy.contains(item)
                    && core.trash.contains_key(item)
                    && core.record_purged(&self.inner, item, thread, true)
            })
            .map(|(item, _)| item)
            .collect()
    }

    /// Settles what the log and the boxes' trash disagree on (see [`Hub::reconcile_boxes`]),
    /// on every online box. Returns what it did.
    pub async fn reconcile_trash(&self) -> Value {
        self.reconcile_boxes(None).await
    }

    /// Settles what the log and the trash of the online boxes (`only` that one) disagree on,
    /// after a crash or a failed append on either side, item by item (none an operation of the
    /// hub holds, [`Core::trash_busy`]):
    ///
    /// - a worktree a restore of an item made ([`TrashItem::restored`], found by its mark)
    ///   while the log has no `thread.restored` of that item and still shows its thread
    ///   deleted: the restore was not recorded, so what it made (the worktree, its record, the
    ///   branch) is removed; the item was never changed. The log never claims a restore the box
    ///   does not have, nor the box one the log does not.
    /// - a complete item whose thread is still here, parked, with nothing of its runtime at
    ///   work, and no `thread.deleted` naming it: its deletion moved everything and was not
    ///   recorded; it is recorded now (`thread.deleted`, from the item's manifest), as the user
    ///   confirmed it.
    ///
    /// Runs when the hub starts (the local box), when a runner's box comes online, and before
    /// every purge. Returns `{undone: [item], recorded: [item], failed: [{item, box, error}]}`.
    ///
    /// [`TrashItem::restored`]: twr_core::git::TrashItem::restored
    pub(super) async fn reconcile_boxes(&self, only: Option<&str>) -> Value {
        enum Act {
            Undo,
            Record,
        }
        let mut done: BTreeMap<&'static str, Vec<String>> = BTreeMap::new();
        let mut failed: Vec<Value> = Vec::new();
        for (box_name, host) in self.online_hosts() {
            if only.is_some_and(|b| b != box_name) {
                continue;
            }
            let git = host.git();
            let items = match git.trash_list().await {
                Ok(i) => i,
                Err(e) => {
                    tracing::debug!(box_name = %box_name, error = %format!("{e:#}"), "cannot list the trash to reconcile it");
                    continue;
                }
            };
            for i in items {
                let m = &i.manifest;
                let id = m.item.clone();
                let act = {
                    let mut core = self.inner.core();
                    if core.trash_busy.contains(&id) {
                        continue;
                    }
                    let act = if i.restored.is_some()
                        && !core.items_restored.contains_key(&id)
                        && core
                            .registry
                            .deleted_view(&m.info.thread)
                            .is_some_and(|v| v.box_name == box_name)
                    {
                        Act::Undo
                    } else if m.complete
                        && !core.items_deleted.contains(&id)
                        && core
                            .registry
                            .get(&m.info.thread)
                            .is_some_and(|v| v.box_name == box_name)
                    {
                        Act::Record
                    } else {
                        continue;
                    };
                    core.trash_busy.insert(id.clone());
                    act
                };
                let (what, r) = match act {
                    Act::Undo => (
                        "undone",
                        git.trash_restore_undo(id.clone())
                            .await
                            .map_err(|e| format!("{e:#}")),
                    ),
                    Act::Record => ("recorded", self.record_trashed(&i)),
                };
                self.inner.core().trash_busy.remove(&id);
                match r {
                    Ok(true) => {
                        tracing::info!(item = %id, box_name = %box_name, what, "reconciled a trash item with the log");
                        done.entry(what).or_default().push(id);
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(item = %id, box_name = %box_name, error = %e, "cannot reconcile a trash item with the log");
                        failed.push(json!({ "item": id, "box": box_name, "error": e }));
                    }
                }
            }
        }
        json!({
            "undone": done.remove("undone").unwrap_or_default(),
            "recorded": done.remove("recorded").unwrap_or_default(),
            "failed": failed,
        })
    }

    /// Records the deletion of the thread whose complete trash item is `i`, which no
    /// `thread.deleted` names (see [`Hub::reconcile_boxes`]): `Ok(false)` when the thread is
    /// not as its deletion left it (gone, or not parked or with something of its runtime at
    /// work: it was taken up again, and the item stays an item of the trash).
    fn record_trashed(&self, i: &TrashItem) -> Result<bool, String> {
        let m = &i.manifest;
        let thread = &m.info.thread;
        let mut core = self.inner.core();
        let Some(view) = core.registry.get(thread) else {
            return Ok(false);
        };
        if !view.parked || core.runtime_outstanding(thread).is_some() {
            tracing::warn!(thread = %thread, item = %m.item, "a thread whose deletion was not recorded was taken up again; its trash item stays");
            return Ok(false);
        }
        let deleted = ThreadDeleted {
            name: view.name.clone(),
            worktree: m.worktree_moved.then(|| m.worktree.display().to_string()),
            worktree_left: m.worktree_left.then(|| m.worktree.display().to_string()),
            build_output_aside: m
                .build_output_aside
                .iter()
                .map(|k| k.path.clone())
                .collect(),
            branch: m.branch.clone().filter(|_| m.branch_deleted),
            trash: Some(m.item.clone()),
            trash_dir: Some(i.dir.display().to_string()),
            trash_bytes: Some(m.bytes),
            losses: Some(m.info.losses.clone()),
            ..Default::default()
        };
        core.record_deleted(&self.inner, thread, deleted, Source::Hub)
            .map(|()| true)
            .map_err(|e| format!("recording the deletion failed: {e:#}"))
    }

    /// `trash.restore {item, as?}` (users only; see the module docs): `{thread, thread_name,
    /// previous_name?, item, box, worktree, branch, notes, skipped, session_id?, message}`.
    /// Runs to its end in a task of its own, also when the request goes away first: what the
    /// box made is recorded (or removed again when it cannot be), and the item and the name
    /// are let go only then.
    pub async fn trash_restore(
        &self,
        query: &str,
        as_name: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let (hub, query, as_name, caller) = (
            self.clone(),
            query.to_string(),
            as_name.map(str::to_string),
            caller.clone(),
        );
        tokio::spawn(async move {
            hub.restore_from_trash(&query, as_name.as_deref(), &caller)
                .await
        })
        .await
        .unwrap_or_else(|e| Err(rpc(codes::INTERNAL, format!("restoring failed: {e}"))))
    }

    /// [`Hub::trash_restore`], in its task.
    async fn restore_from_trash(
        &self,
        query: &str,
        as_name: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "only the user restores a thread from the trash",
            ));
        }
        let (entries, _) = self.trash_entries().await;
        let entry = Self::find_entry(&entries, query)?.clone();
        let item = entry.item.clone();
        if let Some(r) = &entry.restored {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "item {} ({}) was restored already, as {} on {}; it stays in the trash until \
                     it is purged (`twr trash empty {}` purges it now)",
                    short(&item),
                    entry.seen.name,
                    r.name,
                    rfc3339(r.at_ms),
                    short(&item)
                ),
            ));
        }
        if entry.offline {
            return Err(rpc(
                codes::BOX_OFFLINE,
                format!(
                    "item {} ({}) is on box {:?}, which is offline",
                    short(&item),
                    entry.seen.name,
                    entry.seen.box_name
                ),
            ));
        }
        if !entry.complete {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "item {} is incomplete: deleting {} failed part way and the thread is still \
                     here; ask again to delete it, or look at {}",
                    short(&item),
                    entry.seen.name,
                    entry.seen.dir.as_deref().unwrap_or("the item")
                ),
            ));
        }
        let hub = &self.inner;
        let thread = entry.seen.thread.clone();
        // `_op`: the box's operation, begun with its gate check and held until the restore is
        // over, so the box is not switched meanwhile (`Core::reserve_box`).
        let (view, name, branch, host, rt, former, _op) = {
            let mut core = hub.core();
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let op = core.reserve_box(hub, &entry.seen.box_name)?;
            let Some(view) = core.registry.deleted_view(&thread).cloned() else {
                return Err(rpc(
                    codes::NOT_FOUND,
                    format!(
                        "item {} was thread {} ({thread}), which is no deleted thread of this hub",
                        short(&item),
                        entry.seen.name
                    ),
                ));
            };
            // The item is the listing box's: it brings back only a thread of that box.
            if view.box_name != entry.seen.box_name {
                tracing::warn!(item = %item, thread = %thread, box_name = %entry.seen.box_name, owner = %view.box_name, "refusing to restore a trash item for a thread of another box");
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "item {} is on box {:?}, but thread {thread} was on box {:?}: it cannot \
                         bring that thread back",
                        short(&item),
                        entry.seen.box_name,
                        view.box_name
                    ),
                ));
            }
            // A thread comes back only from the item of its latest deletion, and never once
            // that item is purged: what its agent did is archived by then. (An older item of
            // it, one whose deletion was not recorded, does not bring it back.)
            let current = core.deleted_rt.get(&thread);
            if current.is_some_and(|d| d.purged) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{}'s trash item was purged; the thread cannot be restored",
                        entry.seen.name
                    ),
                ));
            }
            if current.is_some_and(|d| d.item.as_deref() != Some(item.as_str())) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "item {} is not the one {} was last deleted into; restore that one",
                        short(&item),
                        entry.seen.name
                    ),
                ));
            }
            let name = match as_name {
                Some(n) => {
                    super::threads::validate_name(n)?;
                    n.to_string()
                }
                None => entry.seen.name.clone(),
            };
            if core.name_taken(&name) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "the name {name} is taken by another thread now; restore it under \
                         another name (`twr trash restore {} --as <name>`)",
                        short(&item)
                    ),
                ));
            }
            if !core.trash_busy.insert(item.clone()) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "item {} ({}) is being restored or purged already",
                        short(&item),
                        entry.seen.name
                    ),
                ));
            }
            let rt = core.deleted_rt.get(&thread).cloned().unwrap_or_default();
            let host = hub.host(&view.box_name);
            let new_branch = || {
                host.as_ref()
                    .map_or_else(|| thread_branch(&name), |h| h.worktree_branch(&name))
            };
            let branch = match (rt.workspace(), as_name) {
                (Workspace::Worktree { .. }, Some(_)) => new_branch(),
                (Workspace::Worktree { branch, .. }, None) => branch.clone(),
                _ => entry.seen.branch.clone().unwrap_or_else(new_branch),
            };
            // Its former names come back with it where no other thread has taken them
            // meanwhile, in any case (a deleted thread's names are free); reserved with its
            // name until the restore is recorded, so none is taken in between. Only the very
            // name it comes back under leaves them: `api` stays when it comes back as `API`.
            let former: Vec<String> = view
                .former_names
                .iter()
                .filter(|n| **n != name && !core.name_taken(n))
                .cloned()
                .collect();
            core.reserved_names.insert(name.clone());
            core.reserved_names.extend(former.iter().cloned());
            (view, name, branch, host, rt, former, op)
        };
        let release = |core: &mut Core| {
            core.trash_busy.remove(&item);
            core.reserved_names.remove(&name);
            for n in &former {
                core.reserved_names.remove(n);
            }
        };
        // The box makes the worktree and branch again from the item (which it does not
        // change); the hub records it, or, when it cannot, has the box remove what it made.
        // A crash in between is settled by `reconcile_boxes` the same way.
        let result = async {
            let host = host.ok_or_else(|| {
                rpc(
                    codes::BOX_OFFLINE,
                    format!("box {:?} is not available", view.box_name),
                )
            })?;
            if !branch.split('/').all(valid_branch_component) {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{branch:?} cannot be a branch"),
                ));
            }
            let git = host.git();
            let mut paths: Vec<PathBuf> = entry.seen.worktree.iter().map(PathBuf::from).collect();
            if let Some(project) = &view.project {
                let fresh = host.worktree_path(project, &name);
                paths.push(fresh.clone());
                for n in 2..=FRESH_PATHS + 1 {
                    paths.push(PathBuf::from(format!("{}-{n}", fresh.display())));
                }
            }
            let repo = view.repo.clone().map(PathBuf::from);
            let held = match &repo {
                Some(r) => Some(
                    hub.reservations
                        .reserve_repository(&view.box_name, git.as_ref(), r)
                        .await,
                ),
                None => None,
            };
            // A worktree switched to a branch another thread records now comes back on its own
            // branch: the threads as they are once the repository is held (one made or parked
            // while the restore waited for it counts).
            let others = hub.core().other_branches(&thread, &view.box_name);
            let restore = git.trash_restore(RestoreRequest {
                item: item.clone(),
                branch: branch.clone(),
                paths,
                other_branches: Some(others),
            });
            let out = match &held {
                Some(h) => h.during(restore).await,
                None => restore.await,
            }
            .map_err(|e| {
                let msg = format!("{e:#}");
                let hint = if msg.contains("exists already") {
                    format!(
                        "; restore it under another name (`twr trash restore {} --as <name>`)",
                        short(&item)
                    )
                } else {
                    String::new()
                };
                rpc(
                    codes::CONFLICT,
                    format!("cannot restore {}: {msg}{hint}", entry.seen.name),
                )
            })?;
            let previous_name = (name != entry.seen.name).then(|| entry.seen.name.clone());
            let existing_branch = matches!(
                rt.workspace(),
                Workspace::Worktree {
                    existing_branch: true,
                    ..
                }
            ) && as_name.is_none();
            let recorded = hub.core().append(
                hub,
                &thread,
                None,
                caller.source(),
                EventBody::ThreadRestored(ThreadRestored {
                    item: item.clone(),
                    name: name.clone(),
                    previous_name: previous_name.clone(),
                    worktree: Some(out.worktree.display().to_string()),
                    branch: Some(out.branch.clone()),
                    existing_branch,
                    former_names: former.clone(),
                    ..Default::default()
                }),
            );
            if let Err(e) = recorded {
                let undo = git.trash_restore_undo(item.clone());
                let undone = match &held {
                    Some(h) => h.during(undo).await,
                    None => undo.await,
                };
                let undone = match undone {
                    Ok(_) => "what it made is removed again, and the item is as it was".to_string(),
                    Err(u) => format!(
                        "removing what it made on box {:?} failed too ({u:#}); the hub removes it \
                         once it can, and the item is as it was",
                        view.box_name
                    ),
                };
                return Err(rpc(
                    codes::INTERNAL,
                    format!(
                        "restoring {} could not be recorded ({e:#}); {undone}",
                        entry.seen.name
                    ),
                ));
            }
            Ok((out, previous_name))
        }
        .await;
        let mut core = hub.core();
        release(&mut core);
        let (out, previous_name) = result?;
        let session = core
            .registry
            .get(&thread)
            .and_then(|v| v.session_id.clone());
        let mut message = format!(
            "{name} is back (parked) from trash item {}, its worktree at {} on branch {}",
            short(&item),
            out.worktree.display(),
            out.checkout.as_ref().unwrap_or(&out.branch)
        );
        if out.checkout.is_some() {
            message.push_str(&format!(
                " as it was when it was deleted (its own branch {} is back too)",
                out.branch
            ));
        }
        for n in &out.notes {
            message.push_str(&format!("; {n}"));
        }
        message.push_str(&format!(
            ". `twr resume {name}` resumes it{}. The item stays in the trash until it is purged.",
            if session.is_some() {
                " (its session too, if its runtime still has it)"
            } else {
                ""
            }
        ));
        Ok(json!({
            "thread": thread,
            "thread_name": name,
            "previous_name": previous_name,
            "item": item,
            "box": view.box_name,
            "worktree": out.worktree,
            "branch": out.branch,
            "checkout": out.checkout,
            "notes": out.notes,
            "skipped": out.skipped,
            "session_id": session,
            "message": message,
        }))
    }
}

/// The trash purge: once right away, then every `period` until the hub stops or is dropped
/// ([`Hub::purge_trash`]).
pub(super) async fn trash_ticker(hub: Weak<Inner>, period: Duration) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(inner) = hub.upgrade() else {
            return;
        };
        if *inner.state.borrow() != HubState::Running {
            return;
        }
        Hub { inner }.purge_trash(now_ms()).await;
    }
}

/// `deleted_at` of a trash item made now (ms since the epoch).
pub(super) fn deleted_at_now() -> u64 {
    now_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::config::Config;

    use super::super::HubOptions;

    /// A remote box's item is recorded purged only from that box's answer: a listing of the
    /// box that lacks it (an answer lost to a crash, or a manifest it cannot read) records
    /// nothing, whatever its age: the item is still listed for the box while it is offline,
    /// and its thread is not taken as purged.
    #[tokio::test]
    async fn a_remote_item_missing_from_its_listing_is_never_taken_as_purged() {
        use crate::hub::checkpoint::tests::{quiet_options, Log};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let item = "01AAAAAAAAAAAAAAAAAAAAAAAA";
        let mut l = Log::default();
        l.ev(
            "t-a",
            Source::Hub,
            EventBody::ThreadCreated(ThreadCreated {
                name: "a".into(),
                box_name: "gpu".into(),
                runtime: "fake".into(),
                ..Default::default()
            }),
        );
        l.ev(
            "t-a",
            Source::Hub,
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "a".into(),
                trash: Some(item.into()),
                ..Default::default()
            }),
        );
        l.write(&db);
        let hub = Hub::open(quiet_options(&db)).await.unwrap();
        let listed = Listed {
            boxes: ["gpu".to_string()].into(),
            ids: Default::default(),
            at_ms: now_ms(),
        };
        assert!(hub.record_gone(&listed, u64::MAX).is_empty());
        let shown: Vec<String> = hub.trash_list().await.unwrap()["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["item"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(shown, [item]);
        {
            let core = hub.inner.core();
            assert!(!core.deleted_rt["t-a"].purged);
        }
        hub.shutdown().await;
    }

    /// What a deleted thread's runtime bookkeeping held besides its events comes
    /// back with a restore, live and on replay: the settings its session last ran in (an
    /// attach that cannot apply the thread's newer model is still refused) and how many of its
    /// runtimes the log closed (the next runtime's attention ids do not reuse an earlier
    /// one's).
    #[tokio::test]
    async fn a_restore_brings_back_the_session_settings_and_runtime_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let user = || Source::User("test".into());
        let session = SessionPayload {
            session_id: Some("s-1".into()),
            ..Default::default()
        };
        for (source, body) in [
            (
                user(),
                EventBody::ThreadCreated(ThreadCreated {
                    name: "api".into(),
                    box_name: "local".into(),
                    runtime: "fake".into(),
                    mode: "manual".into(),
                    ..Default::default()
                }),
            ),
            (
                user(),
                EventBody::ThreadModelChanged(ThreadModelChanged {
                    model: Some("m1".into()),
                    ..Default::default()
                }),
            ),
            (
                Source::Adapter("fake".into()),
                EventBody::ThreadStarted(session.clone()),
            ),
            (
                Source::Adapter("fake".into()),
                EventBody::ThreadParked(session.clone()),
            ),
            (
                user(),
                EventBody::ThreadModelChanged(ThreadModelChanged {
                    model: Some("m2".into()),
                    previous_model: Some("m1".into()),
                    ..Default::default()
                }),
            ),
        ] {
            hub.append("t1", None, source, body, None).unwrap();
        }
        let kept = |core: &Core| {
            let rt = core.rt.get("t1").unwrap();
            (
                rt.session_settings.as_ref().and_then(|s| s.model.clone()),
                rt.closed_runtimes,
            )
        };
        let before = kept(&hub.inner.core());
        assert_eq!(before, (Some("m1".into()), 1));
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "api".into(),
                trash: Some("01J00000000000000000000000".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert!(!hub.inner.core().rt.contains_key("t1"));
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: "api".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(kept(&hub.inner.core()), before);
        {
            let core = hub.inner.core();
            assert!(core.items_deleted.contains("01J00000000000000000000000"));
            assert!(core
                .items_restored
                .contains_key("01J00000000000000000000000"));
        }
        hub.shutdown().await;
        drop(hub);
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        assert_eq!(kept(&hub.inner.core()), before);
        hub.shutdown().await;
    }

    /// A thread restored under another name (`trash restore --as`) is on the branch made
    /// again under that name: the handoff its next runtime gets names that branch, not the
    /// one `thread.created` recorded (which still says where it started).
    #[tokio::test]
    async fn the_handoff_names_the_branch_a_restore_made() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        let user = || Source::User("test".into());
        let mut created = ThreadCreated {
            name: "old".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            worktree: Some("/w/old".into()),
            ..Default::default()
        };
        created.ext = super::super::twr_ext_map(json!({
            "worktree": "twr",
            "branch": "twr/old",
            "base": "main",
        }));
        hub.append("t1", None, user(), EventBody::ThreadCreated(created), None)
            .unwrap();
        let item = "01J00000000000000000000000";
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "old".into(),
                trash: Some(item.into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadRestored(ThreadRestored {
                item: item.into(),
                name: "new".into(),
                previous_name: Some("old".into()),
                worktree: Some("/w/new".into()),
                branch: Some("twr/new".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let handoff = hub
            .inner
            .core()
            .thread_handoff("t1", None)
            .unwrap()
            .expect("a handoff");
        assert!(
            handoff.contains("- branch: twr/new (its own worktree of"),
            "{handoff}"
        );
        assert!(handoff.contains("started from main"), "{handoff}");
        assert!(!handoff.contains("twr/old"), "{handoff}");
        hub.shutdown().await;
    }
}

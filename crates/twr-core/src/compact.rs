//! The compacted control backlog: which stored control items a client folding the control
//! stream needs to end in the state it would reach folding all of them (`control.subscribe`
//! with `compact: true`).
//!
//! Most of a long-used hub's control log is `task.updated` (progress and heartbeats of tasks
//! long ended), then `git.snapshot`, `account.usage` and `worktree.disk`, of which only the
//! latest counts. [`select`] keeps, of the items of `(since, head]`:
//!
//! - every item of every other kind, and the first item of each thread (its view's
//!   `created_at` when it has no `thread.created`);
//! - `task.updated`, per task of a thread:
//!   - for the task present at `head` (in the thread's view, or its deleted view: [`Head`]),
//!     the update that started it (its `started_seq`) and, after it, the latest update, the
//!     latest with a description, the latest naming a job's process group and the latest
//!     naming its members: what its [`BackgroundTask`] holds at `head`;
//!   - for any task, the updates that may have started it while something read the thread's
//!     tasks: a `report.ready` of the thread (the notifier's interim-report rule,
//!     [`ThreadView::own_background`]), or a stop, park, crash, attach, restore or new runtime
//!     start ([`ThreadView::cut_short`] reads whether the runtime had tasks). A task may start
//!     at its first running update after the thread's last lifecycle event or the task's last
//!     end, so each of those is a candidate (of a job, only the first after its end: a job's
//!     update always applies); one is kept when such a reader comes before the task's next
//!     end (or, for a runtime's task, the next event that drops it), with that end. Every
//!     update between candidates finds the task already there, or is ignored as it would
//!     have been. A running update kept for another reason (the thread's first item) has its
//!     end kept too. Each candidate is looked at once: the selection is linear in the rows;
//!   - every end of a job (the TUI remembers the jobs that ended: a `!` command's answer that
//!     comes after its end);
//! - `git.snapshot`: the latest per thread, none before a `thread.restored` (which clears it);
//! - `worktree.disk`: the latest per thread (every `worktree.trimmed` is kept);
//! - `account.usage`: the latest per thread, runtime and account;
//! - `conversation.update`: a thread's first item, and every one that turns the thread's offers
//!   to compact its context on or off (`ext.twr.idle_compaction`,
//!   [`ThreadView::idle_compaction_off`]); the reducers read nothing else of them but the time
//!   of a thread's first item and [`ThreadView::last_seq`].
//!
//! Events of a thread while it is deleted change nothing of it ([`Registry`] drops them):
//! none of them is a candidate or the latest of anything.
//!
//! A thread purged from the trash (deleted at head, and the trash item of its last delete
//! purged: a `trash.purged` of an older item, from a delete it was restored from, does not
//! count) can never come back, and no item names it again. Of its items before that, only those that fold into more than its own
//! deleted view and its own entries of the notifier's memory are kept: its first item, its
//! lifecycle (`thread.created`, `thread.renamed`, `thread.deleted`, `thread.restored`: the
//! colors of the threads created after it are allocated from those alive then), its
//! `decision.made` (the decision table), its attention items (the requests a turn raised, the
//! stalls that went on, the notifier's attention keys) and the stops, parks, crashes and
//! attaches that close its requests, its `turn.started` from a message's ref (the turns a
//! client's messages started), its jobs' ends (the `!` commands that ended) and anything else
//! not listed here. Its tasks' updates, git snapshots, disk measurements, reports, other turn
//! starts, turn ends, runtime starts and resumes, and `conversation.update`s go.
//!
//! What it guarantees: folding the kept items in gseq order into [`Registry`],
//! [`AttentionStack`], [`crate::usage::UsageView`] and [`crate::notify::Policy`] gives the
//! state folding every item gives, except [`ThreadView::last_activity`],
//! [`ThreadView::worked_at`] and [`ThreadView::last_seq`], which no client reads from the
//! control stream, and, of a purged thread, its deleted view beyond who it was (id, name,
//! aliases, former names, role, color, creation time) and the notifier's report and reply keys
//! and reports known of it, which nothing reads again. (So past the notifier's bound on the
//! keys it remembers, it may still remember older keys of other threads that a full fold let
//! go: no key it gives can come again, so what it remembers of them changes nothing it gives.) A client that folded the items up to
//! `since` this way gets the same after `(since, head]`. The tests check it on made-up logs
//! and, by hand, on a real one.
//!
//! [`BackgroundTask`]: crate::reduce::BackgroundTask
//! [`ThreadView::own_background`]: crate::reduce::ThreadView::own_background
//! [`ThreadView::cut_short`]: crate::reduce::ThreadView::cut_short
//! [`ThreadView::last_activity`]: crate::reduce::ThreadView::last_activity
//! [`ThreadView::worked_at`]: crate::reduce::ThreadView::worked_at
//! [`ThreadView::last_seq`]: crate::reduce::ThreadView::last_seq
//! [`ThreadView::idle_compaction_off`]: crate::reduce::ThreadView::idle_compaction_off
//! [`Registry`]: crate::reduce::Registry
//! [`AttentionStack`]: crate::reduce::AttentionStack

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use serde::de::IgnoredAny;
use serde::Deserialize;

use crate::event::{message_ref, Event, EventBody, Source, HUB_THREAD, JOB_TASK_PREFIX};
use crate::reduce::Registry;

/// The kinds whose `data` [`Row::stored`] reads; of every other kind, the kind is enough.
pub const DATA_KINDS: &[&str] = &[
    "task.updated",
    "account.usage",
    "turn.started",
    "thread.deleted",
    "trash.purged",
    "conversation.update",
];

/// One stored control item as [`select`] reads it: its position, its thread and what of it
/// matters to the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Control gseq.
    pub gseq: u64,
    /// Thread id.
    pub thread: String,
    /// Thread seq.
    pub seq: u64,
    /// What it is.
    pub item: Item,
}

/// What [`select`] reads of an item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// `task.updated`.
    Task(TaskRow),
    /// `git.snapshot`.
    GitSnapshot,
    /// `worktree.disk`.
    WorktreeDisk,
    /// `account.usage` of `runtime`'s `account`.
    AccountUsage {
        /// Runtime.
        runtime: String,
        /// Account (`None`: the runtime's own login).
        account: Option<String>,
    },
    /// `report.ready`: the notifier reads whether the thread has tasks of its own.
    Report,
    /// `thread.started`: drops the runtime's tasks (reading whether it had any) and reopens
    /// task tracking, unless the thread waits for another runtime's start.
    Started,
    /// `thread.stopped`, `thread.parked`, `thread.crashed`, `thread.attached`: drops the
    /// runtime's tasks (reading whether it had any) and closes task tracking.
    Cut,
    /// `thread.resumed`: reopens task tracking.
    Reopen,
    /// `turn.started`: reopens task tracking. `by_message`: started from a message's ref
    /// ([`crate::event::message_ref`]).
    TurnStarted { by_message: bool },
    /// `turn.ended`.
    TurnEnded,
    /// `conversation.update`. `offers`: it turns the thread's offers to compact its context on
    /// or off (`ext.twr.idle_compaction`, [`crate::event::IDLE_COMPACTION_KEY`]).
    ConversationUpdate { offers: bool },
    /// `trash.purged` of trash `item`, which kept `thread`.
    Purged { thread: String, item: String },
    /// `thread.deleted`, `trash`: the trash item it went to.
    Deleted { trash: Option<String> },
    /// `thread.restored`: back as it was, its runtime's tasks dropped, its git snapshot cleared.
    Restored,
    /// Anything else (always kept).
    Other,
}

/// What [`select`] reads of a `task.updated`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TaskRow {
    /// Task id.
    pub task_id: String,
    /// Still running.
    pub running: bool,
    /// Its description is not empty.
    pub described: bool,
    /// A job's update, as the hub records one ([`crate::event::is_job_task`]).
    pub job: bool,
    /// It names a job's process group (`ext.twr.job.pgid`).
    pub group: bool,
    /// It names a job's group members (`ext.twr.job.members`).
    pub members: bool,
}

impl Row {
    /// The row of control item `ev` at `gseq`.
    pub fn of(gseq: u64, ev: &Event) -> Row {
        let item = match &ev.body {
            EventBody::TaskUpdated(p) => {
                let job = |key: &str| {
                    p.ext
                        .as_ref()
                        .and_then(|e| e.get("twr"))
                        .and_then(|t| t.get("job"))
                        .is_some_and(|j| j.get(key).is_some())
                };
                Item::Task(TaskRow {
                    task_id: p.task_id.clone(),
                    running: p.running,
                    described: !p.description.is_empty(),
                    job: crate::event::is_job_task(&ev.source, &ev.body),
                    group: job("pgid"),
                    members: job("members"),
                })
            }
            EventBody::AccountUsage(p) => Item::AccountUsage {
                runtime: p.runtime.clone(),
                account: p.account.clone(),
            },
            EventBody::TurnStarted(p) => Item::TurnStarted {
                by_message: message_ref(p).is_some(),
            },
            EventBody::ThreadDeleted(p) => Item::Deleted {
                trash: p.trash.clone(),
            },
            EventBody::ConversationUpdate(p) => Item::ConversationUpdate {
                offers: p
                    .ext
                    .as_ref()
                    .and_then(|e| e.get("twr"))
                    .and_then(|t| t.get(crate::event::IDLE_COMPACTION_KEY))
                    .is_some_and(serde_json::Value::is_string),
            },
            // A purge the hub recorded (on its thread): the only kind it folds (`hub::trash`).
            EventBody::TrashPurged(p) if ev.thread == HUB_THREAD && ev.source == Source::Hub => {
                Item::Purged {
                    thread: p.thread.clone(),
                    item: p.item.clone(),
                }
            }
            _ => Item::of_kind(ev.kind()),
        };
        Row {
            gseq,
            thread: ev.thread.clone(),
            seq: ev.seq,
            item,
        }
    }

    /// The row of a stored control item: its gseq, thread, seq, kind, source and, for a
    /// `task.updated` or `account.usage`, its `data` (read without decoding the rest). Data
    /// that does not read as its kind is an [`Item::Other`], kept.
    pub fn stored(
        gseq: u64,
        thread: String,
        seq: u64,
        kind: &str,
        source: &str,
        data: Option<&str>,
    ) -> Row {
        let item = match (kind, data) {
            ("task.updated", Some(data)) => match serde_json::from_str::<LightTask>(data) {
                Ok(t) => {
                    let job = t.ext.and_then(|e| e.twr).and_then(|t| t.job);
                    Item::Task(TaskRow {
                        job: source == "hub" && t.task_id.starts_with(JOB_TASK_PREFIX),
                        task_id: t.task_id.into_owned(),
                        running: t.running,
                        described: !t.description.is_empty(),
                        group: job.as_ref().is_some_and(|j| j.pgid.is_some()),
                        members: job.as_ref().is_some_and(|j| j.members.is_some()),
                    })
                }
                Err(_) => Item::Other,
            },
            ("account.usage", Some(data)) => match serde_json::from_str::<LightUsage>(data) {
                Ok(u) => Item::AccountUsage {
                    runtime: u.runtime.into_owned(),
                    account: u.account.map(Cow::into_owned),
                },
                Err(_) => Item::Other,
            },
            ("turn.started", Some(data)) => match serde_json::from_str::<LightTurn>(data) {
                Ok(t) => Item::TurnStarted {
                    by_message: t.by_message(),
                },
                Err(_) => Item::Other,
            },
            ("trash.purged", Some(data)) if thread == HUB_THREAD && source == "hub" => {
                match serde_json::from_str::<LightPurge>(data) {
                    Ok(p) => Item::Purged {
                        thread: p.thread.into_owned(),
                        item: p.item.into_owned(),
                    },
                    Err(_) => Item::Other,
                }
            }
            ("conversation.update", Some(data)) => match serde_json::from_str::<LightTurn>(data) {
                Ok(u) => Item::ConversationUpdate { offers: u.offers() },
                Err(_) => Item::Other,
            },
            ("thread.deleted", Some(data)) => match serde_json::from_str::<LightDeleted>(data) {
                Ok(d) => Item::Deleted {
                    trash: d.trash.map(Cow::into_owned),
                },
                Err(_) => Item::Other,
            },
            (
                "task.updated"
                | "account.usage"
                | "turn.started"
                | "thread.deleted"
                | "trash.purged"
                | "conversation.update",
                None,
            ) => Item::Other,
            _ => Item::of_kind(kind),
        };
        Row {
            gseq,
            thread,
            seq,
            item,
        }
    }

    /// Whether [`Row::stored`] reads `data` of an item of `kind` ([`DATA_KINDS`]).
    pub fn reads_data(kind: &str) -> bool {
        DATA_KINDS.contains(&kind)
    }
}

impl Item {
    /// Whether an item of a purged thread's folds into nothing but its own deleted view and
    /// its own entries of the notifier's memory (module docs): one [`select`] drops.
    fn folds_into_its_view_alone(&self) -> bool {
        match self {
            // A job's end stays: the `!` commands that ended (kept before this is asked).
            Item::Task(_)
            | Item::GitSnapshot
            | Item::WorktreeDisk
            | Item::Report
            | Item::Started
            | Item::Reopen
            | Item::TurnEnded
            | Item::ConversationUpdate { .. } => true,
            Item::TurnStarted { by_message } => !by_message,
            // A stop, park or crash closes the thread's requests (`AttentionStack`).
            Item::Cut
            | Item::AccountUsage { .. }
            | Item::Deleted { .. }
            | Item::Restored
            | Item::Purged { .. } => false,
            Item::Other => false,
        }
    }

    /// The item of a kind whose payload [`select`] does not read.
    fn of_kind(kind: &str) -> Item {
        match kind {
            "git.snapshot" => Item::GitSnapshot,
            "worktree.disk" => Item::WorktreeDisk,
            "report.ready" => Item::Report,
            "thread.started" => Item::Started,
            "thread.stopped" | "thread.parked" | "thread.crashed" | "thread.attached" => Item::Cut,
            "thread.resumed" => Item::Reopen,
            "turn.ended" => Item::TurnEnded,
            "thread.deleted" => Item::Deleted { trash: None },
            "thread.restored" => Item::Restored,
            _ => Item::Other,
        }
    }
}

/// A `task.updated`'s data, as much as [`Row::stored`] reads of it.
#[derive(Deserialize)]
#[serde(default)]
struct LightTask<'a> {
    #[serde(borrow)]
    task_id: Cow<'a, str>,
    running: bool,
    #[serde(borrow)]
    description: Cow<'a, str>,
    ext: Option<LightExt>,
}

impl Default for LightTask<'_> {
    fn default() -> Self {
        LightTask {
            task_id: Cow::Borrowed(""),
            running: false,
            description: Cow::Borrowed(""),
            ext: None,
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LightExt {
    twr: Option<LightTwr>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LightTwr {
    job: Option<LightJob>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LightJob {
    pgid: Option<IgnoredAny>,
    members: Option<IgnoredAny>,
}

/// A `turn.started`'s data, as much as [`Row::stored`] reads of it: whether a message's ref
/// started it (`ext.twr.ref`, [`crate::event::MESSAGE_REF_KEY`]); and a `conversation.update`'s:
/// whether it turns the thread's offers to compact on or off.
/// (Read as loosely as [`crate::event::message_ref`] reads it: any `ext.twr` and `ref`, the
/// ref counting only as an integer.)
#[derive(Deserialize, Default)]
#[serde(default)]
struct LightTurn {
    ext: Option<serde_json::Value>,
}

impl LightTurn {
    /// Whether it turns the thread's offers to compact its context on or off (a
    /// `conversation.update`'s `ext.twr.idle_compaction`, read as loosely as the reducer reads
    /// it: any string).
    fn offers(&self) -> bool {
        self.ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get(crate::event::IDLE_COMPACTION_KEY))
            .is_some_and(serde_json::Value::is_string)
    }

    /// Whether a message's ref started it.
    fn by_message(&self) -> bool {
        self.ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get(crate::event::MESSAGE_REF_KEY))
            .and_then(serde_json::Value::as_i64)
            .is_some()
    }
}

/// A `trash.purged`'s data, as much as [`Row::stored`] reads of it.
#[derive(Deserialize)]
#[serde(default)]
struct LightPurge<'a> {
    #[serde(borrow)]
    thread: Cow<'a, str>,
    #[serde(borrow)]
    item: Cow<'a, str>,
}

impl Default for LightPurge<'_> {
    fn default() -> Self {
        LightPurge {
            thread: Cow::Borrowed(""),
            item: Cow::Borrowed(""),
        }
    }
}

/// A `thread.deleted`'s data, as much as [`Row::stored`] reads of it.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LightDeleted<'a> {
    #[serde(borrow)]
    trash: Option<Cow<'a, str>>,
}

/// An `account.usage`'s data, as much as [`Row::stored`] reads of it.
#[derive(Deserialize)]
#[serde(default)]
struct LightUsage<'a> {
    #[serde(borrow)]
    runtime: Cow<'a, str>,
    #[serde(borrow)]
    account: Option<Cow<'a, str>>,
}

impl Default for LightUsage<'_> {
    fn default() -> Self {
        LightUsage {
            runtime: Cow::Borrowed(""),
            account: None,
        }
    }
}

/// What [`select`] needs of the state at `head`: the tasks present then, and which threads
/// were deleted. Taken with `head` under the hub's lock.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Head {
    /// Each task present at head, by (thread, task id): the thread seq of the update that
    /// started it ([`crate::reduce::BackgroundTask::started_seq`]).
    pub tasks: HashMap<(String, String), u64>,
    /// The threads deleted at head.
    pub deleted: HashSet<String>,
}

impl Head {
    /// What `registry` holds: the tasks of its threads and of its deleted threads' views.
    pub fn of(registry: &Registry) -> Head {
        let mut head = Head::default();
        for view in registry.list() {
            for (id, task) in &view.background_tasks {
                head.tasks
                    .insert((view.id.clone(), id.clone()), task.started_seq);
            }
        }
        for view in registry.deleted_views() {
            head.deleted.insert(view.id.clone());
            for (id, task) in &view.background_tasks {
                head.tasks
                    .insert((view.id.clone(), id.clone()), task.started_seq);
            }
        }
        head
    }
}

/// Selection state of one thread.
#[derive(Default)]
struct ThreadSel<'a> {
    deleted: bool,
    /// Bumped by each lifecycle event: a task's next running update is a candidate again.
    epoch: u64,
    git: Option<usize>,
    disk: Option<usize>,
    /// The runtime's tasks' candidates nothing read yet, whose task has not ended (nor been
    /// dropped) since, by task: their rows. A reader takes them all (each row is looked at
    /// once), an end or a drop those of its task.
    pending: HashMap<&'a str, Vec<usize>>,
    /// The jobs' candidates nothing read yet, likewise: one per job (a job's first running
    /// update after its end always applies, so a later one adds nothing).
    pending_jobs: HashMap<&'a str, usize>,
    /// The runtime's tasks with a candidate since their end, read or not: a drop of them
    /// (a stop, park, crash, attach or restore) leaves no end of theirs to keep.
    open: HashSet<&'a str>,
}

/// Selection state of one task.
#[derive(Default)]
struct TaskSel {
    /// The epoch of its thread when its latest candidate was taken; `None` before any, or
    /// after an end.
    candidate_epoch: Option<u64>,
    /// One of its candidates was kept: its next end is too.
    want_end: bool,
}

/// What of the task present at head is kept: rows of its updates since it started.
#[derive(Default, Clone, Copy)]
struct HeadSel {
    latest: Option<usize>,
    described: Option<usize>,
    group: Option<usize>,
    members: Option<usize>,
}

/// The gseqs of `rows` (the control items of `(since, head]`, in gseq order) a client needs,
/// ascending: see the module docs. `head` is the state at the last of them.
pub fn select(rows: &[Row], since: u64, head: &Head) -> Vec<u64> {
    let rows = &rows[rows.partition_point(|r| r.gseq <= since)..];
    let head_tasks: HashMap<(&str, &str), u64> = head
        .tasks
        .iter()
        .map(|((thread, id), started)| ((thread.as_str(), id.as_str()), *started))
        .collect();
    let mut keep = vec![false; rows.len()];
    let mut threads: HashMap<&str, ThreadSel> = HashMap::new();
    // Whether each thread was deleted at `since`: as its first delete or restore here says,
    // else as it is at head.
    let mut first_seen: HashSet<&str> = HashSet::new();
    for r in rows {
        if first_seen.insert(&r.thread) {
            threads.entry(&r.thread).or_default().deleted = head.deleted.contains(&r.thread);
        }
    }
    // The threads purged from the trash by head: of their items, what folds into no more
    // than their own deleted view goes (module docs).
    // (Purged: deleted at head, and the trash item of its last delete purged since; an older
    // item, kept from a delete the thread was restored from, says nothing of the thread.)
    let mut trashed: HashMap<&str, Option<&str>> = HashMap::new();
    let mut purged: HashSet<&str> = HashSet::new();
    for r in rows {
        match &r.item {
            Item::Deleted { trash } => {
                trashed.insert(&r.thread, trash.as_deref());
            }
            Item::Restored => {
                trashed.remove(r.thread.as_str());
                purged.remove(r.thread.as_str());
            }
            Item::Purged { thread, item }
                if trashed.get(thread.as_str()).copied().flatten() == Some(item.as_str()) =>
            {
                purged.insert(thread.as_str());
            }
            _ => {}
        }
    }
    purged.retain(|t| head.deleted.contains(*t));
    let mut settled: HashSet<&str> = HashSet::new();
    for r in rows {
        if matches!(r.item, Item::Deleted { .. } | Item::Restored) && settled.insert(&r.thread) {
            threads.entry(&r.thread).or_default().deleted = r.item == Item::Restored;
        }
    }
    let mut tasks: HashMap<(&str, &str), TaskSel> = HashMap::new();
    let mut heads: HashMap<(&str, &str), HeadSel> = HashMap::new();
    let mut usage: HashMap<(&str, &str, Option<&str>), usize> = HashMap::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for (i, r) in rows.iter().enumerate() {
        debug_assert!(i == 0 || rows[i - 1].gseq < r.gseq, "rows in gseq order");
        let thread = r.thread.as_str();
        if seen.insert(thread) {
            keep[i] = true;
        }
        if let Item::Task(t) = &r.item {
            if t.job && !t.running {
                keep[i] = true;
            }
        }
        if !keep[i] && purged.contains(thread) && r.item.folds_into_its_view_alone() {
            continue;
        }
        let t = threads.entry(thread).or_default();
        if t.deleted && r.item != Item::Restored {
            // Dropped by the registry; what is no state of a thread's goes on.
            match &r.item {
                Item::Task(_) | Item::GitSnapshot | Item::WorktreeDisk => {}
                Item::AccountUsage { runtime, account } => {
                    usage.insert((thread, runtime, account.as_deref()), i);
                }
                _ => keep[i] = true,
            }
            continue;
        }
        match &r.item {
            Item::Task(task) => {
                if task.task_id.is_empty() {
                    continue;
                }
                let key = (thread, task.task_id.as_str());
                let sel = tasks.entry(key).or_insert_with(|| TaskSel {
                    candidate_epoch: None,
                    // A task present before `since` ends here, perhaps.
                    want_end: since > 0,
                });
                if !task.running {
                    if std::mem::take(&mut sel.want_end) {
                        keep[i] = true;
                    }
                    sel.candidate_epoch = None;
                    t.pending.remove(task.task_id.as_str());
                    t.pending_jobs.remove(task.task_id.as_str());
                    t.open.remove(task.task_id.as_str());
                    continue;
                }
                // Kept already (the thread's first item): it may start the task, so its end
                // is wanted, whatever reads it.
                let forced = keep[i];
                if forced {
                    sel.want_end = true;
                }
                let candidate = match task.job {
                    true => sel.candidate_epoch.is_none(),
                    false => sel.candidate_epoch != Some(t.epoch),
                };
                if candidate {
                    sel.candidate_epoch = Some(t.epoch);
                    if !forced {
                        match task.job {
                            true => {
                                t.pending_jobs.insert(&task.task_id, i);
                            }
                            false => t.pending.entry(&task.task_id).or_default().push(i),
                        }
                    }
                    if !task.job {
                        t.open.insert(&task.task_id);
                    }
                }
                if let Some(&started) = head_tasks.get(&key) {
                    if r.seq == started {
                        keep[i] = true;
                    }
                    if r.seq >= started {
                        let h = heads.entry(key).or_default();
                        h.latest = Some(i);
                        if task.described {
                            h.described = Some(i);
                        }
                        if task.group {
                            h.group = Some(i);
                        }
                        if task.members {
                            h.members = Some(i);
                        }
                    }
                }
            }
            Item::GitSnapshot => t.git = Some(i),
            Item::WorktreeDisk => t.disk = Some(i),
            Item::AccountUsage { runtime, account } => {
                usage.insert((thread, runtime, account.as_deref()), i);
            }
            Item::Report => {
                keep[i] = true;
                read(t, &mut tasks, thread, &mut keep, false);
            }
            Item::Started => {
                keep[i] = true;
                read(t, &mut tasks, thread, &mut keep, true);
                t.epoch += 1;
            }
            Item::Cut | Item::Restored => {
                keep[i] = true;
                read(t, &mut tasks, thread, &mut keep, true);
                // The runtime's tasks go (a job stays, also through a delete and restore).
                // (Taken whole: a cleared table keeps its size, and the next one would scan it.)
                scanned(t.open.capacity());
                for id in std::mem::take(&mut t.open) {
                    if let Some(sel) = tasks.get_mut(&(thread, id)) {
                        sel.want_end = false;
                    }
                }
                t.epoch += 1;
                if r.item == Item::Restored {
                    t.deleted = false;
                    t.git = None;
                }
            }
            Item::Reopen | Item::TurnStarted { .. } => {
                keep[i] = true;
                t.epoch += 1;
            }
            // One that turns the thread's offers on or off is folded; any other is read by no
            // reducer but as a thread's first item (kept above).
            Item::ConversationUpdate { offers } => {
                if *offers {
                    keep[i] = true;
                }
            }
            Item::Deleted { .. } => {
                keep[i] = true;
                t.deleted = true;
                t.epoch += 1;
            }
            Item::TurnEnded | Item::Purged { .. } | Item::Other => keep[i] = true,
        }
    }
    for t in threads.values() {
        for i in t.git.into_iter().chain(t.disk) {
            keep[i] = true;
        }
    }
    for i in usage.into_values() {
        keep[i] = true;
    }
    for h in heads.into_values() {
        for i in [h.latest, h.described, h.group, h.members]
            .into_iter()
            .flatten()
        {
            keep[i] = true;
        }
    }
    rows.iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(r, _)| r.gseq)
        .collect()
}

/// Something reads `thread`'s tasks (`t`): every candidate that may have started one still
/// there is kept, and so is that task's next end. `runtime`: only the runtime's tasks are
/// read, not jobs. Each candidate is taken once: the work is the candidates' number.
fn read<'a>(
    t: &mut ThreadSel<'a>,
    tasks: &mut HashMap<(&'a str, &'a str), TaskSel>,
    thread: &'a str,
    keep: &mut [bool],
    runtime: bool,
) {
    let mut want_end = |id: &'a str| {
        if let Some(sel) = tasks.get_mut(&(thread, id)) {
            sel.want_end = true;
        }
    };
    // Taken whole, not drained: a drained table keeps its size, so a thread that once had
    // many tasks would scan that much at each read.
    scanned(t.pending.capacity());
    for (id, rows) in std::mem::take(&mut t.pending) {
        for i in rows {
            keep[i] = true;
        }
        want_end(id);
    }
    if !runtime {
        scanned(t.pending_jobs.capacity());
        for (id, i) in std::mem::take(&mut t.pending_jobs) {
            keep[i] = true;
            want_end(id);
        }
    }
}

/// The room of the tables the selection went through (tests: [`SCANNED`]).
fn scanned(_room: usize) {
    #[cfg(test)]
    SCANNED.with(|s| s.set(s.get() + _room));
}

#[cfg(test)]
thread_local! {
    /// What [`scanned`] counted on this thread.
    static SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The threads `events` (control items in log order) purge from the trash, as [`select`] tells
/// them: deleted at the end, and the trash item of their last delete purged since by the hub
/// (a `trash.purged` on its thread, of its source: the only one it folds).
pub fn purged_threads<'a>(events: impl IntoIterator<Item = &'a Event>) -> HashSet<String> {
    let mut trashed: HashMap<String, Option<String>> = HashMap::new();
    let mut purged = HashSet::new();
    for ev in events {
        match &ev.body {
            EventBody::ThreadDeleted(d) => {
                trashed.insert(ev.thread.clone(), d.trash.clone());
            }
            EventBody::ThreadRestored(_) => {
                trashed.remove(&ev.thread);
                purged.remove(&ev.thread);
            }
            EventBody::TrashPurged(p)
                if ev.thread == HUB_THREAD
                    && ev.source == Source::Hub
                    && trashed.get(&p.thread).and_then(Option::as_deref)
                        == Some(p.item.as_str()) =>
            {
                purged.insert(p.thread.clone());
            }
            _ => {}
        }
    }
    purged
}

/// [`select`] over control items as events, with [`Head::of`] the registry they fold into:
/// what a hub would keep of them. For tests and benchmarks.
pub fn select_events<'a>(
    items: impl IntoIterator<Item = (u64, &'a Event)>,
    since: u64,
) -> Vec<u64> {
    let mut registry = Registry::new();
    let mut rows = Vec::new();
    for (gseq, ev) in items {
        registry.apply(ev);
        rows.push(Row::of(gseq, ev));
    }
    select(&rows, since, &Head::of(&registry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::*;
    use crate::notify::{NotifyEvent, Policy};
    use crate::reduce::{AttentionStack, ThreadView};
    use crate::testkit::{compaction_cases, compaction_log};
    use crate::usage::UsageView;

    /// What the clients fold from the control stream, without the fields no client reads.
    #[derive(Debug, Clone, PartialEq)]
    struct Folded {
        registry: Registry,
        attention: AttentionStack,
        usage: UsageView,
        policy: Policy,
    }

    impl Folded {
        fn new() -> Folded {
            Folded {
                registry: Registry::new(),
                attention: AttentionStack::new(),
                usage: UsageView::default(),
                policy: Policy::new(&[
                    NotifyEvent::Permission,
                    NotifyEvent::Stalled,
                    NotifyEvent::Report,
                    NotifyEvent::Reply,
                ]),
            }
        }

        fn apply(&mut self, gseq: u64, ev: &Event) {
            self.registry.apply(ev);
            self.attention.apply_at(ev, Some(gseq));
            self.usage.apply(ev);
            self.policy
                .recall(ev, Some(gseq), &self.registry, &self.attention);
        }

        /// The thread views, deleted ones after, without what no client reads: of a purged
        /// thread (in `purged`), who it was only ([`identity`]).
        fn views(&self, purged: &HashSet<String>) -> (Vec<ThreadView>, Vec<ThreadView>) {
            let strip = |v: &ThreadView| match purged.contains(&v.id) {
                true => identity(v),
                false => ThreadView {
                    last_activity: None,
                    worked_at: None,
                    last_seq: 0,
                    ..v.clone()
                },
            };
            let mut deleted: Vec<ThreadView> = self.registry.deleted_views().map(strip).collect();
            deleted.sort_by(|a, b| a.id.cmp(&b.id));
            (
                self.registry.list().into_iter().map(strip).collect(),
                deleted,
            )
        }
    }

    /// Who thread view `v` was: what of a purged thread's deleted view the compacted backlog
    /// keeps (module docs).
    pub(crate) fn identity(v: &ThreadView) -> ThreadView {
        let mut t = ThreadView::new(v.id.clone());
        t.name.clone_from(&v.name);
        t.aliases.clone_from(&v.aliases);
        t.former_names.clone_from(&v.former_names);
        t.role.clone_from(&v.role);
        t.color = v.color;
        t.created_at = v.created_at;
        t
    }

    /// The threads `log` purges from the trash ([`purged_threads`]).
    pub(crate) fn purged_in(log: &[Event]) -> HashSet<String> {
        purged_threads(log)
    }

    /// Folds `log` in full and compacted after `since` (in full up to it); panics, naming
    /// `what`, where they differ. Returns how many items the compacted backlog has.
    fn check(what: &str, log: &[Event], since: u64) -> usize {
        let purged = purged_in(log);
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        let kept = select_events(items.iter().copied(), since);
        assert!(
            kept.windows(2).all(|w| w[0] < w[1]),
            "{what}: kept in order"
        );
        let mut full = Folded::new();
        let mut compacted = Folded::new();
        for &(g, ev) in &items {
            full.apply(g, ev);
            if g <= since || kept.binary_search(&g).is_ok() {
                compacted.apply(g, ev);
            }
        }
        assert_eq!(
            full.views(&purged),
            compacted.views(&purged),
            "{what} (since {since}): views"
        );
        assert_eq!(full.attention, compacted.attention, "{what}: attention");
        assert_eq!(full.usage, compacted.usage, "{what}: usage");
        assert_eq!(
            full.policy.without_threads(&purged),
            compacted.policy.without_threads(&purged),
            "{what}: notifier"
        );
        kept.len()
    }

    #[test]
    fn the_cases_fold_alike() {
        for (name, log) in compaction_cases() {
            for since in 0..log.len() as u64 {
                check(name, &log, since);
            }
        }
    }

    #[test]
    fn made_up_logs_fold_alike_and_keep_less() {
        let (mut all, mut kept) = (0, 0);
        for seed in 0..300 {
            let log = compaction_log(seed, 400);
            all += log.len();
            kept += check(&format!("seed {seed}"), &log, 0);
            for since in [1, 57, 200, 399] {
                check(&format!("seed {seed}"), &log, since);
            }
        }
        assert!(kept < all * 9 / 10, "kept {kept} of {all}");
    }

    /// Every `account.added`, `account.removed` and `account.status` stays (the accounts are
    /// folded from them, `crate::account::AccountsView`, which the hub's `account.list` hands a
    /// client), and of a runtime's account's `account.usage` the latest, a clear (no windows,
    /// from a removal) included: the usage view and the accounts fold alike from either.
    #[test]
    fn account_events_stay_with_their_latest_usage() {
        use crate::account::{AccountsView, Login};
        let mut h = crate::testkit::ThreadScript::new(HUB_THREAD);
        let mut ev = |body: EventBody| by_hub(h.event(body));
        let added = |name: &str, dir: Option<&str>, native: bool| {
            EventBody::AccountAdded(AccountAdded {
                runtime: "claude-code".into(),
                account: name.into(),
                dir: dir.map(str::to_string),
                native,
                ..Default::default()
            })
        };
        let usage = |pct: Option<f64>, dir: Option<&str>| {
            EventBody::AccountUsage(AccountUsage {
                runtime: "claude-code".into(),
                account: Some("work".into()),
                windows: pct
                    .map(|p| {
                        vec![crate::usage::UsageWindow {
                            window: "5h".into(),
                            used_pct: p,
                            resets_at: None,
                            minutes: Some(300),
                            observed_at: None,
                        }]
                    })
                    .unwrap_or_default(),
                source: "event".into(),
                dir: dir.map(str::to_string),
                ..Default::default()
            })
        };
        let status = |login: Login| {
            EventBody::AccountStatus(AccountStatus {
                runtime: "claude-code".into(),
                account: "work".into(),
                login,
                ..Default::default()
            })
        };
        let log = vec![
            ev(added("work", Some("/a/work"), false)), // 1
            ev(usage(Some(40.0), Some("/a/work"))),    // 2
            ev(status(Login::LoggedIn)),               // 3
            ev(usage(Some(50.0), Some("/a/work"))),    // 4
            ev(EventBody::AccountRemoved(AccountRemoved {
                runtime: "claude-code".into(),
                account: "work".into(),
                ..Default::default()
            })), // 5
            ev(usage(None, None)),                     // 6
            ev(added("work", Some("/a/work2"), false)), // 7
            ev(status(Login::LoggedOut)),              // 8
            ev(added("personal", None, true)),         // 9
        ];
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        let kept = select_events(items.iter().copied(), 0);
        assert_eq!(kept, vec![1, 3, 5, 6, 7, 8, 9]);
        for since in 0..log.len() as u64 {
            check("accounts", &log, since);
            let kept = select_events(items.iter().copied(), since);
            let fold = |all: bool| {
                let mut v = AccountsView::default();
                for &(g, e) in &items {
                    if all || g <= since || kept.binary_search(&g).is_ok() {
                        v.apply(e);
                    }
                }
                v
            };
            assert_eq!(fold(true), fold(false), "accounts (since {since})");
        }
    }

    /// A `conversation.update` that turns a thread's offers to compact its context off or on is
    /// folded (`ThreadView::idle_compaction_off`), so it stays, whatever it comes after; any
    /// other goes unless it is its thread's first item.
    #[test]
    fn an_offers_switch_stays() {
        let mut s = crate::testkit::ThreadScript::new("w");
        let offers = |s: &mut crate::testkit::ThreadScript, v: &str| {
            let mut ev = s.event(EventBody::ConversationUpdate(ConversationUpdate::default()));
            if let EventBody::ConversationUpdate(p) = &mut ev.body {
                p.ext = serde_json::json!({"twr": {crate::event::IDLE_COMPACTION_KEY: v}})
                    .as_object()
                    .cloned();
            }
            by_hub(ev)
        };
        let log = vec![
            s.created("w"),                                                        // 1
            s.event(EventBody::ConversationUpdate(ConversationUpdate::default())), // 2
            offers(&mut s, "off"),                                                 // 3
            s.event(EventBody::ConversationUpdate(ConversationUpdate::default())), // 4
            offers(&mut s, "on"),                                                  // 5
            offers(&mut s, "off"),                                                 // 6
        ];
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        assert_eq!(select_events(items, 0), vec![1, 3, 5, 6]);
        check("offers", &log, 0);
        check("offers", &log, 3);
        check("offers", &log, 5);
    }

    /// Of a thread purged from the trash, what folds into nothing but its own view goes; its
    /// lifecycle, decisions, requests, message turns and jobs' ends stay, and so does a later
    /// thread's color. A `conversation.update` goes unless it is its thread's first item.
    #[test]
    fn a_purged_thread_keeps_who_it_was_and_what_others_read() {
        let mut s = crate::testkit::ThreadScript::new("w");
        let mut hub = crate::testkit::ThreadScript::new(HUB_THREAD);
        let mut o = crate::testkit::ThreadScript::new("o");
        let mut ref_turn = s.turn_started("from a message");
        if let EventBody::TurnStarted(p) = &mut ref_turn.body {
            p.ext = serde_json::json!({"twr": {"ref": 7}}).as_object().cloned();
        }
        let mut job_end = s.event(EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("{JOB_TASK_PREFIX}j"),
            running: false,
            ..Default::default()
        }));
        job_end.source = Source::Hub;
        let log = vec![
            o.event(EventBody::ConversationUpdate(ConversationUpdate::default())), // 1: first
            o.event(EventBody::ConversationUpdate(ConversationUpdate::default())), // 2
            s.created("w"),                                                        // 3
            s.turn_started("go"),                                                  // 4
            s.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: "t".into(),
                running: true,
                ..Default::default()
            })), // 5
            s.raise("a", AttentionReason::Permission),                             // 6
            s.event(EventBody::DecisionMade(DecisionMade::default())),             // 7
            s.git("main", 1),                                                      // 8
            s.turn_ended(TurnResult::Completed),                                   // 9
            s.report(Some(1), Grade::Success),                                     // 10
            ref_turn,                                                              // 11
            job_end,                                                               // 12
            s.event(EventBody::ThreadDeleted(ThreadDeleted {
                name: "w".into(),
                trash: Some("i".into()),
                ..Default::default()
            })), // 13
            by_hub(hub.event(EventBody::TrashPurged(TrashPurged {
                item: "i".into(),
                thread: "w".into(),
                ..Default::default()
            }))), // 14
            crate::testkit::ThreadScript::new("x").created("x"),                   // 15
        ];
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        assert_eq!(
            select_events(items, 0),
            vec![1, 3, 6, 7, 11, 12, 13, 14, 15]
        );
        check("purged", &log, 0);
        // The stored rows read alike.
        for (ev, g) in log.iter().zip(1u64..) {
            let data = ev.body.data().unwrap().to_string();
            let stored = Row::stored(
                g,
                ev.thread.clone(),
                ev.seq,
                ev.kind(),
                &ev.source.to_string(),
                Row::reads_data(ev.kind()).then_some(data.as_str()),
            );
            assert_eq!(stored, Row::of(g, ev), "{}", ev.kind());
        }
    }

    /// A purge of an older trash item, from a delete the thread was restored from, says nothing
    /// of the thread: alive, or deleted again into another item, it keeps its items.
    #[test]
    fn a_purge_of_an_older_trash_item_purges_no_thread() {
        let mut s = crate::testkit::ThreadScript::new("w");
        let mut hub = crate::testkit::ThreadScript::new(HUB_THREAD);
        let deleted = |s: &mut crate::testkit::ThreadScript, item: &str| {
            s.event(EventBody::ThreadDeleted(ThreadDeleted {
                name: "w".into(),
                trash: Some(item.into()),
                ..Default::default()
            }))
        };
        let purge = |hub: &mut crate::testkit::ThreadScript, item: &str| {
            by_hub(hub.event(EventBody::TrashPurged(TrashPurged {
                item: item.into(),
                thread: "w".into(),
                ..Default::default()
            })))
        };
        let mut log = vec![
            s.created("w"),
            deleted(&mut s, "i1"),
            s.event(EventBody::ThreadRestored(ThreadRestored {
                item: "i1".into(),
                name: "w".into(),
                ..Default::default()
            })),
            s.turn_started("go"),
            purge(&mut hub, "i1"),
        ];
        assert!(purged_threads(&log).is_empty());
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        assert!(select_events(items, 0).contains(&4), "its turn start stays");
        check("restored, older item purged", &log, 0);
        log.push(s.turn_ended(TurnResult::Completed));
        log.push(deleted(&mut s, "i2"));
        log.push(purge(&mut hub, "i1"));
        assert!(purged_threads(&log).is_empty());
        check("deleted again, older item purged", &log, 0);
        // Not recorded by the hub: the hub folds no such purge, and the thread can come back.
        let mut forged = purge(&mut hub, "i2");
        forged.source = Source::User("tui".into());
        log.push(forged);
        assert!(purged_threads(&log).is_empty());
        let g = log.len() as u64;
        let forged = log.last().unwrap();
        assert_eq!(stored_row(g, forged).item, Item::Other);
        assert_eq!(Row::of(g, forged).item, Item::Other);
        check("deleted again, its item purged by no hub", &log, 0);
        log.push(purge(&mut hub, "i2"));
        assert_eq!(purged_threads(&log), HashSet::from(["w".to_string()]));
        check("deleted again, its item purged", &log, 0);
    }

    /// A `turn.started` whose `ext.twr` or ref is odd (not an object, a ref that is no integer)
    /// reads the same from the store as decoded.
    #[test]
    fn odd_message_refs_read_alike() {
        let mut s = crate::testkit::ThreadScript::new("w");
        for ext in [
            serde_json::json!({"twr": {"ref": "7"}}),
            serde_json::json!({"twr": {"ref": 7.5}}),
            serde_json::json!({"twr": {"ref": 7}}),
            serde_json::json!({"twr": "x"}),
            serde_json::json!({"twr": null}),
        ] {
            let mut ev = s.turn_started("go");
            if let EventBody::TurnStarted(p) = &mut ev.body {
                p.ext = ext.as_object().cloned();
            }
            assert_eq!(stored_row(1, &ev), Row::of(1, &ev), "{ext}");
        }
    }

    /// A purge recorded by the hub: on its thread, of its source.
    fn by_hub(mut ev: Event) -> Event {
        ev.source = Source::Hub;
        ev
    }

    /// `ev`'s row at `gseq` as [`Row::stored`] reads it from the store.
    fn stored_row(g: u64, ev: &Event) -> Row {
        let data = ev.body.data().unwrap().to_string();
        Row::stored(
            g,
            ev.thread.clone(),
            ev.seq,
            ev.kind(),
            &ev.source.to_string(),
            Row::reads_data(ev.kind()).then_some(data.as_str()),
        )
    }

    /// A task that ended before anything read it is gone from the backlog, its progress too;
    /// one running at head keeps its start and its latest; of snapshots, the latest.
    #[test]
    fn what_goes_and_what_stays() {
        let mut s = crate::testkit::ThreadScript::new("w");
        let task = |s: &mut crate::testkit::ThreadScript, id: &str, running, d: &str| {
            s.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: id.into(),
                running,
                description: d.into(),
                ..Default::default()
            }))
        };
        let log = vec![
            s.created("w"),                // 1
            s.turn_started("go"),          // 2
            task(&mut s, "a", true, "x"),  // 3
            task(&mut s, "a", true, "y"),  // 4
            task(&mut s, "a", false, ""),  // 5
            task(&mut s, "b", true, "b1"), // 6
            task(&mut s, "b", true, "b2"), // 7
            task(&mut s, "b", true, ""),   // 8
            task(&mut s, "b", true, ""),   // 9
            s.git("main", 1),              // 10
            s.git("main", 2),              // 11
        ];
        let items: Vec<(u64, &Event)> = log.iter().zip(1..).map(|(e, g)| (g, e)).collect();
        assert_eq!(select_events(items, 0), vec![1, 2, 6, 7, 9, 11]);
    }

    /// The work is linear in the log: 20 000 tasks and 20 000 jobs started in one turn and then
    /// ended, then a job running through 50 000 turns, their reports and stops, select at
    /// once (each candidate looked at once, an end finding its own, and no read scanning the
    /// room the earlier tasks took).
    #[test]
    fn long_lives_select_in_linear_time() {
        let mut s = crate::testkit::ThreadScript::new("w");
        let mut log = vec![s.created("w"), s.started("s"), s.turn_started("many")];
        let job = |s: &mut crate::testkit::ThreadScript, id: String, running: bool| {
            let mut ev = s.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: format!("{JOB_TASK_PREFIX}{id}"),
                running,
                heartbeat: true,
                ..Default::default()
            }));
            ev.source = Source::Hub;
            ev
        };
        for running in [true, false] {
            for n in 0..20_000 {
                log.push(s.event(EventBody::TaskUpdated(TaskUpdated {
                    task_id: format!("t{n}"),
                    running,
                    ..Default::default()
                })));
                log.push(job(&mut s, format!("K{n}"), running));
            }
        }
        log.push(s.turn_ended(TurnResult::Completed));
        for turn in 2..=50_001 {
            log.push(s.turn_started("go"));
            log.push(job(&mut s, "J".into(), true));
            log.push(s.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: "bg".into(),
                running: true,
                ..Default::default()
            })));
            log.push(s.turn_ended(TurnResult::Completed));
            log.push(s.report(Some(turn), Grade::Success));
            log.push(s.stopped());
        }
        let rows: Vec<Row> = log.iter().zip(1..).map(|(e, g)| Row::of(g, e)).collect();
        SCANNED.with(|s| s.set(0));
        let t = std::time::Instant::now();
        let kept = select(&rows, 0, &Head::default());
        let took = t.elapsed();
        assert!(kept.len() > 200_000);
        // Tables kept at the size the first turn's tasks gave them, scanned at each of the
        // 50 000 reads, came to billions.
        let scanned = SCANNED.with(|s| s.get());
        assert!(
            scanned < 4 * rows.len(),
            "{scanned} for {} rows",
            rows.len()
        );
        // Quadratic, it took a minute; linear, a second at most (a debug build).
        assert!(took < std::time::Duration::from_secs(10), "{took:?}");
    }

    /// What the hub reads of a stored row is what the event says.
    #[test]
    fn stored_rows_read_as_their_events() {
        let mut checked = 0;
        let mut logs: Vec<Vec<Event>> = compaction_cases().into_iter().map(|(_, l)| l).collect();
        logs.push(compaction_log(7, 2000));
        for ev in logs.iter().flatten() {
            let kind = ev.kind();
            let data = ev.body.data().unwrap().to_string();
            let stored = Row::stored(
                3,
                ev.thread.clone(),
                ev.seq,
                kind,
                &ev.source.to_string(),
                Row::reads_data(kind).then_some(data.as_str()),
            );
            assert_eq!(stored, Row::of(3, ev), "{kind}");
            checked += 1;
        }
        assert!(checked > 2000);
        // Data that does not read is kept.
        let odd = Row::stored(1, "w".into(), 1, "task.updated", "hub", Some("[1]"));
        assert_eq!(odd.item, Item::Other);
    }
}

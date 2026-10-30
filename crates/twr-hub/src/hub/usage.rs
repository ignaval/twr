//! Subscription usage (`twr_core::usage`): the latest usage windows per runtime and account.
//!
//! An adapter reports what its runtime says as `runtime.limits` (`twr_adapters::usage`). The
//! hub does not record that event on the thread: [`Core::record_usage`] merges its windows into
//! what is known of the reporting runtime's account (the windows a report leaves out are kept
//! until they reset) and appends `account.usage` on the hub thread when the merge says
//! something new (another set of windows, a whole percent or a reset time that moved, another
//! plan) or the last record is [`REFRESH_AFTER`] old, so a client can tell how fresh it is.
//! Every client folds `account.usage` from the control stream ([`UsageView`]), and so does the
//! hub, live and on replay ([`Core::apply`]); `usage.get` ([`Hub::usage`]) answers what it
//! knows.
//!
//! The reporting runtime is the one the session was started as (`ThreadRt::runtime_settings`),
//! not the thread's runtime now: a thread moved to another runtime keeps its previous one
//! running until its turn ends, and what that one reports is its own. Until the hub knows it
//! (a remote session taken over after a restart, whose `thread.started` is not recorded yet)
//! a report is dropped: the session's next one comes after. Observation times are
//! the reports' (a runner's capture time), not the record's `ts`, which the hub thread's order
//! may push later: each window keeps its newest report (`observed_at` on the window; the
//! record's is the newest report folded so far), so a runner's replayed report updates only
//! the windows it has newer, and its plan only when it is not older than the last record. The 10-minute
//! refresh is per window: a record is due once any window's report is that much newer than
//! the one recorded.
//!
//! Accounts: the entry is keyed by runtime and account, the account the reporting session was
//! started on (`RunSettings::account`; absent: the runtime's native account), not the thread's
//! now: a thread moved to another account keeps its previous session running until its turn
//! ends. A report of a session whose account's name leads to another directory now (removed and
//! added again elsewhere: `RunSettings::account_dir`), or was removed, is dropped: it is not
//! the account's now. `usage.get`'s labels name the account by the accounts' rule ([`usage_json`]): while
//! the runtime shows account labels (`AccountsView::shows_labels`), the native account under
//! its current name.
//!
//! Directories: a named account's record says the directory its session ran in
//! (`AccountUsage::dir`, links followed), and a report merges into the known windows only when
//! they were read in the reporting session's directory (else it starts afresh: a sparse report
//! of the account's new login never keeps the old login's windows). When a named account's
//! directory changes (removed, added again elsewhere, or config names another one at a start)
//! the hub records an `account.usage` with no windows for it ([`Core::usage_clears`], in the
//! batch of the `account.added` / `account.removed` that changed it; [`Core::clear_moved_usage`]
//! at a start), so every client's [`UsageView`] drops what it showed. `usage.get` also leaves
//! out an entry read in another directory than the account's now ([`moved`]), whatever the
//! log says. An entry without a directory (the native account's, an older hub's) is kept.

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::account::AccountsView;
use twr_core::event::{AccountUsage, EventBody, RuntimeLimits, Source, Usage, HUB_THREAD};
use twr_core::usage::{changed, merge_windows, Spent, UsageEntry, UsageView, REFRESH_AFTER};

use super::accounts::same_dir;
use super::{store, Batch, BatchEvent, Core, EndedTurn, Hub, Inner};

impl Core {
    /// Folds `thread`'s runtime's usage report `limits` (see the module docs), committing the
    /// runner frame `ack` with what it records, or alone.
    pub(super) fn record_usage(
        &mut self,
        hub: &Inner,
        thread: &str,
        limits: RuntimeLimits,
        ack: Option<&store::RunnerAck>,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Option<EndedTurn>> {
        let started_as = self
            .rt
            .get(thread)
            .and_then(|r| r.runtime_settings.as_ref())
            .map(|s| (s.runtime.clone(), s.account.clone(), s.account_dir.clone()));
        let known = self.registry.get(thread).is_some();
        let Some((runtime, account, dir)) = started_as.filter(|(r, _, _)| known && !r.is_empty())
        else {
            tracing::debug!(
                thread,
                "a usage report of a session whose runtime is not known yet; dropped"
            );
            self.commit_ack(hub, ack)?;
            return Ok(None);
        };
        // A session on an account its name no longer leads to (removed, or added again
        // elsewhere): its usage is not the account's now.
        if let Some(why) = self.session_account_gone(&runtime, account.as_deref(), dir.as_deref()) {
            tracing::debug!(
                thread,
                why,
                "a usage report of an account's previous directory; dropped"
            );
            self.commit_ack(hub, ack)?;
            return Ok(None);
        }
        let now = Utc::now();
        let when = at.unwrap_or(now).min(now);
        // The session's directory, as recorded with the windows: a named account's only.
        let dir = account
            .as_ref()
            .and(dir)
            .map(|d| d.to_string_lossy().into_owned());
        // What is known of the account, if it was read in this directory: another directory's
        // windows are another login's.
        let prev = self
            .usage
            .get(&runtime, account.as_deref())
            .filter(|p| same_entry_dir(p.dir.as_deref(), dir.as_deref()));
        let reported: Vec<_> = limits
            .windows
            .into_iter()
            .map(|w| twr_core::usage::UsageWindow {
                observed_at: Some(when),
                ..w
            })
            .collect();
        let windows = merge_windows(prev.map_or(&[][..], |p| &p.windows[..]), &reported, now);
        // The newest report folded so far: never earlier than the last record's, also when the
        // windows that report named have reset since.
        let observed_at = prev.map_or(when, |p| p.observed_at.max(when));
        // The plan as the newest report says it: an older report's is not taken.
        let plan = match prev {
            Some(p) if when < p.observed_at => p.plan.clone(),
            _ => limits.plan.or_else(|| prev.and_then(|p| p.plan.clone())),
        };
        let refreshed = |p: &twr_core::usage::UsageEntry| {
            windows.iter().any(|w| {
                let before = p
                    .windows
                    .iter()
                    .find(|o| o.window == w.window)
                    .and_then(|o| o.observed_at);
                match (w.observed_at, before) {
                    (Some(n), Some(o)) => n - o >= REFRESH_AFTER,
                    (Some(n), None) => n - p.observed_at >= REFRESH_AFTER,
                    _ => false,
                }
            })
        };
        let due = match prev {
            None => !windows.is_empty(),
            Some(p) => changed(&p.windows, &windows) || p.plan != plan || refreshed(p),
        };
        if !due {
            self.commit_ack(hub, ack)?;
            return Ok(None);
        }
        let body = EventBody::AccountUsage(AccountUsage {
            runtime,
            account,
            windows,
            source: limits.source,
            plan,
            thread: Some(thread.to_string()),
            observed_at: Some(observed_at),
            dir,
            ..Default::default()
        });
        let mut batch = Batch {
            runner_ack: ack.cloned(),
            at,
            ..Batch::default()
        };
        batch.push(HUB_THREAD, None, Source::Hub, body);
        self.append_batch(hub, batch)?;
        Ok(None)
    }

    /// The `account.usage` records with no windows that `events` (a batch about to be
    /// committed) call for: one per named account whose known usage was read in another
    /// directory than the one the accounts' registry gives it once the batch's
    /// `account.added` / `account.removed` are folded (see the module docs). Nothing for a
    /// batch without either.
    pub(super) fn usage_clears(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let changes = |e: &&BatchEvent| {
            e.thread == HUB_THREAD
                && matches!(
                    e.body,
                    EventBody::AccountAdded(_) | EventBody::AccountRemoved(_)
                )
        };
        if !events.iter().any(|e| changes(&e)) {
            return Vec::new();
        }
        let mut after = self.accounts.clone();
        for (i, e) in events.iter().filter(changes).enumerate() {
            after.apply(&twr_core::event::Event::new(
                HUB_THREAD,
                i as u64 + 1,
                None,
                e.source.clone(),
                e.body.clone(),
            ));
        }
        // An entry this batch clears already (or that a record of the batch replaces) is left
        // to it.
        let recorded = |runtime: &str, account: Option<&str>| {
            events.iter().any(|e| {
                matches!(&e.body, EventBody::AccountUsage(p)
                    if e.thread == HUB_THREAD
                        && p.runtime == runtime
                        && p.account.as_deref() == account)
            })
        };
        self.usage
            .entries()
            .filter(|e| moved(e, &after) && !recorded(&e.runtime, e.account.as_deref()))
            .map(|e| BatchEvent {
                thread: HUB_THREAD.to_string(),
                turn: None,
                source: Source::Hub,
                body: cleared(e),
                raw: None,
            })
            .collect()
    }

    /// Startup: clears the known usage of each named account whose directory is another than
    /// the one it was read in (config names another directory now, or a log from before the
    /// clearing was part of the account's change). One append.
    pub(super) fn clear_moved_usage(&mut self, hub: &Inner) -> anyhow::Result<()> {
        let mut batch = Batch::default();
        for e in self.usage.entries().filter(|e| moved(e, &self.accounts)) {
            batch.push(HUB_THREAD, None, Source::Hub, cleared(e));
        }
        if batch.events.is_empty() {
            return Ok(());
        }
        self.append_batch(hub, batch).map(drop)
    }
}

/// Whether windows read in directory `known` (`None`: the native account's, or an older hub's
/// record) may take a report of a session in `dir`: the same directory, or neither has one.
fn same_entry_dir(known: Option<&str>, dir: Option<&str>) -> bool {
    match (known, dir) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b || same_dir(std::path::Path::new(a), std::path::Path::new(b)),
        _ => false,
    }
}

/// Whether entry `e` was read in another directory than the one `accounts` gives its named
/// account now (or the account is removed, or its name is the native account's now). An entry
/// without a directory, or with no windows left to show, is not.
pub(super) fn moved(e: &UsageEntry, accounts: &AccountsView) -> bool {
    let (Some(name), Some(dir)) = (e.account.as_deref(), e.dir.as_deref()) else {
        return false;
    };
    if e.windows.is_empty() {
        return false;
    }
    let now = match accounts.resolve(&e.runtime, name) {
        Some(Some(_)) => accounts
            .info(&e.runtime, Some(name))
            .filter(|a| !a.native && !a.removed)
            .and_then(|a| a.dir),
        _ => None,
    };
    now.is_none_or(|now| {
        now.as_os_str() != std::ffi::OsStr::new(dir) && !same_dir(&now, std::path::Path::new(dir))
    })
}

/// The `account.usage` that clears entry `e`: no windows, no directory.
fn cleared(e: &UsageEntry) -> EventBody {
    EventBody::AccountUsage(AccountUsage {
        runtime: e.runtime.clone(),
        account: e.account.clone(),
        source: e.source.clone(),
        ..Default::default()
    })
}

/// What `usage.get` answers for `view` at `now`: per runtime and account, the windows that
/// have not reset, with their labels, and the data's age. The labels name the account as the
/// accounts' registry `accounts` says threads' do (`UsageView::shown_for`: while the runtime
/// shows labels, the native account under its current name); without one, as an older hub's
/// did (`UsageView::shown`).
pub fn usage_json(view: &UsageView, accounts: Option<&AccountsView>, now: DateTime<Utc>) -> Value {
    let shown = match accounts {
        // Not an entry read in another directory than its account's now ([`moved`]).
        Some(a) => {
            let mut view = view.clone();
            view.retain(|e| !moved(e, a));
            view.shown_for(now, a)
        }
        None => view.shown(now),
    };
    let runtimes: Vec<Value> = shown
        .into_iter()
        .map(|s| {
            let windows: Vec<Value> = s
                .windows
                .iter()
                .map(|w| {
                    json!({
                        "window": w.window,
                        "label": w.label(),
                        "used_pct": w.used_pct,
                        "remaining_pct": twr_core::usage::remaining_pct(w.used_pct),
                        "resets_at": w.resets_at,
                        "minutes": w.length(),
                    })
                })
                .collect();
            json!({
                "runtime": s.runtime,
                "account": s.account,
                "label": s.label,
                "plan": s.plan,
                "windows": windows,
                "observed_at": s.observed_at,
                "age_secs": s.age.num_seconds(),
                "stale": s.stale,
                "source": s.source,
            })
        })
        .collect();
    json!({ "runtimes": runtimes, "now": now })
}

impl Hub {
    /// `usage.get`: the latest known usage of each runtime's account ([`usage_json`]).
    pub fn usage(&self) -> Value {
        let core = self.inner.core();
        usage_json(&core.usage, Some(&core.accounts), Utc::now())
    }

    /// `usage.threads {since?, by?}`: what the turns spent ([`Spent`]), summed from the log's
    /// `turn.ended` events since `since` (all of them without it), grouped `by` thread (the
    /// default), project or runtime (the adapter that reported the turn). Deleted threads
    /// count too. `{since, by, rows: [{key, label, input, output, cost, turns, costed_turns}],
    /// total}`, the rows by cost, then tokens, the most first; `cost` is null when no turn of
    /// the row reported one.
    pub async fn usage_threads(
        &self,
        since: Option<DateTime<Utc>>,
        by: SpentBy,
    ) -> anyhow::Result<Value> {
        // The scan runs on the reader connection, off the async workers and without the core:
        // the log may be long. A second of margin in SQL (timestamps are compared as text),
        // the exact window below.
        let inner = std::sync::Arc::clone(&self.inner);
        let from = since.map(|t| {
            t.checked_sub_signed(chrono::TimeDelta::seconds(1))
                .unwrap_or(t)
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
        });
        let turns: Vec<(String, String, Usage)> = tokio::task::spawn_blocking(move || {
            let conn = inner.reader();
            let mut stmt = conn.prepare_cached(
                "SELECT thread, ts, source, data FROM events \
                 WHERE kind = 'turn.ended' AND (?1 IS NULL OR ts >= ?1)",
            )?;
            let mut rows = stmt.query([from])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? {
                let (thread, ts, source, data): (String, String, String, String) =
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?);
                if let Some(since) = since {
                    match DateTime::parse_from_rfc3339(&ts) {
                        Ok(at) if at.with_timezone(&Utc) >= since => {}
                        _ => continue,
                    }
                }
                let Ok(data) = serde_json::from_str::<Value>(&data) else {
                    continue;
                };
                if let Some(usage) = data
                    .get("usage")
                    .filter(|u| u.is_object())
                    .and_then(|u| serde_json::from_value::<Usage>(u.clone()).ok())
                {
                    out.push((thread, source, usage));
                }
            }
            anyhow::Ok(out)
        })
        .await??;
        let core = self.inner.core();
        let mut groups: std::collections::BTreeMap<String, (String, Spent)> = Default::default();
        let mut total = Spent::default();
        for (thread, source, usage) in turns {
            let view = core
                .registry
                .get(&thread)
                .or_else(|| core.registry.deleted_view(&thread));
            let (key, label) = match by {
                SpentBy::Thread => {
                    let label = match (core.registry.get(&thread), view) {
                        (Some(v), _) => v.name.clone(),
                        (None, Some(v)) => format!("{} (deleted)", v.name),
                        (None, None) => thread.clone(),
                    };
                    (thread.clone(), label)
                }
                SpentBy::Project => {
                    let p = view
                        .and_then(|v| v.project.clone())
                        .unwrap_or_else(|| "-".into());
                    (p.clone(), p)
                }
                SpentBy::Runtime => {
                    let r = source
                        .strip_prefix("adapter:")
                        .unwrap_or(&source)
                        .to_string();
                    (r.clone(), r)
                }
            };
            let entry = groups
                .entry(key)
                .or_insert_with(|| (label, Spent::default()));
            entry.1.add(&usage);
            total.add(&usage);
        }
        drop(core);
        let mut list: Vec<_> = groups.into_iter().collect();
        list.sort_by(|a, b| {
            let (x, y) = (&a.1 .1, &b.1 .1);
            y.cost
                .partial_cmp(&x.cost)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(y.tokens().cmp(&x.tokens()))
                .then(a.0.cmp(&b.0))
        });
        let row = |key: &str, label: &str, s: &Spent| {
            json!({
                "key": key,
                "label": label,
                "input": s.input,
                "output": s.output,
                "cost": s.cost(),
                "turns": s.turns,
                "costed_turns": s.costed_turns,
            })
        };
        Ok(json!({
            "since": since,
            "by": by.as_str(),
            "rows": list.iter().map(|(k, (l, s))| row(k, l, s)).collect::<Vec<_>>(),
            "total": row("total", "total", &total),
        }))
    }
}

/// How `usage.threads` groups what the turns spent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpentBy {
    /// Per thread (deleted ones included).
    #[default]
    Thread,
    /// Per project.
    Project,
    /// Per runtime: the adapter that reported the turn.
    Runtime,
}

impl SpentBy {
    /// Its name, as `usage.threads` takes it.
    pub fn as_str(self) -> &'static str {
        match self {
            SpentBy::Thread => "thread",
            SpentBy::Project => "project",
            SpentBy::Runtime => "runtime",
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use twr_core::config::Config;
    use twr_core::event::{
        ContextUpdated, Event, EventBody, SessionPayload, ThreadCreated, ThreadModelChanged,
    };
    use twr_core::usage::UsageWindow;

    use super::super::HubOptions;
    use super::*;

    #[test]
    fn usage_json_adds_clamped_remaining_and_keeps_used_pct() {
        let now = Utc::now();
        let mut view = UsageView::default();
        let mut ev = Event::new(
            twr_core::HUB_THREAD,
            1,
            None,
            Source::Hub,
            EventBody::AccountUsage(AccountUsage {
                runtime: "claude-code".into(),
                windows: [-3.0, 22.5, 100.0, 104.4]
                    .into_iter()
                    .map(|used_pct| UsageWindow {
                        window: "five_hour".into(),
                        used_pct,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }),
        );
        ev.ts = now;
        view.apply(&ev);
        let u = usage_json(&view, None, now);
        let windows = u["runtimes"][0]["windows"].as_array().unwrap();
        for (w, (used, left)) in
            windows
                .iter()
                .zip([(-3.0, 100.0), (22.5, 77.5), (100.0, 0.0), (104.4, 0.0)])
        {
            assert_eq!(w["used_pct"], used);
            assert_eq!(w["remaining_pct"], left);
        }
    }

    /// `usage.get` leaves out an entry read in another directory than its account's now (a
    /// log without the record that cleared it), and keeps one read in the account's directory,
    /// the native account's, and an older hub's that names no directory.
    #[test]
    fn usage_json_leaves_out_another_directorys_usage() {
        let now = Utc::now();
        let entry = |account: Option<&str>, dir: Option<&str>| {
            let mut ev = Event::new(
                twr_core::HUB_THREAD,
                1,
                None,
                Source::Hub,
                EventBody::AccountUsage(AccountUsage {
                    runtime: "claude-code".into(),
                    account: account.map(str::to_string),
                    windows: vec![UsageWindow {
                        window: "seven_day".into(),
                        used_pct: 40.0,
                        ..Default::default()
                    }],
                    dir: dir.map(str::to_string),
                    ..Default::default()
                }),
            );
            ev.ts = now;
            ev
        };
        let mut view = UsageView::default();
        view.apply(&entry(None, None));
        view.apply(&entry(Some("work"), Some("/acc/a")));
        view.apply(&entry(Some("old"), None));
        let accounts = |work: &str| {
            let mut c = Config::default();
            for (name, dir) in [("work", work), ("old", "/acc/old")] {
                c.accounts.entry("claude-code".into()).or_default().insert(
                    name.into(),
                    twr_core::config::AccountConfig {
                        dir: Some(dir.into()),
                        ..Default::default()
                    },
                );
            }
            AccountsView::new(&c)
        };
        let shown = |accounts: &AccountsView| -> Vec<Value> {
            usage_json(&view, Some(accounts), now)["runtimes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["account"].clone())
                .collect()
        };
        assert_eq!(
            shown(&accounts("/acc/a")),
            [Value::Null, json!("old"), json!("work")]
        );
        assert_eq!(shown(&accounts("/acc/b")), [Value::Null, json!("old")]);
    }

    /// A report merges only into windows read in its session's directory: known windows of
    /// another directory (a log without the record that cleared them) are another login's, so a
    /// sparse report starts afresh, and the record says the session's directory.
    #[tokio::test]
    async fn a_report_keeps_no_windows_of_another_directory() {
        use twr_core::event::AccountAdded;
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        {
            let mut core = hub.inner.core();
            core.accounts.apply(&Event::new(
                twr_core::HUB_THREAD,
                1,
                None,
                Source::Hub,
                EventBody::AccountAdded(AccountAdded {
                    runtime: "claude-code".into(),
                    account: "work".into(),
                    dir: Some(b.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            ));
            core.usage.apply(&Event::new(
                twr_core::HUB_THREAD,
                1,
                None,
                Source::Hub,
                EventBody::AccountUsage(AccountUsage {
                    runtime: "claude-code".into(),
                    account: Some("work".into()),
                    windows: vec![window("five_hour", 30.0), window("seven_day", 44.0)],
                    dir: Some(a.to_string_lossy().into_owned()),
                    ..Default::default()
                }),
            ));
            let view = core.registry.get("t1").unwrap().clone();
            core.rt("t1").runtime_settings = Some(super::super::restart::RunSettings {
                account: Some("work".into()),
                account_dir: Some(b.clone()),
                ..super::super::restart::RunSettings::of(&view)
            });
        }
        let known = |hub: &Hub| {
            let core = hub.inner.core();
            let e = core.usage.get("claude-code", Some("work")).unwrap();
            let pcts: Vec<(String, f64)> = e
                .windows
                .iter()
                .map(|w| (w.window.clone(), w.used_pct))
                .collect();
            (pcts, e.dir.clone())
        };
        let b_dir = b.to_str().map(str::to_string);
        record(&hub, of(vec![window("seven_day", 50.0)]), None);
        assert_eq!(
            known(&hub),
            (vec![("seven_day".into(), 50.0)], b_dir.clone())
        );
        // The next report of `b` merges into `b`'s.
        record(&hub, of(vec![window("five_hour", 5.0)]), None);
        assert_eq!(
            known(&hub),
            (
                vec![("five_hour".into(), 5.0), ("seven_day".into(), 50.0)],
                b_dir
            )
        );
    }

    async fn hub_with_thread(dir: &std::path::Path) -> Hub {
        let hub = Hub::open(HubOptions::new(dir.join("twr.db"), Config::default()))
            .await
            .unwrap();
        hub.append(
            "t1",
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "claude-code".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            "t1",
            None,
            Source::Adapter("claude-code".into()),
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s1".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub
    }

    fn report(pct: f64) -> RuntimeLimits {
        RuntimeLimits {
            windows: vec![UsageWindow {
                window: "seven_day".into(),
                used_pct: pct,
                resets_at: Some(Utc::now() + TimeDelta::days(3)),
                minutes: None,
                observed_at: None,
            }],
            source: "event".into(),
            ..Default::default()
        }
    }

    fn window(id: &str, pct: f64) -> UsageWindow {
        UsageWindow {
            window: id.into(),
            used_pct: pct,
            resets_at: Some(Utc::now() + TimeDelta::days(3)),
            ..Default::default()
        }
    }

    fn of(windows: Vec<UsageWindow>) -> RuntimeLimits {
        RuntimeLimits {
            windows,
            source: "event".into(),
            ..Default::default()
        }
    }

    /// The 10-minute refresh is per window: a report of one window that refreshes the record
    /// does not keep the other window from being refreshed by the next full report.
    #[tokio::test]
    async fn every_window_is_refreshed_every_ten_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let t0 = Utc::now() - TimeDelta::minutes(40);
        let both = || of(vec![window("five_hour", 10.0), window("seven_day", 40.0)]);
        record(&hub, both(), Some(t0));
        for round in 1..=3 {
            let t = t0 + TimeDelta::minutes(10 * round);
            record(&hub, of(vec![window("seven_day", 40.0)]), Some(t));
            record(&hub, both(), Some(t + TimeDelta::seconds(1)));
            let core = hub.inner.core();
            let e = core.usage.get("claude-code", None).unwrap();
            let five = e.windows.iter().find(|w| w.window == "five_hour").unwrap();
            assert_eq!(
                five.observed_at,
                Some(t + TimeDelta::seconds(1)),
                "round {round}"
            );
        }
    }

    /// The plan is the newest report's: a runner's replayed report does not bring back an older
    /// plan.
    #[tokio::test]
    async fn an_older_report_keeps_the_newer_plan() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let t0 = Utc::now() - TimeDelta::minutes(30);
        let with_plan = |plan: &str| RuntimeLimits {
            plan: Some(plan.into()),
            ..of(vec![window("seven_day", 40.0)])
        };
        record(&hub, with_plan("plus"), Some(t0 + TimeDelta::minutes(10)));
        record(&hub, with_plan("prolite"), Some(t0 + TimeDelta::minutes(5)));
        let core = hub.inner.core();
        let e = core.usage.get("claude-code", None).unwrap();
        assert_eq!(e.plan.as_deref(), Some("plus"));
        drop(core);
        record(&hub, with_plan("pro"), Some(t0 + TimeDelta::minutes(15)));
        let core = hub.inner.core();
        let e = core.usage.get("claude-code", None).unwrap();
        assert_eq!(e.plan.as_deref(), Some("pro"));
    }

    /// The plan's time does not go back when the window of the report that set it resets: an
    /// older replayed report still cannot bring back an older plan.
    #[tokio::test]
    async fn a_reset_window_does_not_let_an_older_plan_back() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let t0 = Utc::now() - TimeDelta::minutes(12);
        let report = |id: &str, resets: DateTime<Utc>, plan: Option<&str>| RuntimeLimits {
            plan: plan.map(str::to_string),
            ..of(vec![UsageWindow {
                resets_at: Some(resets),
                ..window(id, 10.0)
            }])
        };
        let later = Utc::now() + TimeDelta::days(3);
        record(&hub, report("seven_day", later, None), Some(t0));
        // Its window resets a moment from now.
        let soon = Utc::now() + TimeDelta::milliseconds(300);
        let pro = report("five_hour", soon, Some("pro"));
        record(&hub, pro, Some(t0 + TimeDelta::minutes(10)));
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let weekly = |at: i64, plan: Option<&str>| {
            record(
                &hub,
                report("seven_day", later, plan),
                Some(t0 + TimeDelta::minutes(at)),
            )
        };
        weekly(5, None);
        weekly(7, Some("plus"));
        let core = hub.inner.core();
        let e = core.usage.get("claude-code", None).unwrap();
        assert_eq!(e.plan.as_deref(), Some("pro"));
        assert_eq!(e.observed_at, t0 + TimeDelta::minutes(10));
    }

    /// Each window keeps its newest report: a runner's replayed report of one window, older
    /// than the last record but newer than that window's report, still updates it.
    #[tokio::test]
    async fn a_replayed_report_updates_the_windows_it_has_newer() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let t0 = Utc::now() - TimeDelta::minutes(30);
        record(
            &hub,
            of(vec![window("five_hour", 10.0), window("seven_day", 40.0)]),
            Some(t0),
        );
        // A local weekly report at t0 + 10 min, then a five-hour one a runner captured at
        // t0 + 5 min.
        record(
            &hub,
            of(vec![window("seven_day", 45.0)]),
            Some(t0 + TimeDelta::minutes(10)),
        );
        record(
            &hub,
            of(vec![window("five_hour", 20.0), window("seven_day", 41.0)]),
            Some(t0 + TimeDelta::minutes(5)),
        );
        let core = hub.inner.core();
        let e = core.usage.get("claude-code", None).unwrap();
        let pcts: Vec<f64> = e.windows.iter().map(|w| w.used_pct).collect();
        assert_eq!(pcts, [20.0, 45.0]);
        assert_eq!(e.observed_at, t0 + TimeDelta::minutes(10));
    }

    /// `usage.threads` sums the turns' usage from the log, per thread, project or runtime,
    /// and only the turns in its window.
    #[tokio::test]
    async fn usage_threads_sums_turns_from_the_log() {
        use twr_core::event::{TurnEnded, TurnResult, TurnStarted, Usage};
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let turn = |n: u64, runtime: &str, usage: Option<Usage>| {
            hub.append(
                "t1",
                Some(n),
                Source::Adapter(runtime.into()),
                EventBody::TurnStarted(TurnStarted {
                    prompt: "go".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
            hub.append(
                "t1",
                Some(n),
                Source::Adapter(runtime.into()),
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    usage,
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        };
        turn(
            1,
            "claude-code",
            Some(Usage {
                input: 1000,
                output: 10,
                cost: Some(0.25),
            }),
        );
        turn(2, "claude-code", None);
        turn(
            3,
            "codex",
            Some(Usage {
                input: 500,
                output: 5,
                cost: None,
            }),
        );
        let all = hub.usage_threads(None, SpentBy::Thread).await.unwrap();
        assert_eq!(all["by"], "thread");
        assert_eq!(all["rows"].as_array().unwrap().len(), 1);
        let row = &all["rows"][0];
        assert_eq!(
            (row["key"].as_str(), row["label"].as_str()),
            (Some("t1"), Some("api"))
        );
        assert_eq!(
            (row["input"].as_u64(), row["turns"].as_u64()),
            (Some(1500), Some(2))
        );
        assert_eq!(row["cost"], 0.25);
        assert_eq!(all["total"]["output"], 15);
        // The thread's own view sums the same.
        let view = hub.thread_view("t1").unwrap();
        assert_eq!((view.spent.input, view.spent.turns), (1500, 2));

        let by_runtime = hub.usage_threads(None, SpentBy::Runtime).await.unwrap();
        let keys: Vec<_> = by_runtime["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| (r["key"].as_str().unwrap(), r["cost"].clone()))
            .collect();
        // By cost first: the codex row reported none.
        assert_eq!(keys, [("claude-code", json!(0.25)), ("codex", Value::Null)]);

        let later = hub
            .usage_threads(
                Some(Utc::now() + chrono::TimeDelta::hours(1)),
                SpentBy::Project,
            )
            .await
            .unwrap();
        assert!(later["rows"].as_array().unwrap().is_empty());
        assert_eq!(later["total"]["turns"], 0);
    }

    fn record(hub: &Hub, limits: RuntimeLimits, at: Option<DateTime<Utc>>) {
        let mut core = hub.inner.core();
        core.record_usage(&hub.inner, "t1", limits, None, at)
            .unwrap();
    }

    /// A session whose runtime the hub does not know yet (a remote one taken over after a
    /// restart, before its `thread.started` is recorded) has its report dropped, not credited
    /// to the thread's runtime now.
    #[tokio::test]
    async fn a_report_before_the_sessions_runtime_is_known_is_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.append(
            "t1",
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "claude-code".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        record(&hub, report(30.0), None);
        let core = hub.inner.core();
        assert!(core.usage.entries().next().is_none());
    }

    /// A thread moved to another runtime while its previous one still runs: what that one
    /// reports is its own runtime's usage, not the new runtime's.
    #[tokio::test]
    async fn a_moved_threads_previous_runtime_reports_its_own_usage() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        hub.append(
            "t1",
            None,
            Source::User("test".into()),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                runtime: Some("codex".into()),
                previous_runtime: Some("claude-code".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        // A remote runtime's frames name the thread's runtime now as their source.
        record(&hub, report(30.0), None);
        let core = hub.inner.core();
        assert!(core.usage.get("codex", None).is_none());
        assert_eq!(
            core.usage.get("claude-code", None).unwrap().windows[0].used_pct,
            30.0
        );
    }

    /// Reports a runner replays keep their capture time, whatever the hub thread's order does
    /// to the records' `ts`: a later capture after an earlier one is recorded, and the age is
    /// the report's.
    #[tokio::test]
    async fn replayed_reports_keep_their_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let hub = hub_with_thread(dir.path()).await;
        let c1 = Utc::now() - TimeDelta::minutes(20);
        let c2 = c1 + TimeDelta::minutes(10);
        // A hub event after both captures: records on the hub thread come after it.
        hub.append(
            twr_core::HUB_THREAD,
            None,
            Source::Hub,
            EventBody::ContextUpdated(ContextUpdated {
                hash: "sha256:00".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        record(&hub, report(30.0), Some(c1));
        record(&hub, report(35.0), Some(c2));
        // An older capture than the last record: nothing.
        record(&hub, report(99.0), Some(c1));
        let core = hub.inner.core();
        let e = core.usage.get("claude-code", None).unwrap();
        assert_eq!(e.windows[0].used_pct, 35.0);
        assert_eq!(e.observed_at, c2);
        drop(core);
        let u = hub.usage();
        assert_eq!(u["runtimes"][0]["stale"], true);
        assert!(u["runtimes"][0]["age_secs"].as_i64().unwrap() >= 9 * 60);
    }
}

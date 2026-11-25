//! The hub's internet connection: whether its own machine can reach the model APIs its
//! runtimes need, so that a thread silent because the connection is down is not taken for a
//! stalled or hung one.
//!
//! **The check** ([`NetProbe`], [`HubOptions::net_probe`](crate::HubOptions::net_probe): `twr
//! hub` passes [`own_net_probe`]; tests a fake or none, and then nothing here runs). A TCP
//! connection to each of its targets at once, each given [`CONNECT_TIMEOUT`]; reachable when
//! any one connects. The targets are `[hub] connectivity_targets`, else the proxy of
//! `HTTPS_PROXY` / `ALL_PROXY` (behind a proxy only it may be reachable), else
//! [`CONNECTIVITY_TARGETS`], the built-in runtimes' model APIs. Nothing is sent: a connection
//! opened is closed at once. It runs every `[hub] connectivity_check_secs` (30 s by default;
//! `0`: never) while the connection is up, every [`SUSPECT_EVERY`] once a check failed, and
//! every [`OFFLINE_EVERY`] while it is down; a turn that failed with a transient error (a
//! network error among them) runs one at once ([`Inner::net_nudge`], at most every
//! [`NUDGE_GAP`]).
//!
//! **No false alarm** ([`NetWatch::observe`]). The connection is down only after
//! [`FAILS_TO_OFFLINE`] failed checks in a row spanning [`OFFLINE_AFTER`] at least, since the
//! first of them (`since`); a gap between two checks much longer than they are apart (the
//! machine slept) starts the count again. It is up again at the first check that connects.
//!
//! **What the hub does while it is down** (all on the hub's own box, `local`: a remote box's
//! runner reaches the models from its own network, and when it loses the hub it shows as not
//! connected, `Sight::Stale`):
//! - It raises one item on [`HUB_THREAD`]: `stalled`, `ext.twr.offline = {since, targets,
//!   detail}` ([`OFFLINE_KEY`]), "No internet connection since 14:02 ...", which notifies as
//!   `offline`; a client shows it as the hub's notice ([`AttentionItem::offline_since`]).
//! - A thread silent for the stall threshold gets its stall with the same mark and "No internet
//!   connection since 14:02 (no activity for 10 min ...)" instead of a plain one, and no
//!   hung-turn card: interrupting it would not help (`hub::stall`). The orchestrator is not
//!   told of either: it could not take a turn to read them.
//! - The orchestrator's notices wait ([`Core::maybe_notify`]).
//! - A worker's failed turn waits for its automatic retry until a check after the failure
//!   connected ([`Core::net_holds_retry`]): retried at once, it would fail the same way, and
//!   its one retry would be gone.
//!
//! **When it is back** ([`Core::observe_net`]): the items marked [`OFFLINE_KEY`] close (`by:
//! timeout`, `ext.twr.back_online` the time, [`BACK_ONLINE_KEY`]) in one batch (one left open
//! by an earlier hub too), held retries start, the orchestrator is told how long the connection
//! was down, and a thread's silence counts from then on: no stall before the stall threshold,
//! no hung-turn card before the hung threshold, has passed since.
//!
//! What it knows is memory only (a restarted hub checks again); the items are in the log.

use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use futures::future::{BoxFuture, FutureExt};
use serde_json::{json, Value};
use twr_core::config::{HubConfig, CONNECTIVITY_TARGETS};
use twr_core::event::*;
use twr_core::reduce::{AttentionItem, ThreadView};

use super::{twr_ext_map, Batch, Core, HubState, Inner, LOCAL_BOX};

/// How long one target is given to accept a connection.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Failed checks in a row before the connection is down.
pub const FAILS_TO_OFFLINE: u32 = 3;

/// The least time those failed checks span, from the first.
pub const OFFLINE_AFTER: Duration = Duration::from_secs(20);

/// How often it checks once a check failed (the connection not yet down).
pub const SUSPECT_EVERY: Duration = Duration::from_secs(5);

/// How often it checks while the connection is down.
pub const OFFLINE_EVERY: Duration = Duration::from_secs(10);

/// The least time between two checks a nudge runs.
pub const NUDGE_GAP: Duration = Duration::from_secs(2);

/// What one check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    /// A target connected.
    pub ok: bool,
    /// Which (`api.anthropic.com:443`), or why none did, per target.
    pub detail: String,
}

/// How often the hub checks its connection and when it calls it down ([`NetProbe::timing`]):
/// the constants above by default; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetTiming {
    /// The period while it is up; `None`: `[hub] connectivity_check_secs`.
    pub every: Option<Duration>,
    /// [`SUSPECT_EVERY`].
    pub suspect_every: Duration,
    /// [`OFFLINE_EVERY`].
    pub offline_every: Duration,
    /// [`OFFLINE_AFTER`].
    pub offline_after: Duration,
    /// [`NUDGE_GAP`].
    pub nudge_gap: Duration,
}

impl Default for NetTiming {
    fn default() -> Self {
        NetTiming {
            every: None,
            suspect_every: SUSPECT_EVERY,
            offline_every: OFFLINE_EVERY,
            offline_after: OFFLINE_AFTER,
            nudge_gap: NUDGE_GAP,
        }
    }
}

/// How the hub checks its connection ([`crate::HubOptions::net_probe`]).
#[derive(Clone)]
pub struct NetProbe {
    /// What it connects to (`host:port`), as the hub's notice names them.
    pub targets: Vec<String>,
    /// One check.
    pub check: Arc<dyn Fn() -> BoxFuture<'static, Probed> + Send + Sync>,
    /// When it checks.
    pub timing: NetTiming,
}

impl std::fmt::Debug for NetProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetProbe")
            .field("targets", &self.targets)
            .field("timing", &self.timing)
            .finish_non_exhaustive()
    }
}

/// What the check connects to for `config`, with `env` the environment: `[hub]
/// connectivity_targets`, else the proxy (`HTTPS_PROXY`, `https_proxy`, `ALL_PROXY`,
/// `all_proxy`, the first set that names a host), else [`CONNECTIVITY_TARGETS`].
pub fn probe_targets(config: &HubConfig, env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    if let Some(t) = &config.connectivity_targets {
        return t.clone();
    }
    for var in ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"] {
        if let Some(t) = env(var).as_deref().and_then(proxy_target) {
            return vec![t];
        }
    }
    CONNECTIVITY_TARGETS.iter().map(|t| t.to_string()).collect()
}

/// The `host:port` of proxy URL `url` (`http://user:pw@proxy.lan:3128/` → `proxy.lan:3128`),
/// its scheme's port when it names none (`http` 80, `https` 443, `socks*` 1080).
fn proxy_target(url: &str) -> Option<String> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.is_empty() {
        return None;
    }
    // `[::1]:3128`, `proxy:3128`, or a host alone.
    let has_port = match host_port.rfind(':') {
        Some(i) => !host_port[i..].contains(']') && host_port[i + 1..].parse::<u16>().is_ok(),
        None => false,
    };
    if has_port {
        return Some(host_port.to_string());
    }
    let port = match scheme.to_ascii_lowercase().as_str() {
        "https" => 443,
        s if s.starts_with("socks") => 1080,
        _ => 80,
    };
    Some(format!("{host_port}:{port}"))
}

/// The hub's own check of `targets` (see the module docs). What `twr hub` uses.
pub fn own_net_probe(targets: Vec<String>) -> NetProbe {
    let list = targets.clone();
    NetProbe {
        targets,
        check: Arc::new(move || connect_any(list.clone()).boxed()),
        timing: NetTiming::default(),
    }
}

/// Connects to each of `targets` at once; ok at the first that connects.
async fn connect_any(targets: Vec<String>) -> Probed {
    let mut tries: futures::stream::FuturesUnordered<_> = targets
        .into_iter()
        .map(|t| async move {
            let r = tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(&t)).await;
            let r = match r {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err(format!("no answer in {} s", CONNECT_TIMEOUT.as_secs())),
            };
            (t, r)
        })
        .collect();
    let mut failed = Vec::new();
    while let Some((t, r)) = futures::StreamExt::next(&mut tries).await {
        match r {
            Ok(()) => {
                return Probed {
                    ok: true,
                    detail: t,
                }
            }
            Err(e) => failed.push(format!("{t}: {e}")),
        }
    }
    Probed {
        ok: false,
        detail: if failed.is_empty() {
            "nothing to check".into()
        } else {
            failed.join("; ")
        },
    }
}

/// A change [`NetWatch::observe`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Change {
    /// The connection is down, since the first failed check of the streak.
    Lost { since: DateTime<Utc> },
    /// It is back (at the check that connected), down since `since`.
    Back {
        since: DateTime<Utc>,
        at: DateTime<Utc>,
    },
}

/// What the hub knows of its connection (see the module docs). Memory only.
#[derive(Debug, Default)]
pub(super) struct NetWatch {
    /// The hub checks (it has a probe and a period).
    enabled: bool,
    /// The period while it is up.
    every: Duration,
    /// The other periods and thresholds.
    timing: NetTiming,
    /// What it connects to.
    targets: Vec<String>,
    /// Down since then.
    offline_since: Option<DateTime<Utc>>,
    /// When it last came back.
    back_at: Option<DateTime<Utc>>,
    /// Failed checks in a row, and when the first of them ran.
    fails: u32,
    first_fail: Option<DateTime<Utc>>,
    /// The last check, and the last that connected.
    last_check: Option<DateTime<Utc>>,
    last_ok: Option<DateTime<Utc>>,
    /// What the last check said.
    detail: Option<String>,
    /// Threads whose automatic retry waits for a check that connects ([`Core::net_holds_retry`]).
    pub(super) held_retries: BTreeSet<String>,
}

impl NetWatch {
    /// A watch that checks `targets` every `every` (zero: one that never checks), as `timing`
    /// says otherwise.
    pub(super) fn new(every: Duration, timing: NetTiming, targets: Vec<String>) -> Self {
        NetWatch {
            enabled: !every.is_zero(),
            every,
            timing,
            targets,
            ..Default::default()
        }
    }

    /// Takes check `p`, run at `at`: what changed, if anything.
    pub(super) fn observe(&mut self, p: &Probed, at: DateTime<Utc>) -> Option<Change> {
        let gap = self.last_check.map(|l| at - l);
        self.last_check = Some(at);
        self.detail = Some(p.detail.clone());
        if p.ok {
            self.fails = 0;
            self.first_fail = None;
            self.last_ok = Some(at);
            let since = self.offline_since.take()?;
            self.back_at = Some(at);
            return Some(Change::Back { since, at });
        }
        if self.offline_since.is_some() {
            return None;
        }
        // Checks much further apart than they run: the machine slept (or the hub was stuck),
        // and what failed before says nothing of now.
        let slept = chrono::Duration::from_std(self.every.max(self.timing.offline_every) * 3)
            .is_ok_and(|g| gap.is_some_and(|gap| gap > g + chrono::Duration::seconds(60)));
        if slept {
            self.fails = 0;
            self.first_fail = None;
        }
        self.fails += 1;
        let first = *self.first_fail.get_or_insert(at);
        let span = chrono::Duration::from_std(self.timing.offline_after).unwrap_or_default();
        if self.fails >= FAILS_TO_OFFLINE && at - first >= span {
            self.offline_since = Some(first);
            return Some(Change::Lost { since: first });
        }
        None
    }

    /// How long until the next check.
    pub(super) fn next_in(&self) -> Duration {
        if self.offline_since.is_some() {
            self.timing.offline_every.min(self.every)
        } else if self.fails > 0 {
            self.timing.suspect_every.min(self.every)
        } else {
            self.every
        }
    }

    /// Since when the connection is down, while it is.
    pub(super) fn offline_since(&self) -> Option<DateTime<Utc>> {
        self.offline_since
    }

    /// When it last came back.
    pub(super) fn back_at(&self) -> Option<DateTime<Utc>> {
        self.back_at
    }

    /// Whether a check at `t` or later connected, and the connection is not down (always, for
    /// a watch that never checks).
    pub(super) fn up_since(&self, t: DateTime<Utc>) -> bool {
        !self.enabled || (self.offline_since.is_none() && self.last_ok.is_some_and(|ok| ok >= t))
    }

    /// `hub.info`'s `connectivity`.
    pub(super) fn to_json(&self) -> Value {
        if !self.enabled {
            return json!({"state": "off"});
        }
        let state = if self.offline_since.is_some() {
            "offline"
        } else if self.last_ok.is_some() {
            "online"
        } else {
            "unknown"
        };
        json!({
            "state": state,
            "since": self.offline_since,
            "back_at": self.back_at,
            "last_check": self.last_check,
            "last_ok": self.last_ok,
            "detail": self.detail,
            "targets": self.targets,
            "every_secs": self.every.as_secs(),
            "held_retries": self.held_retries.len(),
        })
    }
}

/// `t` as the hub's machine shows the time: `14:02`, with the day when not today (`Mon 14:02`).
pub(super) fn clock(t: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let (t, now) = (t.with_timezone(&Local), now.with_timezone(&Local));
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else {
        t.format("%a %H:%M").to_string()
    }
}

/// `targets` in words: `a`, `a or b`, `a, b or c`.
fn either(targets: &[String]) -> String {
    match targets {
        [] => "the internet".into(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// How long `secs` is, in words (`18 min`, `2 h 5 min`, `40 s`).
fn span_words(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        format!("{secs} s")
    } else if secs < 3600 {
        format!("{} min", secs / 60)
    } else {
        format!("{} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

/// Whether `item` is one of the hub's items about the connection ([`OFFLINE_KEY`]).
fn is_offline_item(item: &AttentionItem) -> bool {
    item.offline_since.is_some()
}

impl Core {
    /// Since when the connection of `v`'s box is down, while it is: only the hub's own box's
    /// (see the module docs).
    pub(super) fn offline_for(&self, v: &ThreadView) -> Option<DateTime<Utc>> {
        self.net.offline_since().filter(|_| v.box_name == LOCAL_BOX)
    }

    /// When the connection of `v`'s box last came back (the hub's own box only).
    pub(super) fn back_for(&self, v: &ThreadView) -> Option<DateTime<Utc>> {
        self.net.back_at().filter(|_| v.box_name == LOCAL_BOX)
    }

    /// Whether the connection is down (the hub's own).
    pub(super) fn net_offline(&self) -> bool {
        self.net.offline_since().is_some()
    }

    /// `hub.info`'s `connectivity`.
    pub(super) fn net_json(&self) -> Value {
        self.net.to_json()
    }

    /// Whether `thread`'s automatic retry of its turn `failed` waits for the connection: it
    /// runs on the hub's own box, and no check since the turn ended connected (or the
    /// connection is down). A check runs at once ([`Inner::net_nudge`]); the retry starts when
    /// one connects ([`Core::observe_net`]).
    pub(super) fn net_holds_retry(
        &mut self,
        hub: &Inner,
        thread: &str,
        failed: u64,
    ) -> anyhow::Result<bool> {
        if !self.net.enabled
            || !self
                .registry
                .get(thread)
                .is_some_and(|v| v.box_name == LOCAL_BOX)
        {
            return Ok(false);
        }
        let ended = self
            .stored_event(thread, Some(failed), "turn.ended")?
            .map_or_else(Utc::now, |e| e.ts);
        if self.net.up_since(ended) {
            self.net.held_retries.remove(thread);
            return Ok(false);
        }
        // (Asked again on every dispatch: one check for it, the first time.)
        if self.net.held_retries.insert(thread.to_string()) {
            tracing::info!(
                thread,
                failed,
                "the automatic retry waits for a check of the internet connection"
            );
            hub.net_nudge.notify_one();
        }
        Ok(true)
    }

    /// Whether `thread`'s automatic retry waits for the connection ([`Core::net_holds_retry`]):
    /// nothing else of its queue runs before it.
    pub(super) fn retry_held_for_net(&self, thread: &str) -> bool {
        self.net.held_retries.contains(thread)
    }

    /// Takes check `p`, run at `at` (see the module docs), and says how long until the next.
    pub(super) fn observe_net(
        &mut self,
        hub: &Arc<Inner>,
        p: &Probed,
        at: DateTime<Utc>,
    ) -> Duration {
        let change = self.net.observe(p, at);
        let r = match change {
            Some(Change::Lost { since }) => {
                tracing::warn!(since = %since, detail = %p.detail, "no internet connection");
                self.net_lost(hub, since, &p.detail, at)
            }
            Some(Change::Back { since, at }) => {
                tracing::info!(since = %since, "the internet connection is back");
                self.net_back(hub, Some(since), at)
            }
            // An item an earlier hub left open, the connection being up: it closes.
            None if p.ok && self.offline_items().next().is_some() => self.net_back(hub, None, at),
            None => Ok(()),
        };
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "cannot record the internet connection's change");
        }
        if p.ok && !self.net.held_retries.is_empty() {
            for thread in std::mem::take(&mut self.net.held_retries) {
                if let Err(e) = self.try_dispatch(hub, &thread) {
                    tracing::warn!(thread = %thread, error = %format!("{e:#}"), "cannot start a held retry");
                }
            }
        }
        self.net.next_in()
    }

    /// The open items marked [`OFFLINE_KEY`]: (thread, attn_id).
    fn offline_items(&self) -> impl Iterator<Item = (String, String)> + '_ {
        self.attention
            .ordered(&Default::default())
            .into_iter()
            .filter(|i| is_offline_item(i))
            .map(|i| (i.thread.clone(), i.attn_id.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// The connection is down since `since`, the last check saying `detail`: the hub's item
    /// about it, unless one is open (an earlier hub's).
    fn net_lost(
        &mut self,
        hub: &Inner,
        since: DateTime<Utc>,
        detail: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        if self.offline_items().any(|(t, _)| t == HUB_THREAD) {
            return Ok(());
        }
        let summary = format!(
            "No internet connection since {}: twr cannot reach {} from this machine, so its \
             threads cannot reach their model APIs. Until it is back, their silence is not \
             called a stall or a hung turn, and automatic retries and the orchestrator's \
             notices wait. This closes by itself when the connection returns.",
            clock(since, now),
            either(&self.net.targets)
        );
        self.append(
            hub,
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Stalled,
                summary,
                options: vec!["dismiss".into()],
                ext: twr_ext_map(json!({ OFFLINE_KEY: {
                    "since": since.to_rfc3339(),
                    "targets": self.net.targets,
                    "detail": twr_core::activity::cut(detail, 400),
                } })),
                ..Default::default()
            }),
        )?;
        Ok(())
    }

    /// The connection is back at `at` (down since `since`, when this hub saw it go): the items
    /// about it close in one batch, and the orchestrator is told.
    fn net_back(
        &mut self,
        hub: &Inner,
        since: Option<DateTime<Utc>>,
        at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        let items: Vec<(String, String)> = self.offline_items().collect();
        let since = since.or_else(|| {
            self.attention
                .ordered(&Default::default())
                .into_iter()
                .filter_map(|i| i.offline_since)
                .min()
        });
        if let (Some(since), Some(_)) = (since, self.orch.thread.as_ref()) {
            self.orch.push_info(format!(
                "- connectivity: the hub's machine had no internet connection from {} to {} \
                 ({}): its threads could not reach their model APIs, so their silence then was \
                 no stall, and failed turns waited for the connection to be retried. Check the \
                 threads that were working before you recommend interrupting one.",
                clock(since, at),
                clock(at, at),
                span_words((at - since).num_seconds())
            ));
        }
        if items.is_empty() {
            return Ok(());
        }
        let mut batch = Batch::default();
        for (thread, attn_id) in items {
            let turn = self
                .registry
                .get(&thread)
                .and_then(|v| v.current_turn.or(v.last_turn.as_ref().map(|t| t.turn)));
            batch.push(
                &thread,
                turn,
                Source::Hub,
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id,
                    by: ResolvedBy::Timeout,
                    answer: None,
                    ext: twr_ext_map(json!({ BACK_ONLINE_KEY: at.to_rfc3339() })),
                    ..Default::default()
                }),
            );
        }
        self.append_batch(hub, batch)?;
        Ok(())
    }
}

/// The check's task (see the module docs): checks at once, then as [`NetWatch::next_in`] says
/// or when nudged, until the hub stops or is dropped.
pub(super) async fn net_ticker(hub: Weak<Inner>, probe: NetProbe) {
    loop {
        let started = tokio::time::Instant::now();
        let p = match tokio::time::timeout(CONNECT_TIMEOUT * 2, (probe.check)()).await {
            Ok(p) => p,
            Err(_) => Probed {
                ok: false,
                detail: "the check did not end".into(),
            },
        };
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() == HubState::Stopped {
            return;
        }
        let wait = h.core().observe_net(&h, &p, Utc::now());
        let nudge = Arc::clone(&h.net_nudge);
        let mut state = h.state.subscribe();
        drop(h);
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = nudge.notified() => {
                tokio::time::sleep_until(started + probe.timing.nudge_gap).await;
            }
            _ = async { state.wait_for(|s| *s == HubState::Stopped).await.is_ok() } => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-11-23T14:02:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::seconds(secs)
    }

    fn ok() -> Probed {
        Probed {
            ok: true,
            detail: "api.anthropic.com:443".into(),
        }
    }

    fn fail() -> Probed {
        Probed {
            ok: false,
            detail: "api.anthropic.com:443: no answer in 5 s".into(),
        }
    }

    fn watch() -> NetWatch {
        NetWatch::new(
            Duration::from_secs(30),
            NetTiming::default(),
            vec!["api.anthropic.com:443".into()],
        )
    }

    #[test]
    fn down_only_after_three_failures_spanning_twenty_seconds() {
        let mut w = watch();
        assert_eq!(w.observe(&ok(), at(0)), None);
        assert_eq!(w.next_in(), Duration::from_secs(30));
        assert_eq!(w.observe(&fail(), at(30)), None);
        // A failed check: the next comes sooner.
        assert_eq!(w.next_in(), SUSPECT_EVERY);
        assert_eq!(w.observe(&fail(), at(35)), None);
        // Three in a row, but only 10 s apart: not yet.
        assert_eq!(w.observe(&fail(), at(40)), None);
        assert_eq!(w.offline_since(), None);
        assert_eq!(
            w.observe(&fail(), at(50)),
            Some(Change::Lost { since: at(30) })
        );
        assert_eq!(w.offline_since(), Some(at(30)));
        assert_eq!(w.next_in(), OFFLINE_EVERY);
        // Still down: no other change.
        assert_eq!(w.observe(&fail(), at(60)), None);
        assert_eq!(
            w.observe(&ok(), at(70)),
            Some(Change::Back {
                since: at(30),
                at: at(70)
            })
        );
        assert_eq!(w.offline_since(), None);
        assert_eq!(w.back_at(), Some(at(70)));
        assert_eq!(w.next_in(), Duration::from_secs(30));
    }

    #[test]
    fn one_check_that_connects_ends_a_streak() {
        let mut w = watch();
        for s in [0, 5, 10] {
            assert_eq!(w.observe(&fail(), at(s)), None);
        }
        assert_eq!(w.observe(&ok(), at(15)), None);
        // The count starts again.
        for s in [20, 25, 30] {
            assert_eq!(w.observe(&fail(), at(s)), None);
        }
        assert_eq!(w.offline_since(), None);
        assert_eq!(
            w.observe(&fail(), at(40)),
            Some(Change::Lost { since: at(20) })
        );
    }

    #[test]
    fn a_machine_that_slept_starts_counting_again() {
        let mut w = watch();
        assert_eq!(w.observe(&fail(), at(0)), None);
        assert_eq!(w.observe(&fail(), at(5)), None);
        // An hour later (it slept): the network may not be up yet, but what failed an hour
        // ago does not count.
        assert_eq!(w.observe(&fail(), at(3600)), None);
        assert_eq!(w.observe(&fail(), at(3605)), None);
        assert_eq!(w.offline_since(), None);
        assert_eq!(
            w.observe(&fail(), at(3620)),
            Some(Change::Lost { since: at(3600) })
        );
    }

    #[test]
    fn up_since_needs_a_check_after_the_time() {
        let mut w = watch();
        w.observe(&ok(), at(0));
        assert!(w.up_since(at(0)));
        assert!(!w.up_since(at(1)));
        w.observe(&ok(), at(2));
        assert!(w.up_since(at(1)));
        // A watch that never checks holds nothing.
        assert!(NetWatch::new(Duration::ZERO, NetTiming::default(), vec![]).up_since(at(100)));
        assert_eq!(
            NetWatch::new(Duration::ZERO, NetTiming::default(), vec![]).to_json()["state"],
            "off"
        );
    }

    #[test]
    fn targets_come_from_the_config_then_the_proxy_then_the_model_apis() {
        let none = |_: &str| None;
        assert_eq!(
            probe_targets(&HubConfig::default(), none),
            CONNECTIVITY_TARGETS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let proxy = |v: &str| (v == "https_proxy").then(|| "http://u:p@proxy.lan:3128/".into());
        assert_eq!(
            probe_targets(&HubConfig::default(), proxy),
            ["proxy.lan:3128"]
        );
        let config = HubConfig {
            connectivity_targets: Some(vec!["gw.lan:443".into()]),
            ..Default::default()
        };
        assert_eq!(probe_targets(&config, proxy), ["gw.lan:443"]);
        assert_eq!(
            proxy_target("socks5://10.0.0.1"),
            Some("10.0.0.1:1080".into())
        );
        assert_eq!(proxy_target("https://proxy"), Some("proxy:443".into()));
        assert_eq!(proxy_target("proxy.lan"), Some("proxy.lan:80".into()));
        assert_eq!(proxy_target("http://[::1]:8080"), Some("[::1]:8080".into()));
        assert_eq!(proxy_target("http://[::1]"), Some("[::1]:80".into()));
        assert_eq!(proxy_target("http://"), None);
    }

    #[test]
    fn words() {
        assert_eq!(either(&["a".into()]), "a");
        assert_eq!(either(&["a".into(), "b".into(), "c".into()]), "a, b or c");
        assert_eq!(span_words(40), "40 s");
        assert_eq!(span_words(18 * 60 + 5), "18 min");
        assert_eq!(span_words(2 * 3600 + 5 * 60), "2 h 5 min");
    }

    #[tokio::test]
    async fn the_own_check_connects_to_a_listener_and_fails_on_a_closed_port() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = l.local_addr().unwrap().to_string();
        let closed = {
            let c = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            c.local_addr().unwrap().to_string()
        };
        let p = own_net_probe(vec![closed.clone(), open.clone()]);
        let r = (p.check)().await;
        assert!(r.ok, "{r:?}");
        assert_eq!(r.detail, open);
        let r = (own_net_probe(vec![closed.clone()]).check)().await;
        assert!(!r.ok);
        assert!(r.detail.starts_with(&closed), "{}", r.detail);
    }
}

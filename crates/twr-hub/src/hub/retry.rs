//! Failed worker turns: the block_kind decision and the one automatic retry.
//!
//! When a worker turn ends `error`, or its runtime crashes while a turn is open, the hub runs
//! the block_kind ladder (code: a sign-in failure → `user_blocked`; a usage or spending
//! limit → `limited`; an adapter's `ext.<runtime>.transient` flag → `transient`; default
//! `user_blocked`) and
//! logs it as `decision.made{site: block_kind}` for that turn. A turn that carried the resume
//! of a session the thread moved back to and failed before the runtime started it
//! (`ThreadView::resume_failed_turn`, `hub::switch`) counts as flagged transient: its retry
//! starts a new session instead (unless the adapter said it is not signed in).
//!
//! - `transient`, and the thread is [`ThreadView::retry_eligible`] (the failed turn is not
//!   itself a retry and no retry of it started): the hub starts one retry turn with
//!   [`RETRY_PROMPT`] (the failed turn's own prompt, as given, when its resume failed: no
//!   session got it), `source: hub`, the failed turn's `from`, and
//!   `turn.started.ext.twr.retry_of = <failed turn>`. Starting it resolves the hub's open
//!   `error` item for the failed turn (a crash raises one, but not for a turn whose resume
//!   failed) `by: auto_retry`. A failed turn gets no `error` item while its retry is pending.
//! - Anything else (`user_blocked`, `limited`, or a retry that failed too): the hub raises an
//!   `error` attention for the turn (a crash already raised one), which stays for the user.
//!   `limited` (the turn carries `ext.twr.limit`, whatever a runtime section says of
//!   `transient`) is never retried: its card says which account hit which limit and when it
//!   resets (`api: claude-code:work hit its weekly limit · resets Thu 05:00 (in 13h)`), naming
//!   a named account always and the native one as the labels do, and carries `ext.twr.limit`
//!   and `ext.twr.account` (a named account). A sign-in failure on a named account says how to
//!   sign that account in (`twr account login <runtime> <account>`, in place of the adapter's
//!   hint); a sign-in failure has the account's login checked again
//!   (`Core::check_account_soon`), the native account's too where its runtime has named
//!   accounts (the runtimes the login checks cover), for a thread on the hub's own box (one
//!   on another box ran on that box's own login). The account is the session's that ran the
//!   turn, in the directory it ran in: one whose name leads elsewhere now (removed, or added
//!   again in another directory) is named as such (`claude-code:work (previous directory)`),
//!   with how to move the thread in place of the sign-in hint, and no login check. The
//!   orchestrator's failed turns get no ladder and no retry, but their `error` item says the
//!   same of limits and sign-in failures (`Core::failure_words`, from `hub::reactions`).
//! - `transient`, and the thread was stopped before its retry started (by the user while its
//!   runtime was being replaced, or by a hub that died meanwhile: a restart's reason is in
//!   its memory only, so the next hub records the stop as anyone's): no retry (a stopped
//!   thread is not eligible), and the `error` item is raised when the stop is recorded or at
//!   the next start, saying the turn was not retried.
//!
//! A failed worker turn gets this `error` item instead of the `review` its `gave_up` grade
//! would raise (one item per failure); its grade and report are logged as for any turn.
//!
//! Everything is derived from the log, so it survives a hub restart: at startup (and whenever
//! the thread frees up, see [`Core::try_dispatch`]) a failed turn whose block kind is
//! `transient`, that is still eligible, gets its retry. The retry's `turn.started` is what
//! makes the turn ineligible, so the retry starts exactly once. A missing block_kind decision
//! (the hub died between the failure and the decision) is made at startup.
//!
//! Crashes: queued prompts are not dispatched after a crash, to avoid crash
//! loops. The automatic retry is the one exception: it restarts (resumes) the runtime once for
//! the failed turn; if the retry fails too, nothing else is dispatched until the user acts.
//! A worker's `thread.crashed` holds its queue (`ThreadRt::queue_held`, folded from the log, so
//! a hub restart keeps it): every ordinary dispatch (the background ticker, a runner's
//! reconnection, a detach, ...) checks it, a due retry aside, and only the user's next prompt
//! (`thread.send`, `say`), resume (`thread.resume`) or a runtime that starts again releases it:
//! not a prompt anything else queues (a job's wake, the orchestrator's send or resume,
//! `Batch::user_send`).

use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{json, Map, Value};
use twr_adapters::usage::{LimitKind, LIMIT_KEY};
use twr_core::event::*;
use twr_core::reduce::ThreadView;
use twr_core::usage::{age, reset_time, FIVE_HOUR, SEVEN_DAY};
use twr_decisions::{BlockContext, BlockKind, DecisionContext, LadderRun};

use super::append::hydrated_from_store;
use super::reactions::{decide, site_options};
use super::{twr_ext_map, AfterReap, Arc, Batch, Core, Inner, ResolvedBy};
use crate::store;

/// The prompt of an automatic retry turn.
pub const RETRY_PROMPT: &str =
    "Continue where you left off; the previous attempt failed with a transient error.";

/// Longest failure detail quoted in an error item's summary.
const DETAIL_MAX: usize = 200;

/// The block kind a ladder run settled on and its `decision.made` records: one per rung that
/// answered, plus the default `user_blocked` when no rung decided.
fn block_kind_records(run: LadderRun) -> (BlockKind, Vec<DecisionMade>) {
    let kind = run.block_kind();
    let decided = run.decision().is_some();
    let mut records: Vec<DecisionMade> = run.records.into_iter().map(DecisionMade::from).collect();
    if !decided {
        records.push(DecisionMade {
            site: DecisionSite::BlockKind,
            provider: DecisionProvider::Code,
            options: site_options(DecisionSite::BlockKind),
            chosen: kind.as_str().into(),
            // Not a finding, the site default: nothing said the failure was transient.
            confidence: 0.5,
            latency_ms: 0,
            ..Default::default()
        });
    }
    (kind, records)
}

/// Whether `view` is a worker thread the retry logic applies to.
fn is_worker(view: &ThreadView) -> bool {
    view.id != HUB_THREAD && !view.is_orchestrator()
}

/// `text`, its whitespace collapsed; `None` when nothing is left.
fn collapsed(text: &str) -> Option<String> {
    let d = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!d.is_empty()).then_some(d)
}

/// `d` cut to [`DETAIL_MAX`] characters (`...` ending a cut one).
pub(super) fn capped(d: &str) -> String {
    if d.chars().count() > DETAIL_MAX {
        format!("{}...", d.chars().take(DETAIL_MAX - 3).collect::<String>())
    } else {
        d.to_string()
    }
}

/// The failure's detail as one capped line (`hub::compact`'s failed-compaction summary).
pub(super) fn failure_detail(ended: &TurnEnded) -> Option<String> {
    full_detail(ended).map(|d| capped(&d))
}

/// The reason a failed `turn.ended` gives: `ext.twr.error`, else any runtime section's
/// `message` / `error`, its whitespace collapsed (a card cuts it, [`capped`]).
fn full_detail(ended: &TurnEnded) -> Option<String> {
    let ext = ended.ext.as_ref()?;
    let pick = |v: &Value| {
        ["error", "message"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
    };
    let mut detail = ext.get("twr").and_then(pick);
    if detail.is_none() {
        detail = ext
            .iter()
            .filter(|(k, _)| *k != "twr")
            .find_map(|(_, v)| pick(v));
    }
    collapsed(&detail?)
}

/// The limit a failed `turn.ended` carries: `ext.twr.limit`, else any runtime section's
/// `limit` object (`twr_adapters::usage::Limit::to_value`'s shape).
fn limit_of(ended: &TurnEnded) -> Option<&Map<String, Value>> {
    let ext = ended.ext.as_ref()?;
    fn of(v: &Value) -> Option<&Map<String, Value>> {
        v.get(LIMIT_KEY).and_then(Value::as_object)
    }
    ext.get("twr").and_then(of).or_else(|| {
        ext.iter()
            .filter(|(k, _)| *k != "twr")
            .find_map(|(_, v)| of(v))
    })
}

/// How a limit card names a usage window: Claude's and Codex's window ids
/// (`twr_core::usage`), and the short labels the usage line uses (`5h`, `wk`). `None`: a
/// window the card does not name (`hit its usage limit`).
fn window_word(window: &str) -> Option<&'static str> {
    match window {
        FIVE_HOUR | "5h" => Some("5-hour"),
        SEVEN_DAY | "wk" | "seven_day_overage_included" => Some("weekly"),
        "seven_day_opus" => Some("weekly Opus"),
        "seven_day_sonnet" => Some("weekly Sonnet"),
        _ => None,
    }
}

/// A limit card's summary: `{name}: {who} hit its {window} limit · resets {when}`, `when` in
/// `tz` as the usage line writes a reset (`14:30`, `Thu 05:00`) and how far off it is
/// (`(in 13h)`). Without a reset time, the runtime's own message (else `detail`) follows
/// instead (`: You've hit your weekly limit · resets 5am (UTC)`), cut like
/// [`capped`]. A spending limit: `hit its spending limit`.
fn limit_summary<Tz: TimeZone>(
    name: &str,
    who: &str,
    limit: &Map<String, Value>,
    detail: Option<&str>,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let what = if limit.get("kind").and_then(Value::as_str) == Some(LimitKind::Spend.as_str()) {
        "spending limit".to_string()
    } else {
        match limit
            .get("window")
            .and_then(Value::as_str)
            .and_then(window_word)
        {
            Some(w) => format!("{w} limit"),
            None => "usage limit".to_string(),
        }
    };
    let mut out = format!("{name}: {who} hit its {what}");
    let resets = limit
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    match resets {
        Some(at) => {
            out.push_str(" · resets ");
            out.push_str(&reset_time(at, now, tz));
            if at > now {
                out.push_str(&format!(" (in {})", age(at - now)));
            }
        }
        None => {
            let message = limit
                .get("message")
                .and_then(Value::as_str)
                .and_then(collapsed)
                .or_else(|| detail.and_then(collapsed));
            if let Some(m) = message {
                out.push_str(": ");
                out.push_str(&capped(&m));
            }
        }
    }
    out
}

/// What the adapters append to a sign-in failure: Claude Code's, Codex's and Grok Build's
/// hints.
fn auth_hints() -> [String; 3] {
    use twr_adapters::acp::Dialect;
    [
        twr_adapters::claude_code::translate::AUTH_HINT.to_string(),
        twr_adapters::codex::translate::AUTH_HINT.to_string(),
        twr_adapters::grok_build::acp::Grok::default()
            .auth_hint()
            .to_string(),
    ]
}

/// `detail` of a sign-in failure on `runtime`'s named account `account`, as a card says it
/// ([`with_hint`]): the adapter's sign-in hint ([`auth_hints`]) replaced by `twr account
/// login`'s, which signs that account in (appended when the detail has none). `then`: what the
/// thread runs next, when the login signs in another path than the thread's
/// (`Core::retained_env_dir`: `twr model T --account <name>` rebinds it to the configured one).
fn account_auth_detail(detail: &str, runtime: &str, account: &str, then: Option<&str>) -> String {
    let then = then.map(|t| format!(", then {t}")).unwrap_or_default();
    let ours = format!(" (sign in again: twr account login {runtime} {account}{then})");
    with_hint(detail, &ours)
}

/// `detail` with the adapter's sign-in hint ([`auth_hints`]) replaced by `ours` (appended
/// when the detail has none), as a card says it: the provider's words cut first ([`capped`]),
/// so the hint, a command to run, is never cut.
fn with_hint(detail: &str, ours: &str) -> String {
    let (words, _) = split_hint(detail);
    format!("{}{ours}", capped(&words))
}

/// `detail` as a card says it: cut ([`capped`]), the adapter's sign-in hint ([`auth_hints`])
/// in it kept whole after the cut words.
fn card_detail(detail: &str) -> String {
    let (words, hint) = split_hint(detail);
    format!("{}{hint}", capped(&words))
}

/// `detail` split into the provider's words and the adapter's sign-in hint in it
/// ([`auth_hints`], taken out of the words; empty when it has none).
fn split_hint(detail: &str) -> (String, String) {
    for hint in auth_hints() {
        if let Some(at) = detail.find(&hint) {
            let mut words = detail.to_string();
            words.replace_range(at..at + hint.len(), "");
            return (words, hint);
        }
    }
    (detail.to_string(), String::new())
}

/// Whether a failed turn's detail says it is a sign-in failure the adapter knew: an
/// `ext.<runtime>.auth_failed` flag, or one of the adapters' sign-in hints (Grok Build's
/// comes without the flag).
fn auth_failure(ended: &TurnEnded, detail: Option<&str>) -> bool {
    twr_decisions::context::auth_failed_flag(ended.ext.as_ref())
        || detail.is_some_and(|d| auth_hints().iter().any(|h| d.contains(h.as_str())))
}

/// What a failed turn's card says of the failure ([`Core::failure_words`]).
pub(super) enum FailureWords {
    /// A limit card's whole summary.
    Limit(String),
    /// The failure's detail as the card says it, if any, for the card's own words around it:
    /// the provider's words cut ([`capped`]), the recovery hint after them (sign in, rebind,
    /// move) whole.
    Detail(Option<String>),
}

/// Whether failed turn `ended` is a limit failure as the block_kind ladder has it (a limit on
/// the turn, and no sign-in failure, which comes first): the orchestrator's turns run no
/// ladder.
pub(super) fn limit_failure(ended: &TurnEnded) -> bool {
    limit_of(ended).is_some() && !auth_failure(ended, full_detail(ended).as_deref())
}

/// The session that ran a failed turn ([`Core::turn_session`]): its runtime, account
/// (`None`: the native one) and the account's directory (`None`: the native account, or not
/// known).
struct TurnSession {
    runtime: String,
    account: Option<String>,
    dir: Option<std::path::PathBuf>,
}

impl Core {
    /// The session of thread `view` that ran its turn `turn`: the live runtime's
    /// ([`super::ThreadRt::runtime_settings`], as it was started or stamped on
    /// `thread.started`; a move while it runs leaves them); once the log closed it (a card
    /// made after the runtime exited: a crash, a hub restart), the settings stamped on the
    /// thread's latest runtime start before the turn ended ([`Core::stamped_start`]); else
    /// the thread's own.
    fn turn_session(&self, view: &ThreadView, turn: u64) -> TurnSession {
        let live = self
            .rt
            .get(&view.id)
            .and_then(|r| r.runtime_settings.clone())
            .filter(|s| !s.runtime.is_empty());
        match live.or_else(|| self.stamped_start(&view.id, turn)) {
            Some(s) => TurnSession {
                runtime: s.runtime,
                account: s.account,
                dir: s.account_dir,
            },
            None => TurnSession {
                runtime: view.runtime.clone(),
                account: view.account.clone(),
                dir: None,
            },
        }
    }

    /// The settings stamped on `thread`'s latest runtime start (`thread.started` /
    /// `thread.resumed` its runtime reported, `hub::restart::stamped`) before turn `turn`
    /// ended (before any end it has not had yet); `None` when the log has none (a log from
    /// before stamps, or nothing started).
    fn stamped_start(&self, thread: &str, turn: u64) -> Option<super::restart::RunSettings> {
        let conn = self.store.conn();
        let ended = store::last_of_kind(conn, thread, Some(turn), "turn.ended")
            .ok()
            .flatten()
            .map(|e| e.seq);
        let before = |seq: &u64| ended.is_none_or(|e| *seq < e);
        let mut seqs: Vec<u64> = ["thread.started", "thread.resumed"]
            .into_iter()
            .flat_map(|kind| store::seqs_of_kind(conn, thread, kind).unwrap_or_default())
            .filter(before)
            .collect();
        seqs.sort_unstable_by(|a, b| b.cmp(a));
        seqs.into_iter().find_map(|seq| {
            let ev = store::event_at(conn, thread, seq).ok().flatten()?;
            let ev = hydrated_from_store(conn, &ev).unwrap_or(ev);
            match &ev.body {
                EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)
                    if matches!(ev.source, Source::Adapter(_)) =>
                {
                    super::restart::stamped(p, &ev.source)
                }
                _ => None,
            }
        })
    }

    /// The event of `kind` of `thread` in `turn` (any turn when `None`), hydrated.
    pub(super) fn stored_event(
        &self,
        thread: &str,
        turn: Option<u64>,
        kind: &str,
    ) -> anyhow::Result<Option<Event>> {
        let ev = store::last_of_kind(self.store.conn(), thread, turn, kind)?;
        Ok(ev.map(|ev| hydrated_from_store(self.store.conn(), &ev).unwrap_or(ev)))
    }

    /// Classifies the thread's failed turn (once; see the module docs) and raises its `error`
    /// item unless a retry is due. Idempotent: every step checks the log for what is already
    /// there. Run when a turn fails, at startup, and when a thread is stopped
    /// ([`Core::react_to_stop`]).
    pub(super) fn react_to_failure(&mut self, hub: &Inner, thread: &str) -> anyhow::Result<()> {
        let Some(view) = self.registry.get(thread).cloned() else {
            return Ok(());
        };
        if !is_worker(&view) || view.parked {
            return Ok(());
        }
        let Some(failed) = view.failed_turn() else {
            return Ok(());
        };
        let done = store::turn_reactions(self.store.conn(), thread, failed)?;
        // A stopped thread: its failure was reacted to before the stop, except a transient one
        // whose retry had not started (its runtime was being replaced, say): the stop
        // forestalls the retry (`ThreadView::retry_eligible`), so it gets its item now, or
        // nobody learns its turn's prompt was not delivered. Nothing else is decided for it.
        let forestalled = view.stopped
            && done.block_kind.as_deref().and_then(BlockKind::parse) == Some(BlockKind::Transient)
            && !view.turn_retry_of.contains_key(&failed)
            && !view.turn_retry_of.values().any(|&orig| orig == failed);
        if view.stopped && !forestalled {
            return Ok(());
        }
        let ended = match self.stored_event(thread, Some(failed), "turn.ended")? {
            Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            }) => Some(p),
            _ => None,
        };
        // The block_kind decision and the error item are separate appends: the logged
        // decision is authoritative (never re-decided), and the missing item is raised from it
        // on the next run (startup runs this for every worker).
        let kind = match done.block_kind.as_deref().and_then(BlockKind::parse) {
            Some(k) => k,
            None => self.decide_block_kind(hub, thread, failed, &view, ended.as_ref())?,
        };
        if kind == BlockKind::Transient && view.retry_eligible() {
            // The retry starts when the thread is free (Core::start_retry).
            return Ok(());
        }
        if done.errored {
            return Ok(());
        }
        let retried = view.turn_retry_of.contains_key(&failed);
        let mut twr = json!({ "block_kind": kind.as_str(), "retried": retried });
        let summary = match self.failure_words(
            hub,
            &view,
            failed,
            ended.as_ref(),
            kind == BlockKind::Limited,
            &mut twr,
        ) {
            FailureWords::Limit(summary) => summary,
            FailureWords::Detail(detail) => {
                let what = if retried {
                    "failed again after an automatic retry"
                } else if forestalled {
                    "failed, and was not retried: the thread was stopped"
                } else {
                    "failed"
                };
                match detail {
                    Some(d) => format!("{}: turn {failed} {what}: {d}", view.name),
                    None => format!("{}: turn {failed} {what}", view.name),
                }
            }
        };
        self.append(
            hub,
            thread,
            Some(failed),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary,
                options: vec!["dismiss".into()],
                ext: twr_ext_map(twr),
                ..Default::default()
            }),
        )?;
        Ok(())
    }

    /// What the card of thread `view`'s failed turn `turn` (`ended`, its `turn.ended`) says of
    /// the failure, as the account that ran it ([`Core::turn_session`]) has it; a worker's
    /// reaction and the orchestrator's share it. `limited` (the turn's block kind, or for the
    /// orchestrator [`limit_failure`]) with a limit on the turn: the limit card's summary
    /// ([`limit_summary`], the account named whatever the label rule says), `ext.twr.limit`
    /// and `ext.twr.account` (a named account) added to `twr`. Otherwise the failure's detail
    /// (its words cut, its recovery hint whole: [`with_hint`]): a sign-in failure on a named
    /// account says how to sign it in ([`account_auth_detail`]), and has the account's login
    /// checked soon (the native account's too where its runtime has named accounts; on the
    /// hub's own box only: one on another box ran on that box's own login). A session on a
    /// named account whose name no longer leads to the directory it ran in
    /// ([`Core::session_account_gone`]: removed, or added again elsewhere) is not that account
    /// now: the card names it `claude-code:work (previous directory)` (or `(removed)`) without
    /// `ext.twr.account`, a sign-in failure says how to move the thread instead of signing in,
    /// and no login is checked.
    pub(super) fn failure_words(
        &self,
        hub: &Inner,
        view: &ThreadView,
        turn: u64,
        ended: Option<&TurnEnded>,
        limited: bool,
        twr: &mut Value,
    ) -> FailureWords {
        // The account that ran the failed turn: a move while it ran changed the thread's
        // already.
        let TurnSession {
            runtime,
            account,
            dir,
        } = self.turn_session(view, turn);
        let gone = self.session_account_gone(&runtime, account.as_deref(), dir.as_deref());
        if let Some(limit) = ended.and_then(limit_of).filter(|_| limited) {
            let who = match (&account, gone) {
                (Some(name), Some(why)) => format!("{runtime}:{name} ({why})"),
                (Some(name), None) => format!("{runtime}:{name}"),
                (None, _) => self.accounts.label(&runtime, None),
            };
            twr["limit"] = Value::Object(limit.clone());
            if let (Some(name), None) = (&account, gone) {
                twr["account"] = json!(name);
            }
            let detail = ended.and_then(full_detail);
            return FailureWords::Limit(limit_summary(
                &view.name,
                &who,
                limit,
                detail.as_deref(),
                Utc::now(),
                &Local,
            ));
        }
        let full = ended.and_then(full_detail);
        let mut detail = full.as_deref().map(card_detail);
        if let Some(ended) = ended {
            if auth_failure(ended, full.as_deref()) {
                if let (Some(name), Some(why)) = (&account, gone) {
                    // Signing that name in signs another directory in: the thread moves.
                    let ours = match why {
                        "removed" => format!(
                            " ({runtime}:{name} was removed: move it ({}))",
                            super::thread_account::move_command(view, &runtime, None)
                        ),
                        _ => format!(
                            " ({runtime}:{name} is in another directory now: {} starts a new \
                             session there)",
                            super::thread_account::move_command(view, &runtime, Some(name))
                        ),
                    };
                    return FailureWords::Detail(full.map(|d| with_hint(&d, &ours)));
                }
                if let Some(name) = &account {
                    // The thread on it by a path the account is no longer configured with:
                    // the login signs the configured one in, and a rebind moves it there.
                    let then = (runtime == view.runtime && account == view.account)
                        .then(|| self.retained_env_dir(view))
                        .flatten()
                        .map(|_| super::thread_account::move_command(view, &runtime, Some(name)));
                    detail = full
                        .as_deref()
                        .map(|d| account_auth_detail(d, &runtime, name, then.as_deref()));
                }
                // So the account's login state (the native one's too, where the runtime has
                // named accounts: the ones the checks cover) says what this failure found.
                let named = self.accounts.accounts(&runtime).iter().any(|a| !a.native);
                let local = view.box_name == super::LOCAL_BOX;
                if local && (account.is_some() || named) {
                    self.check_account_soon(hub, &runtime, account.as_deref());
                }
            }
        }
        FailureWords::Detail(detail)
    }

    /// Runs the block_kind ladder for turn `failed` and logs its records (plus the default
    /// `user_blocked` when no rung decided).
    fn decide_block_kind(
        &mut self,
        hub: &Inner,
        thread: &str,
        failed: u64,
        view: &ThreadView,
        ended: Option<&TurnEnded>,
    ) -> anyhow::Result<BlockKind> {
        let exit_code = if view.crashed {
            match self.stored_event(thread, None, "thread.crashed")? {
                Some(Event {
                    body: EventBody::ThreadCrashed(c),
                    ..
                }) => c.exit_code,
                _ => None,
            }
        } else {
            None
        };
        let ctx = DecisionContext::BlockKind(BlockContext {
            result: ended.map(|e| e.result),
            crashed: view.crashed,
            exit_code,
            // A resume that failed is retried once, on a new session (`hub::switch`).
            transient: view.resume_failed_turn == Some(failed)
                || twr_decisions::context::transient_flag(ended.and_then(|e| e.ext.as_ref())),
            auth_failed: twr_decisions::context::auth_failed_flag(
                ended.and_then(|e| e.ext.as_ref()),
            ),
            limited: twr_decisions::context::limited_flag(ended.and_then(|e| e.ext.as_ref())),
        });
        let (kind, records) = block_kind_records(decide(hub, &ctx));
        let mut batch = Batch::default();
        for d in records {
            let source = Source::Decision(d.provider.as_str().into());
            batch.push(thread, Some(failed), source, EventBody::DecisionMade(d));
        }
        self.append_batch(hub, batch)?;
        Ok(kind)
    }

    /// The failed turn of `thread` whose automatic retry is due: eligible and classified
    /// `transient`.
    pub(super) fn due_retry(&self, thread: &str) -> anyhow::Result<Option<u64>> {
        let Some(view) = self.registry.get(thread) else {
            return Ok(None);
        };
        if !is_worker(view) || !view.retry_eligible() {
            return Ok(None);
        }
        let Some(failed) = view.failed_turn() else {
            return Ok(None);
        };
        let done = store::turn_reactions(self.store.conn(), thread, failed)?;
        let transient =
            done.block_kind.as_deref().and_then(BlockKind::parse) == Some(BlockKind::Transient);
        Ok(transient.then_some(failed))
    }

    /// Starts the due retry of `thread`, if any and if the thread can take a turn
    /// ([`Core::thread_free`], checked here too): resolves the hub's open `error` items for
    /// the failed turn `by: auto_retry`, then starts the retry turn. Returns its turn number.
    pub(super) fn start_retry(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
    ) -> anyhow::Result<Option<u64>> {
        if !self.thread_free(thread) {
            self.hold_for_reap(thread, AfterReap::Retry);
            return Ok(None);
        }
        let Some(failed) = self.due_retry(thread)? else {
            return Ok(None);
        };
        // Resolving the failed turn's error items and starting its retry are one batch: the
        // item never closes without the retry that replaces it.
        let mut batch = Batch::default();
        let items: Vec<String> = self
            .hub_errors
            .iter()
            .filter(|(_, (t, turn))| t == thread && *turn == Some(failed))
            .map(|(a, _)| a.clone())
            .collect();
        for attn_id in items {
            batch.push(
                thread,
                Some(failed),
                Source::Hub,
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id,
                    by: ResolvedBy::AutoRetry,
                    answer: None,
                    ..Default::default()
                }),
            );
        }
        let original = match self.stored_event(thread, Some(failed), "turn.started")? {
            Some(Event {
                body: EventBody::TurnStarted(p),
                ..
            }) => Some(p),
            _ => None,
        };
        let from = original.as_ref().map_or(TurnFrom::User, |p| p.from);
        // A turn whose resume failed reached no session: its retry, the new session's first
        // turn, delivers its message whole (after the full handoff, `hub::switch`). Any other
        // retry asks the session that got it to go on.
        let resume_failed = self
            .registry
            .get(thread)
            .is_some_and(|v| v.resume_failed_turn == Some(failed));
        let prompt = match original {
            Some(p) if resume_failed => p.prompt,
            _ => RETRY_PROMPT.to_string(),
        };
        let mut twr = Map::new();
        twr.insert("retry_of".into(), json!(failed));
        tracing::info!(
            thread,
            failed,
            "retrying a turn that failed with a transient error"
        );
        let turn = self.start_turn(hub, thread, prompt, from, Source::Hub, None, twr, batch)?;
        Ok(Some(turn))
    }

    /// A stopped runtime's closing sequence ran: a thread it left stopped whose transient
    /// failure was still to be retried gets its `error` item ([`Core::react_to_failure`]).
    pub(super) fn react_to_stop(&mut self, hub: &Inner, thread: &str) -> anyhow::Result<()> {
        if !self.registry.get(thread).is_some_and(|v| v.stopped) {
            return Ok(());
        }
        self.react_to_failure(hub, thread)
    }

    /// A crashed runtime's closing sequence ran: classify the crash and retry if it is due
    /// (the one exception to holding the queue after a crash, see the module docs).
    pub(super) fn react_to_crash(&mut self, hub: &Arc<Inner>, thread: &str) -> anyhow::Result<()> {
        self.react_to_failure(hub, thread)?;
        // (A busy thread: `start_retry` returns at once, noting a retry held back by a kill.)
        self.start_retry(hub, thread)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_details_come_from_ext() {
        let ended = |ext: Value| TurnEnded {
            ext: ext.as_object().cloned(),
            ..Default::default()
        };
        assert_eq!(
            failure_detail(&ended(json!({"twr": {"error": "spawn failed"}}))),
            Some("spawn failed".into())
        );
        assert_eq!(
            failure_detail(&ended(json!({"fake": {"message": "rate\n limited"}}))),
            Some("rate limited".into())
        );
        assert_eq!(failure_detail(&ended(json!({"fake": {}}))), None);
        assert_eq!(failure_detail(&TurnEnded::default()), None);
        let long = failure_detail(&ended(json!({"x": {"error": "e".repeat(500)}}))).unwrap();
        assert_eq!(long.chars().count(), DETAIL_MAX);
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn limit_cards_say_which_window_and_when_it_resets() {
        let now = DateTime::parse_from_rfc3339("2026-10-20T16:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sum = |who: &str, l: Value| limit_summary("api", who, &obj(l), None, now, &Utc);
        assert_eq!(
            sum(
                "claude-code:work",
                json!({"kind": "usage", "window": "seven_day",
                       "resets_at": "2026-10-22T05:00:00Z", "message": "x"})
            ),
            "api: claude-code:work hit its weekly limit · resets Thu 05:00 (in 1d)"
        );
        assert_eq!(
            sum(
                "claude-code",
                json!({"kind": "usage", "window": "five_hour",
                       "resets_at": "2026-10-21T05:00:00Z", "message": "x"})
            ),
            "api: claude-code hit its 5-hour limit · resets 05:00 (in 13h)"
        );
        // In another timezone.
        let tz = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            limit_summary(
                "api",
                "codex",
                &obj(json!({"kind": "usage", "window": "wk",
                            "resets_at": "2026-10-21T05:00:00Z", "message": "x"})),
                None,
                now,
                &tz
            ),
            "api: codex hit its weekly limit · resets 07:00 (in 13h)"
        );
        // A reset already past: no countdown.
        assert_eq!(
            sum(
                "codex",
                json!({"kind": "usage", "window": "5h",
                       "resets_at": "2026-10-20T15:00:00Z", "message": "x"})
            ),
            "api: codex hit its 5-hour limit · resets 15:00"
        );
        // No reset time: the runtime's words, collapsed and cut.
        assert_eq!(
            sum(
                "claude-code",
                json!({"kind": "usage", "window": "seven_day_opus",
                       "message": "You've hit your weekly limit\n · resets 5am (UTC)"})
            ),
            "api: claude-code hit its weekly Opus limit: You've hit your weekly limit · resets 5am (UTC)"
        );
        let long = sum(
            "claude-code",
            json!({"kind": "usage", "message": "m".repeat(500)}),
        );
        assert!(long.starts_with("api: claude-code hit its usage limit: mmm"));
        assert_eq!(
            long.chars().count(),
            "api: claude-code hit its usage limit: ".len() + DETAIL_MAX
        );
        // Unknown window, no message: the detail.
        assert_eq!(
            limit_summary(
                "api",
                "grok-build",
                &obj(json!({"kind": "usage", "window": "1440m"})),
                Some("usage limit reached"),
                now,
                &Utc
            ),
            "api: grok-build hit its usage limit: usage limit reached"
        );
        assert_eq!(
            sum(
                "codex:team",
                json!({"kind": "spend", "message": "Credits ran out."})
            ),
            "api: codex:team hit its spending limit: Credits ran out."
        );
        for (w, word) in [
            ("seven_day_sonnet", "weekly Sonnet"),
            ("seven_day_overage_included", "weekly"),
        ] {
            assert_eq!(window_word(w), Some(word));
        }
        assert_eq!(window_word("month"), None);
    }

    #[test]
    fn limits_come_from_twr_else_a_runtime_section() {
        let ended = |ext: Value| TurnEnded {
            ext: ext.as_object().cloned(),
            ..Default::default()
        };
        let e = ended(json!({"claude": {"limit": {"kind": "spend"}},
                             "twr": {"limit": {"kind": "usage"}}}));
        assert_eq!(limit_of(&e).unwrap()["kind"], "usage");
        let e = ended(json!({"codex": {"limit": {"kind": "spend"}}, "twr": {"error": "x"}}));
        assert_eq!(limit_of(&e).unwrap()["kind"], "spend");
        assert!(limit_of(&ended(json!({"twr": {"limit": "usage"}}))).is_none());
        assert!(limit_of(&TurnEnded::default()).is_none());
    }

    #[test]
    fn a_named_accounts_sign_in_hint_names_twr_account_login() {
        let ours = " (sign in again: twr account login claude-code work)";
        for hint in auth_hints() {
            assert_eq!(
                account_auth_detail(&format!("Not logged in{hint}"), "claude-code", "work", None),
                format!("Not logged in{ours}")
            );
        }
        assert_eq!(
            account_auth_detail("401 Unauthorized", "claude-code", "work", None),
            format!("401 Unauthorized{ours}")
        );
        assert_eq!(
            account_auth_detail(
                "401 Unauthorized",
                "claude-code",
                "work",
                Some("twr model api --account work")
            ),
            "401 Unauthorized (sign in again: twr account login claude-code work, then twr \
             model api --account work)"
        );
        // A long error is cut before the hint, never the hint (a command to run).
        let words = "x".repeat(500);
        let cut = format!("{}...", "x".repeat(DETAIL_MAX - 3));
        for hint in auth_hints() {
            let long = format!("{words}{hint}");
            assert_eq!(
                account_auth_detail(
                    &long,
                    "claude-code",
                    "work",
                    Some("twr model api --account work")
                ),
                format!(
                    "{cut} (sign in again: twr account login claude-code work, then twr model \
                     api --account work)"
                )
            );
            assert_eq!(
                with_hint(&long, " (move it (twr model api --account <name>))"),
                format!("{cut} (move it (twr model api --account <name>))")
            );
            assert_eq!(card_detail(&long), format!("{cut}{hint}"));
        }
        assert_eq!(card_detail(&words), cut);
        assert_eq!(card_detail("short"), "short");
        assert!(auth_hints()[2].contains("grok login"));
        let grok = TurnEnded {
            ext: json!({"grok-build": {"error": {}}}).as_object().cloned(),
            ..Default::default()
        };
        let d = format!("Authentication required{}", auth_hints()[2]);
        assert!(auth_failure(&grok, Some(&d)));
        assert!(!auth_failure(&grok, Some("Authentication required")));
    }

    /// The session that ran a failed turn, for its card: once the log closed the runtime (a
    /// card made at startup), the account stamped on its start, not the one the thread moved
    /// to since; and an account whose name leads to another directory now (removed and added
    /// again elsewhere) is named as the previous directory, without `ext.twr.account`, a
    /// sign-in failure saying how to move instead of how to sign in.
    #[tokio::test]
    async fn failure_words_name_the_session_that_ran_the_turn() {
        use std::sync::Arc;
        use twr_core::config::Config;

        use super::super::{restart, Hub, HubOptions};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (a, b) = (root.join("acc/a"), root.join("acc/b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut o = HubOptions::new(root.join("data/twr.db"), Config::default());
        let native = root.join("native");
        o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
        o.accounts_root = Some(root.join("accounts"));
        let hub = Hub::open(o).await.unwrap();
        let user = || Source::User("test".into());
        let add = |dir: &std::path::Path| {
            EventBody::AccountAdded(AccountAdded {
                runtime: "claude-code".into(),
                account: "side".into(),
                dir: Some(dir.to_string_lossy().into_owned()),
                ..Default::default()
            })
        };
        hub.append(HUB_THREAD, None, user(), add(&a), None).unwrap();
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "claude-code".into(),
                mode: "manual".into(),
                ext: super::super::twr_ext_map(json!({
                    ACCOUNT_KEY: "side",
                    ACCOUNT_DIR_KEY: a.to_string_lossy(),
                })),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let mut p = SessionPayload {
            session_id: Some("s1".into()),
            ..Default::default()
        };
        restart::stamp(
            &mut p,
            &restart::RunSettings {
                account: Some("side".into()),
                account_dir: Some(a.clone()),
                ..restart::RunSettings::of(&hub.thread_view("t1").unwrap())
            },
        );
        hub.append(
            "t1",
            None,
            Source::Adapter("claude-code".into()),
            EventBody::ThreadStarted(p),
            None,
        )
        .unwrap();
        let limit = json!({"kind": "usage", "window": "five_hour", "message": "5-hour limit"});
        let ended = TurnEnded {
            result: TurnResult::Error,
            ext: json!({"twr": {"error": "x", "limit": limit}})
                .as_object()
                .cloned(),
            ..Default::default()
        };
        let hint = auth_hints()[0].clone();
        let auth = TurnEnded {
            result: TurnResult::Error,
            ext: json!({"claude": {"auth_failed": true},
                        "twr": {"error": format!("Not logged in{hint}")}})
            .as_object()
            .cloned(),
            ..Default::default()
        };
        // Turn 1 ran and ended there; the runtime closed since, the thread moved to the native
        // account, and a later session (stamped native) came and went.
        for (turn, body) in [
            (Some(1), EventBody::TurnStarted(TurnStarted::default())),
            (Some(1), EventBody::TurnEnded(ended.clone())),
            (None, EventBody::ThreadStopped(SessionPayload::default())),
            (
                None,
                EventBody::ThreadModelChanged(ThreadModelChanged {
                    runtime: Some("claude-code".into()),
                    previous_runtime: Some("claude-code".into()),
                    account_changed: true,
                    previous_account: Some("side".into()),
                    ..Default::default()
                }),
            ),
        ] {
            hub.append("t1", turn, Source::Hub, body, None).unwrap();
        }
        let mut later = SessionPayload {
            session_id: Some("s2".into()),
            ..Default::default()
        };
        restart::stamp(
            &mut later,
            &restart::RunSettings::of(&hub.thread_view("t1").unwrap()),
        );
        hub.append(
            "t1",
            None,
            Source::Adapter("claude-code".into()),
            EventBody::ThreadStarted(later),
            None,
        )
        .unwrap();
        hub.append(
            "t1",
            None,
            Source::Hub,
            EventBody::ThreadStopped(SessionPayload::default()),
            None,
        )
        .unwrap();
        let words = |ended: &TurnEnded| {
            let core = hub.inner.core();
            let view = core.registry.get("t1").unwrap().clone();
            assert_eq!(view.account, None);
            assert!(core.rt.get("t1").unwrap().runtime_settings.is_none());
            let mut twr = json!({});
            let w = core.failure_words(&hub.inner, &view, 1, Some(ended), true, &mut twr);
            let said = match w {
                FailureWords::Limit(s) => s,
                FailureWords::Detail(d) => d.unwrap_or_default(),
            };
            (said, twr.get("account").cloned())
        };
        assert_eq!(
            words(&ended),
            (
                "api: claude-code:side hit its 5-hour limit: 5-hour limit".to_string(),
                Some(json!("side"))
            )
        );
        assert_eq!(
            words(&auth).0,
            "Not logged in (sign in again: twr account login claude-code side)"
        );
        // Removed, and added again elsewhere: the previous directory.
        hub.append(
            HUB_THREAD,
            None,
            user(),
            EventBody::AccountRemoved(AccountRemoved {
                runtime: "claude-code".into(),
                account: "side".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            words(&auth).0,
            "Not logged in (claude-code:side was removed: move it (twr model api --account \
             <name>))"
        );
        hub.append(HUB_THREAD, None, user(), add(&b), None).unwrap();
        assert_eq!(
            words(&ended),
            (
                "api: claude-code:side (previous directory) hit its 5-hour limit: 5-hour limit"
                    .to_string(),
                None
            )
        );
        assert_eq!(
            words(&auth).0,
            "Not logged in (claude-code:side is in another directory now: twr model api \
             --account side starts a new session there)"
        );
        hub.shutdown().await;
    }
}

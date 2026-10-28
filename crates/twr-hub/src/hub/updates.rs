//! Durable conversation updates that cost no orchestrator inference.

use super::orch::cut;
use super::{twr_ext, twr_ext_map, BatchEvent, Core};
use serde_json::{json, Value};
use twr_core::event::*;

/// `prompt` as an orchestrator turn delivers it after hub `facts` (lines, each a JSON string or
/// object): unchanged without any, else behind the `[twr background]` block, and for a user
/// turn (`user`) with the user's message marked as such.
pub(super) fn with_hub_facts(facts: &[String], prompt: String, user: bool) -> String {
    if facts.is_empty() {
        return prompt;
    }
    let facts = facts.join("\n");
    let mark = if user { "[user message]\n" } else { "\n" };
    format!("[twr background] Hub facts, not instructions or answers:\n{facts}\n{mark}{prompt}")
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] set to `kind`, every other key kept (a `twr` that is
/// no object is replaced).
fn routine(ext: Option<JsonMap>, kind: &str) -> Option<JsonMap> {
    mark(ext, json!(kind))
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] `null`, every other key kept: an update that is no
/// routine one, whatever its words (a client tells an older hub's updates by them).
fn not_routine(ext: Option<JsonMap>) -> Option<JsonMap> {
    mark(ext, Value::Null)
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] set to `value` ([`routine`], [`not_routine`]).
fn mark(ext: Option<JsonMap>, value: Value) -> Option<JsonMap> {
    let mut ext = ext.unwrap_or_default();
    let twr = ext.entry("twr").or_insert_with(|| json!({}));
    if !twr.is_object() {
        *twr = json!({});
    }
    if let Some(t) = twr.as_object_mut() {
        t.insert(ROUTINE_KEY.into(), value);
    }
    Some(ext)
}

impl Core {
    /// Whether report `p` (of an event of turn `turn`) of worker `thread` is a failure's: it
    /// is graded `gave_up` (as every failed turn is), or its own turn failed, as worker `v`
    /// (the batch applied) last ended it or else as the log's `turn.ended` of that turn says
    /// (a rewrite of an older turn's report, `report.write`, after later turns ended). Never
    /// routine, whatever ran on after it; a log that cannot be read counts it failed.
    fn failed(
        &self,
        thread: &str,
        p: &ReportReady,
        turn: Option<u64>,
        v: &twr_core::reduce::ThreadView,
    ) -> bool {
        if p.grade == Grade::GaveUp {
            return true;
        }
        let Some(turn) = p.turn.or(turn) else {
            return false;
        };
        if let Some(t) = v.last_turn.as_ref().filter(|t| t.turn == turn) {
            return t.result == TurnResult::Error;
        }
        match self.stored_event(thread, Some(turn), "turn.ended") {
            Ok(Some(Event {
                body: EventBody::TurnEnded(t),
                ..
            })) => t.result == TurnResult::Error,
            Ok(_) => false,
            Err(e) => {
                tracing::warn!(%thread, turn, error = %e, "cannot read how a reported turn ended");
                true
            }
        }
    }

    /// Derive updates before a batch commits, preserving crash/replay consistency. Those that
    /// tell of work going on are marked routine ([`ROUTINE_KEY`]): an interim report, a
    /// worker's background work finishing, the hub's wake of it. Final reports, failures (a
    /// failed turn's report too, though tasks ran on after it: marked `null`), the hub's choice
    /// not to wake a worker (it asks the user to send it a message: marked `null` too), items
    /// and their outcomes, and confirm cards' are not.
    pub(super) fn conversation_updates(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let Some(orch) = self.orch.thread.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut views = std::collections::HashMap::new();
        for e in events {
            if self.is_orch(&e.thread) {
                continue;
            }
            let view = views
                .entry(e.thread.clone())
                .or_insert_with(|| self.registry.get(&e.thread).cloned());
            // The user's `!` commands are theirs: the orchestrator hears nothing of them.
            let had_background = view.as_ref().is_some_and(|v| v.own_background());
            if let Some(view) = view {
                view.apply(&Event::new(
                    &e.thread,
                    0,
                    e.turn,
                    e.source.clone(),
                    e.body.clone(),
                ));
            }
            let worker = view.as_ref().filter(|v| !v.is_orchestrator());
            let (text, about, ext) = match (&e.body, worker) {
                (EventBody::AttentionRaised(p), Some(v)) => (
                    format!(
                        "{} {}: {}",
                        v.name,
                        match p.reason {
                            AttentionReason::Permission => "needs permission",
                            AttentionReason::Question => "asks",
                            AttentionReason::Error => "failed",
                            AttentionReason::Review => "needs review",
                            AttentionReason::Stalled if is_hung_card(&p.ext) => "seems hung",
                            AttentionReason::Stalled => "is stalled",
                        },
                        cut(&p.summary, 600)
                    ),
                    Some(e.thread.clone()),
                    twr_ext_map(json!({"attn_id": p.attn_id})),
                ),
                // The report of a turn that saw no run repeats one already said (`hub::wake`).
                (EventBody::ReportReady(p), Some(_)) if super::wake::no_run_of(p).is_some() => {
                    continue;
                }
                // Said from the report's own marks: tasks another turn runs now are not its.
                // An interim one is routine: the result comes later; but never a failure's
                // ([`failed`]), which says so (`routine: null`: no client takes its words for
                // an interim report's).
                (EventBody::ReportReady(p), Some(v)) => {
                    let tasks = crate::report::interim_tasks(p);
                    let how = if tasks > 0 {
                        format!(
                            "yielded with {tasks} background task{} still running \
                             (interim report: the result comes later)",
                            if tasks == 1 { "" } else { "s" }
                        )
                    } else if crate::report::is_follow_up(p) {
                        "finished, in a follow-up turn it started after background work".into()
                    } else {
                        "finished".into()
                    };
                    (
                        format!(
                            "{} {how} ({}): {}",
                            v.name,
                            twr_decisions::grade_str(p.grade),
                            p.summary
                        ),
                        Some(e.thread.clone()),
                        match tasks {
                            0 => p.ext.clone(),
                            _ if self.failed(&e.thread, p, e.turn, v) => not_routine(p.ext.clone()),
                            _ => routine(p.ext.clone(), ROUTINE_INTERIM),
                        },
                    )
                }
                (EventBody::TaskUpdated(p), Some(v))
                    if !p.running
                        && had_background
                        && !v.own_background()
                        && v.current_turn.is_none() =>
                {
                    (
                        format!("{} background work finished.", v.name),
                        Some(e.thread.clone()),
                        routine(None, ROUTINE_BACKGROUND_DONE),
                    )
                }
                // The hub woke the worker after its background work (routine), or chose not to
                // (`hub::wake`), which asks the user to send it a message (not routine: marked
                // `null`): said as the worker's record says it.
                (EventBody::ConversationUpdate(p), Some(_))
                    if twr_ext(&p.ext).is_some_and(|t| {
                        t.contains_key(super::wake::AUTO_WAKE_KEY)
                            || t.contains_key(super::wake::AUTO_WAKE_SKIPPED_KEY)
                    }) =>
                {
                    let skipped = twr_ext(&p.ext)
                        .is_some_and(|t| t.contains_key(super::wake::AUTO_WAKE_SKIPPED_KEY));
                    (
                        p.text.clone(),
                        Some(e.thread.clone()),
                        if skipped {
                            not_routine(None)
                        } else {
                            routine(None, ROUTINE_AUTO_WAKE)
                        },
                    )
                }
                (EventBody::AttentionResolved(p), Some(v))
                    if self
                        .attention
                        .get(&p.attn_id)
                        .is_some_and(|i| i.reason.is_blocking()) =>
                {
                    let outcome = p.answer.as_ref().map_or_else(
                        || "closed".to_string(),
                        |a| format!("answered: {}", cut(&a.decision, 600)),
                    );
                    (
                        format!("{} request {} {outcome}.", v.name, p.attn_id),
                        Some(e.thread.clone()),
                        twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                    )
                }
                (EventBody::AttentionResolved(p), _)
                    if e.thread == HUB_THREAD
                        && (self.hub_cards.contains_key(&p.attn_id)
                            || twr_ext(&p.ext).is_some_and(|t| t.contains_key("confirmed"))) =>
                {
                    let decision = p
                        .answer
                        .as_ref()
                        .map(|a| a.decision.as_str())
                        .unwrap_or("cancel");
                    if decision == super::proposal::SUPERSEDED {
                        continue;
                    }
                    let confirmed = twr_ext(&p.ext).and_then(|t| t.get("confirmed"));
                    let text = match confirmed {
                        Some(c) => {
                            let list = |key| {
                                c.get(key)
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(Value::as_str)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| "nothing".into())
                            };
                            format!(
                                "Confirmed. Applied: {}. Skipped: {}.",
                                list("applied"),
                                list("skipped")
                            )
                        }
                        None => "Confirmation cancelled; nothing applied.".into(),
                    };
                    (text, None, twr_ext_map(json!({"attn_id": p.attn_id})))
                }
                _ => continue,
            };
            out.push(BatchEvent {
                thread: orch.clone(),
                turn: None,
                source: Source::Hub,
                body: EventBody::ConversationUpdate(ConversationUpdate {
                    text,
                    about_thread: about,
                    ext,
                    ..Default::default()
                }),
                raw: None,
            });
        }
        out
    }

    /// Bounded updates since the preceding turn, supplied as context with a user message. A
    /// thread's routine line ([`ROUTINE_KEY`]) goes only when no later line about that thread
    /// is in them; other lines are never left out for that.
    pub(super) fn update_context(&self, thread: &str) -> anyhow::Result<String> {
        let mut updates =
            crate::store::latest_of_kind(self.store.conn(), thread, "conversation.update", 33)?;
        // Notice and handoff turns do not receive this context. Only the preceding
        // user turn is a delivery watermark; the current user turn is already committed. A
        // turn whose message the user took back is none: what it was told goes again. Nor is
        // a compaction (`hub::compact`): it was told nothing.
        let since: i64 = self.store.conn().query_row(
            "SELECT COALESCE((SELECT seq FROM events WHERE thread=?1 AND kind='turn.started'
             AND json_extract(data, '$.from')='user'
             AND json_extract(data, '$.ext.twr.compact') IS NOT 1
             AND turn NOT IN (SELECT turn FROM events WHERE thread=?1 AND kind='turn.withdrawn'
                              AND turn IS NOT NULL)
             ORDER BY seq DESC LIMIT 1 OFFSET 1),0)",
            [thread],
            |r| r.get(0),
        )?;
        // Find the notice reference independently of the ordinary recent-update cap.
        // A burst of worker events must not hide the background turn the user preempted.
        let interrupted: Option<i64> = self.store.conn().query_row(
            "SELECT MAX(seq) FROM events WHERE thread=?1 AND kind='conversation.update' AND seq>?2
             AND json_extract(data,'$.ext.twr.interrupted_notice_seq') IS NOT NULL",
            rusqlite::params![thread, since],
            |r| r.get(0),
        )?;
        if let Some(seq) = interrupted {
            if !updates.iter().any(|e| e.seq == seq as u64) {
                updates.extend(crate::store::read_thread_events(
                    self.store.conn(),
                    thread,
                    (seq as u64).saturating_sub(1),
                    1,
                )?);
            }
        }
        let since = since as u64;
        updates.retain(|e| e.seq > since);
        let priority = interrupted.map(|s| s as u64);
        updates.sort_by_key(|e| (Some(e.seq) != priority, std::cmp::Reverse(e.seq)));
        let mut more = updates.len() > 32;
        let mut lines = Vec::new();
        let mut bytes = 0;
        // The newest line in the facts about each thread: a routine line of a thread with a
        // later line there says nothing that one doesn't (interim chatter costs no tokens).
        let mut newest: std::collections::HashMap<String, u64> = Default::default();
        for e in updates.into_iter().take(32) {
            if let EventBody::ConversationUpdate(p) = e.body {
                let about = p.about_thread.clone();
                let later = about
                    .as_ref()
                    .and_then(|t| newest.get(t))
                    .is_some_and(|seq| *seq > e.seq);
                if later && routine_of(&p.ext).is_some() {
                    continue;
                }
                let (mut fact, hint) = match twr_ext(&p.ext)
                    .and_then(|t| t.get("interrupted_notice"))
                    .and_then(Value::as_str)
                {
                    Some(notice) => {
                        let seq = twr_ext(&p.ext)
                            .and_then(|t| t.get("interrupted_notice_seq"))
                            .and_then(Value::as_u64)
                            .unwrap_or(e.seq);
                        (cut(notice, 2500), format!(" Full background notice: read_transcript thread={thread} seq={seq}."))
                    }
                    None => (cut(&p.text, 3000), String::new()),
                };
                let mut line = serde_json::to_string(&format!("{fact}{hint}"))?;
                while line.len() > 3500 {
                    fact = cut(&fact, fact.len() / 2);
                    line = serde_json::to_string(&format!("{fact}{hint}"))?;
                }
                if bytes + line.len() + 1 > 3900 {
                    more = true;
                    break;
                }
                bytes += line.len() + 1;
                lines.push((e.seq, line));
                if let Some(t) = about {
                    let seq = newest.entry(t).or_default();
                    *seq = (*seq).max(e.seq);
                }
            }
        }
        lines.sort_by_key(|(seq, _)| *seq);
        let mut lines = lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>();
        if more {
            lines.push(
                "\"More hub updates are available with subscribe_control or read_transcript.\""
                    .into(),
            );
        }
        // Open questions survive the recent-update watermark and burst cap. This is a
        // current-state snapshot, independent of historical updates and notice context.
        let questions = self
            .attention
            .ordered(&Default::default())
            .into_iter()
            .filter(|i| {
                i.reason == AttentionReason::Question
                    && self
                        .registry
                        .get(&i.thread)
                        .is_some_and(|v| !v.is_orchestrator())
            })
            .collect::<Vec<_>>();
        if questions.is_empty() && lines.is_empty() {
            return Ok(String::new());
        }
        let mut pending = vec![serde_json::to_string(&format!(
            "Current open worker questions: {}. These require user answers, not autonomous approval.",
            questions.len()
        ))?];
        let mut bytes = pending[0].len();
        for (n, item) in questions.iter().enumerate() {
            let line = serde_json::to_string(&json!({
                "status": "open_question",
                "attn_id": item.attn_id,
                "revision": item.revision,
                "thread_id": item.thread,
                "thread": self.registry.get(&item.thread).map(|v| &v.name),
                "summary": cut(&item.summary, 160),
                "details": "attention_list or read_transcript"
            }))?;
            if bytes + line.len() + 1 > 3500 {
                pending.push(serde_json::to_string(&format!(
                    "{} more open worker questions: use attention_list for their complete text and options.",
                    questions.len() - n
                ))?);
                break;
            }
            bytes += line.len() + 1;
            pending.push(line);
        }
        pending.extend(lines);
        Ok(pending.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Hub, HubOptions, LOCAL_BOX};
    use super::*;
    use twr_core::config::Config;

    /// A thread's routine lines go from the background facts when a later line about it is
    /// there: only the newest routine line of a thread with nothing after it stays, and no
    /// other line is left out.
    #[tokio::test]
    async fn background_facts_keep_only_a_threads_latest_routine_line() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        hub.append(
            "o1",
            None,
            Source::Hub,
            EventBody::ThreadCreated(ThreadCreated {
                name: "orchestrator".into(),
                box_name: LOCAL_BOX.into(),
                runtime: "fake".into(),
                ext: twr_ext_map(json!({"role": "orchestrator"})),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let say = |about: &str, text: &str, kind: Option<&str>| {
            hub.append(
                "o1",
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: text.into(),
                    about_thread: Some(about.into()),
                    ext: kind.and_then(|k| routine(None, k)),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        };
        say("api", "api yielded 1", Some(ROUTINE_INTERIM));
        say("api", "api background 1", Some(ROUTINE_BACKGROUND_DONE));
        say("web", "web yielded 1", Some(ROUTINE_INTERIM));
        say("api", "api failed 2", None);
        say("api", "api failed 3", None);
        say("api", "api yielded 4", Some(ROUTINE_INTERIM));
        say("db", "db woke", Some(ROUTINE_AUTO_WAKE));
        say("db", "db background", Some(ROUTINE_BACKGROUND_DONE));
        let facts = hub.inner.core().update_context("o1").unwrap();
        let lines: Vec<&str> = facts.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "\"web yielded 1\"",
                "\"api failed 2\"",
                "\"api failed 3\"",
                "\"api yielded 4\"",
                "\"db background\"",
            ],
            "{facts}"
        );
    }

    /// The hub's wake of a worker after its background work is mirrored into the
    /// orchestrator's conversation as a routine update; its choice not to wake it, which asks
    /// the user to send it a message, as no routine one (`routine: null`), whatever its words.
    #[tokio::test]
    async fn a_wake_the_hub_skipped_is_no_routine_update() {
        use super::super::wake::{AUTO_WAKE_KEY, AUTO_WAKE_SKIPPED_KEY};
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        for (id, name, twr) in [
            ("o1", "orchestrator", json!({"role": "orchestrator"})),
            ("w1", "api", json!({})),
        ] {
            hub.append(
                id,
                None,
                Source::Hub,
                EventBody::ThreadCreated(ThreadCreated {
                    name: name.into(),
                    box_name: LOCAL_BOX.into(),
                    runtime: "fake".into(),
                    ext: twr_ext_map(twr),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        // As `hub::wake` records them on the worker.
        for (text, key) in [
            (
                "api's background work finished and no turn started after it: twr woke it to \
                 continue (after its interim report of turn 1).",
                AUTO_WAKE_KEY,
            ),
            (
                "api's background work finished and no turn started after it; twr did not \
                 wake it (woken 5 times in a row already). Send it a message to continue.",
                AUTO_WAKE_SKIPPED_KEY,
            ),
        ] {
            hub.append(
                "w1",
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: text.into(),
                    about_thread: Some("w1".into()),
                    ext: twr_ext_map(json!({ key: {"turn": 1} })),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        let (_, evs) = hub.thread_events("o1", 0, 100).unwrap();
        let marks: Vec<Option<Value>> = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ConversationUpdate(p) => {
                    Some(twr_ext(&p.ext).and_then(|t| t.get(ROUTINE_KEY)).cloned())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            marks,
            [Some(json!(ROUTINE_AUTO_WAKE)), Some(Value::Null)],
            "{evs:#?}"
        );
    }

    /// The routine mark joins whatever `ext` had.
    #[test]
    fn the_routine_mark_keeps_the_ext() {
        let ext = twr_ext_map(json!({"background_tasks": 2}));
        let marked = routine(ext, ROUTINE_INTERIM);
        assert_eq!(
            serde_json::to_value(&marked).unwrap(),
            json!({"twr": {"background_tasks": 2, "routine": "interim"}})
        );
        assert_eq!(routine_of(&marked), Some(ROUTINE_INTERIM));
        let mut odd = JsonMap::new();
        odd.insert("twr".into(), json!("x"));
        odd.insert("other".into(), json!(1));
        let marked = routine(Some(odd), ROUTINE_AUTO_WAKE);
        assert_eq!(
            serde_json::to_value(&marked).unwrap(),
            json!({"twr": {"routine": "auto_wake"}, "other": 1})
        );
        // Marked no routine update: kept too, and no kind.
        let not = not_routine(twr_ext_map(json!({"background_tasks": 2})));
        assert_eq!(
            serde_json::to_value(&not).unwrap(),
            json!({"twr": {"background_tasks": 2, "routine": null}})
        );
        assert_eq!(routine_of(&not), None);
    }
}

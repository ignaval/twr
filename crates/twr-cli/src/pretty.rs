//! One-line human rendering of events (`--pretty`).

use std::collections::HashMap;

use serde_json::Value;
use twr_core::blob::truncate_utf8;
use twr_core::event::{Event, EventBody};
use twr_core::reduce::reported_model;
// `ext.twr.reason` of the `thread.stopped` a hub shutdown records.
use twr_hub::hub::HUB_SHUTDOWN as SHUTDOWN;

const TEXT_MAX: usize = 100;

/// `turn.started` `ext.twr` key of the handoff a turn delivered to a thread's new runtime
/// (`twr_hub`'s `hub::switch::HANDOFF_KEY`, which the hub does not export).
const RUNTIME_HANDOFF: &str = "runtime_handoff";

/// Collapses whitespace and truncates to [`TEXT_MAX`] bytes.
fn short(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= TEXT_MAX {
        return one_line;
    }
    format!("{}...", truncate_utf8(&one_line, TEXT_MAX - 3))
}

fn json_short(v: &Value) -> String {
    short(&serde_json::to_string(v).unwrap_or_default())
}

/// A short description of an event's payload.
pub fn summary(ev: &Event) -> String {
    match &ev.body {
        EventBody::ThreadCreated(p) => {
            // The requested model and effort (`ext.twr.model` / `effort`); none: the runtime
            // picks.
            let twr = |key: &str| {
                p.ext
                    .as_ref()
                    .and_then(|e| e.get("twr")?.get(key)?.as_str())
                    .unwrap_or("(runtime default)")
                    .to_string()
            };
            let mut s = format!(
                "{} project={} runtime={} box={} mode={} model={}",
                p.name,
                p.project.as_deref().unwrap_or("-"),
                p.runtime,
                p.box_name,
                p.mode,
                twr("model")
            );
            if p.ext
                .as_ref()
                .and_then(|e| e.get("twr")?.get("effort"))
                .is_some()
            {
                s.push_str(&format!(" effort={}", twr("effort")));
            }
            s
        }
        EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p) => {
            let mut s = format!("session={}", p.session_id.as_deref().unwrap_or("-"));
            // The model the runtime reports it runs.
            if let Some(m) = reported_model(p.ext.as_ref()) {
                s.push_str(&format!(" model={m}"));
            }
            s
        }
        EventBody::ThreadStopped(p) if twr_core::event::is_settings_restart(p) => {
            let reason = p
                .ext
                .as_ref()
                .and_then(|e| e.get("twr")?.get("reason")?.as_str());
            let session = p.session_id.as_deref().unwrap_or("-");
            // A runtime change stops this runtime for good: the next turn starts the new one.
            if reason == Some(twr_core::event::RUNTIME_RESTART_REASON) {
                return format!("session={session} (replaced by its new runtime)");
            }
            if reason == Some(twr_core::event::TRIM_RESTART_REASON) {
                return format!(
                    "session={session} (stopped for a trim; the next prompt resumes the session)"
                );
            }
            if reason == Some(twr_core::event::BOX_UPGRADE_REASON) {
                return format!(
                    "session={session} (restarting on the box's new twr build; the next prompt \
                     resumes the session)"
                );
            }
            let what = if reason == Some(twr_core::event::MODEL_RESTART_REASON) {
                "model"
            } else {
                "mode"
            };
            format!("session={session} (restarting in its new {what})")
        }
        EventBody::ThreadParked(p) | EventBody::ThreadStopped(p) => {
            format!("session={}", p.session_id.as_deref().unwrap_or("-"))
        }
        EventBody::ThreadModeChanged(p) => {
            let by = serde_json::to_value(p.by).unwrap_or_default();
            let by = by.as_str().unwrap_or("?");
            if p.previous == p.mode {
                format!("kept {} by {by}", p.mode)
            } else {
                format!("{} -> {} by {by}", p.previous, p.mode)
            }
        }
        EventBody::ThreadRenamed(p) => {
            let by = serde_json::to_value(p.by).unwrap_or_default();
            format!(
                "{} -> {} by {}",
                p.previous,
                p.name,
                by.as_str().unwrap_or("?")
            )
        }
        EventBody::ThreadModelChanged(p) => {
            let by = serde_json::to_value(p.by).unwrap_or_default();
            let by = by.as_str().unwrap_or("?");
            let shown = |v: &Option<String>| v.clone().unwrap_or_else(|| "(default)".into());
            let runtime = match &p.runtime {
                Some(r) => format!("runtime {} -> {r}, ", shown(&p.previous_runtime)),
                None => String::new(),
            };
            format!(
                "{runtime}model {} -> {} effort {} -> {} by {by}",
                shown(&p.previous_model),
                shown(&p.model),
                shown(&p.previous_effort),
                shown(&p.effort)
            )
        }
        EventBody::ThreadDeleted(p) => {
            let mut s = format!(
                "{} worktree={} branch={}",
                p.name,
                p.worktree.as_deref().unwrap_or("-"),
                p.branch.as_deref().unwrap_or("-")
            );
            if let Some(n) = p.dirty_files {
                s.push_str(&format!(" dirty_files={n}"));
            }
            if let Some(n) = p.branch_only_commits {
                s.push_str(&format!(" branch_only_commits={n}"));
            }
            if let Some(n) = p.head_only_commits {
                s.push_str(&format!(" head_only_commits={n}"));
            }
            if let Some(w) = &p.worktree_left {
                s.push_str(&format!(
                    " worktree_left={w} (part of it could not be moved)"
                ));
            }
            if !p.build_output_aside.is_empty() {
                s.push_str(&format!(
                    " build_output_aside={} (kept in the trash)",
                    p.build_output_aside.join(",")
                ));
            }
            s.push_str(&format!(" dropped_prompts={}", p.dropped_prompts));
            if let Some(item) = &p.trash {
                s.push_str(&format!(" trash={item}"));
                if let Some(b) = p.trash_bytes {
                    s.push_str(&format!(" ({})", twr_core::git::human_bytes(b)));
                }
            }
            s
        }
        EventBody::ThreadRestored(p) => {
            let mut s = format!(
                "{} worktree={} branch={} trash={}",
                p.name,
                p.worktree.as_deref().unwrap_or("-"),
                p.branch.as_deref().unwrap_or("-"),
                p.item
            );
            if let Some(old) = &p.previous_name {
                s.push_str(&format!(" (was {old})"));
            }
            s
        }
        EventBody::ThreadCrashed(p) => format!(
            "exit={} {}",
            p.exit_code.map_or("signal".to_string(), |c| c.to_string()),
            short(p.stderr_tail.lines().last().unwrap_or_default())
        ),
        EventBody::ThreadAttached(p) | EventBody::ThreadDetached(p) => {
            format!("client={}", p.client)
        }
        EventBody::TurnStarted(p) => {
            let from = serde_json::to_value(p.from).unwrap_or_default();
            // The first turn of a thread's new runtime carries the handoff: its size only (a
            // capped one is in the blob store, its size unknown here without --hydrate).
            let twr = p.ext.as_ref().and_then(|e| e.get("twr"));
            let handoff = match twr.and_then(|t| t.get(RUNTIME_HANDOFF)) {
                _ if twr.is_some_and(|t| t.get(format!("{RUNTIME_HANDOFF}_ref")).is_some()) => {
                    "(with handoff) ".to_string()
                }
                Some(Value::String(h)) => format!("(with handoff, {} bytes) ", h.len()),
                Some(_) => "(with handoff) ".to_string(),
                None => String::new(),
            };
            format!(
                "from={} {handoff}{}",
                from.as_str().unwrap_or("?"),
                short(&p.prompt)
            )
        }
        // A message the user took back (`message.withdraw`).
        EventBody::TurnWithdrawn(p) => format!("ref={} taken back", p.msg_ref),
        // The user sent the next queued message now (`message.send_now`).
        EventBody::TurnInterrupted(p) => format!("interrupted to send ref={} now", p.msg_ref),
        EventBody::PromptWithdrawn(p) => {
            format!(
                "ref={} taken back before it ran: {}",
                p.msg_ref,
                short(&p.prompt)
            )
        }
        EventBody::TaskUpdated(p) => format!(
            "task={} {} {}",
            p.task_id,
            if p.running { "running" } else { "finished" },
            short(&p.description)
        ),
        EventBody::TurnEnded(p) => {
            let result = serde_json::to_value(p.result).unwrap_or_default();
            let mut s = format!("result={}", result.as_str().unwrap_or("?"));
            if let Some(ms) = p.duration_ms {
                s.push_str(&format!(" {ms}ms"));
            }
            s
        }
        EventBody::AgentText(p) => {
            if p.is_final {
                format!("» {}", short(&p.text))
            } else {
                short(&p.text)
            }
        }
        EventBody::AgentToolCall(p) => format!(
            "{}{} {}",
            p.tool,
            if p.risky { " (risky)" } else { "" },
            short(&p.input_summary)
        ),
        EventBody::AgentToolResult(p) => format!(
            "{} {}",
            if p.ok { "ok" } else { "failed" },
            short(&p.output_summary)
        ),
        EventBody::AgentThinking(p) => short(&p.text),
        EventBody::AttentionRaised(p) => {
            let reason = serde_json::to_value(p.reason).unwrap_or_default();
            format!(
                "{} {} [{}] id={}",
                reason.as_str().unwrap_or("?"),
                short(&p.summary),
                p.options.join("/"),
                p.attn_id
            )
        }
        EventBody::AttentionAnnotated(p) => format!("id={} {}", p.attn_id, short(&p.note)),
        EventBody::AttentionResolved(p) => {
            let by = serde_json::to_value(p.by).unwrap_or_default();
            format!(
                "id={} by={} answer={}",
                p.attn_id,
                by.as_str().unwrap_or("?"),
                p.answer.as_ref().map_or("-", |a| a.decision.as_str())
            )
        }
        EventBody::DecisionMade(p) => {
            let site = serde_json::to_value(p.site).unwrap_or_default();
            let provider = serde_json::to_value(p.provider).unwrap_or_default();
            format!(
                "{} by {}: {} ({:.2})",
                site.as_str().unwrap_or("?"),
                provider.as_str().unwrap_or("?"),
                p.chosen,
                p.confidence
            )
        }
        EventBody::ReportReady(p) => {
            let grade = serde_json::to_value(p.grade).unwrap_or_default();
            format!("{} {}", grade.as_str().unwrap_or("?"), p.summary)
        }
        EventBody::AccountUsage(p) => {
            let (label, _) = twr_core::usage::runtime_labels(&p.runtime);
            let account = p
                .account
                .as_ref()
                .map_or(String::new(), |a| format!(":{a}"));
            let windows: Vec<String> = p
                .windows
                .iter()
                .map(|w| format!("{} {}%", w.label(), w.left_pct()))
                .collect();
            format!(
                "{label}{account}: {} left ({})",
                windows.join(" · "),
                p.source
            )
        }
        EventBody::ContextCompacted(p) => p.line(),
        other => json_short(&other.data().unwrap_or_default()),
    }
}

/// Renders one line: `[gseq] time thread kind turn summary`.
pub fn line(gseq: Option<u64>, ev: &Event, names: &HashMap<String, String>) -> String {
    let name = name_of(&ev.thread, names);
    let turn = ev.turn.map_or(String::new(), |t| format!("#{t} "));
    let prefix = gseq.map_or(String::new(), |g| format!("{g:>5} "));
    let l = format!(
        "{prefix}{} {name:<12} {:<18} {turn}{}",
        ev.ts.format("%H:%M:%S"),
        ev.kind(),
        summary(ev)
    );
    crate::term::clean(&l).into_owned()
}

/// Why a hub-appended `thread.stopped` was recorded (`ext.twr.reason`).
fn stop_reason(ev: &Event) -> Option<&str> {
    match &ev.body {
        EventBody::ThreadStopped(p) => p.ext.as_ref()?.get("twr")?.get("reason")?.as_str(),
        _ => None,
    }
}

/// What [`Pretty`] holds back until it knows the line is complete.
enum Pending {
    /// Consecutive streamed `agent.text` deltas of one turn of one thread: one line.
    Text {
        gseq: Option<u64>,
        first: Event,
        text: String,
    },
    /// Consecutive `thread.stopped` the hub recorded as it shut down: one summary line.
    Shutdown {
        gseq: Option<u64>,
        first: Event,
        threads: Vec<String>,
    },
}

/// `--pretty` rendering with lines merged: the streamed deltas of an agent's reply
/// (`agent.text`, not final) of one turn become one line, and the `thread.stopped` a hub
/// shutdown records for every thread one dimmed summary. Feed events with [`Pretty::push`];
/// [`Pretty::flush`] prints what is held back (at the end, or on a pause while following).
pub struct Pretty {
    pending: Option<Pending>,
    /// Dim the shutdown summary (a terminal).
    dim: bool,
}

impl Pretty {
    pub fn new(dim: bool) -> Self {
        Pretty { pending: None, dim }
    }

    /// Whether a line is held back.
    pub fn holding(&self) -> bool {
        self.pending.is_some()
    }

    /// The lines complete once `ev` arrived (`ev`'s own, unless it is held back).
    pub fn push(
        &mut self,
        gseq: Option<u64>,
        ev: &Event,
        names: &HashMap<String, String>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        match (&mut self.pending, &ev.body) {
            (Some(Pending::Text { first, text, .. }), EventBody::AgentText(t))
                if !t.is_final && first.thread == ev.thread && first.turn == ev.turn =>
            {
                text.push_str(&t.text);
                return out;
            }
            (Some(Pending::Shutdown { threads, .. }), _) if stop_reason(ev) == Some(SHUTDOWN) => {
                threads.push(name_of(&ev.thread, names));
                return out;
            }
            _ => {}
        }
        out.extend(self.flush(names));
        self.pending = match &ev.body {
            EventBody::AgentText(t) if !t.is_final => Some(Pending::Text {
                gseq,
                first: ev.clone(),
                text: t.text.clone(),
            }),
            _ if stop_reason(ev) == Some(SHUTDOWN) => Some(Pending::Shutdown {
                gseq,
                first: ev.clone(),
                threads: vec![name_of(&ev.thread, names)],
            }),
            _ => {
                out.push(line(gseq, ev, names));
                None
            }
        };
        out
    }

    /// The held-back line, if any.
    pub fn flush(&mut self, names: &HashMap<String, String>) -> Option<String> {
        Some(match self.pending.take()? {
            Pending::Text { gseq, first, text } => {
                let mut ev = first;
                if let EventBody::AgentText(t) = &mut ev.body {
                    t.text = text;
                }
                line(gseq, &ev, names)
            }
            Pending::Shutdown {
                gseq,
                first,
                threads,
            } => {
                let prefix = gseq.map_or(String::new(), |g| format!("{g:>5} "));
                let what = if threads.len() == 1 {
                    format!("stopped {}", threads[0])
                } else {
                    format!("stopped {} threads: {}", threads.len(), threads.join(", "))
                };
                let l = format!(
                    "{prefix}{} {:<12} {:<18} {}",
                    first.ts.format("%H:%M:%S"),
                    "hub",
                    "hub shutdown",
                    short(&what)
                );
                let l = crate::term::clean(&l).into_owned();
                if self.dim {
                    format!("\x1b[2m{l}\x1b[0m")
                } else {
                    l
                }
            }
        })
    }
}

fn name_of(thread: &str, names: &HashMap<String, String>) -> String {
    names
        .get(thread)
        .cloned()
        .unwrap_or_else(|| thread.chars().take(10).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::event::{AgentText, Source};

    #[test]
    fn usage_reads_as_its_windows() {
        let ev = Event::new(
            twr_core::HUB_THREAD,
            1,
            None,
            Source::Hub,
            EventBody::AccountUsage(twr_core::event::AccountUsage {
                runtime: "claude-code".into(),
                windows: vec![
                    twr_core::usage::UsageWindow {
                        window: "five_hour".into(),
                        used_pct: 22.4,
                        ..Default::default()
                    },
                    twr_core::usage::UsageWindow {
                        window: "seven_day".into(),
                        used_pct: 44.0,
                        ..Default::default()
                    },
                ],
                source: "read".into(),
                ..Default::default()
            }),
        );
        assert_eq!(summary(&ev), "Claude: 5h 78% · wk 56% left (read)");
    }

    /// A stop the hub makes to restart a runtime says why, not that the thread stopped.
    #[test]
    fn settings_restarts_say_why() {
        use twr_core::event::*;
        let stop = |reason: Option<&str>| {
            let ext = reason.and_then(|r| {
                serde_json::json!({"twr": {"reason": r}})
                    .as_object()
                    .cloned()
            });
            summary(&Event::new(
                "t1",
                4,
                None,
                Source::Hub,
                EventBody::ThreadStopped(SessionPayload {
                    session_id: Some("s1".into()),
                    ext,
                    ..Default::default()
                }),
            ))
        };
        assert_eq!(
            stop(Some(MODE_RESTART_REASON)),
            "session=s1 (restarting in its new mode)"
        );
        assert_eq!(
            stop(Some(MODEL_RESTART_REASON)),
            "session=s1 (restarting in its new model)"
        );
        assert_eq!(
            stop(Some(RUNTIME_RESTART_REASON)),
            "session=s1 (replaced by its new runtime)"
        );
        assert_eq!(
            stop(Some(TRIM_RESTART_REASON)),
            "session=s1 (stopped for a trim; the next prompt resumes the session)"
        );
        assert_eq!(
            stop(Some(BOX_UPGRADE_REASON)),
            "session=s1 (restarting on the box's new twr build; the next prompt resumes the \
             session)"
        );
        assert_eq!(stop(Some("hub_shutdown")), "session=s1");
        assert_eq!(stop(None), "session=s1");
    }

    #[test]
    fn withdrawals_say_the_ref() {
        let ev = |body| Event::new("t1", 3, Some(2), Source::User("tui".into()), body);
        let turn = ev(EventBody::TurnWithdrawn(twr_core::event::TurnWithdrawn {
            msg_ref: 7,
            turn: 2,
            ..Default::default()
        }));
        assert_eq!(summary(&turn), "ref=7 taken back");
        let queued = ev(EventBody::PromptWithdrawn(
            twr_core::event::PromptWithdrawn {
                msg_ref: 8,
                prompt: "deploy\nnow".into(),
                ..Default::default()
            },
        ));
        assert_eq!(
            summary(&queued),
            "ref=8 taken back before it ran: deploy now"
        );
    }

    #[test]
    fn renders_a_line() {
        let ev = Event::new(
            "01ABCDEFGHJK",
            3,
            Some(2),
            Source::Hub,
            EventBody::AgentText(AgentText {
                text: "hello\n  world".into(),
                is_final: true,
                ..Default::default()
            }),
        );
        let mut names = HashMap::new();
        let l = line(Some(7), &ev, &names);
        assert!(l.starts_with("    7 "), "{l}");
        assert!(l.contains("01ABCDEFGH "), "{l}");
        assert!(l.ends_with("agent.text         #2 » hello world"), "{l}");
        names.insert("01ABCDEFGHJK".to_string(), "api".to_string());
        assert!(line(None, &ev, &names).contains(" api "));
        assert!(short(&"x".repeat(500)).len() <= TEXT_MAX);
    }

    /// `thread.created` names the requested model (or the runtime's default), `thread.started`
    /// the one the runtime reports.
    #[test]
    fn thread_lines_name_the_model() {
        use twr_core::event::{SessionPayload, ThreadCreated};
        let created = |model: Option<&str>| {
            let mut p = ThreadCreated {
                name: "api".into(),
                runtime: "claude-code".into(),
                box_name: "local".into(),
                mode: "manual".into(),
                ..Default::default()
            };
            if let Some(m) = model {
                p.ext = serde_json::json!({"twr": {"model": m}})
                    .as_object()
                    .cloned();
            }
            Event::new("t1", 1, None, Source::Hub, EventBody::ThreadCreated(p))
        };
        assert!(
            summary(&created(Some("opus"))).ends_with(" mode=manual model=opus"),
            "{}",
            summary(&created(Some("opus")))
        );
        assert!(summary(&created(None)).ends_with(" model=(runtime default)"));
        let started = |ext: serde_json::Value| {
            let body = EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s1".into()),
                ext: ext.as_object().cloned(),
                ..Default::default()
            });
            Event::new("t1", 2, None, Source::Adapter("claude-code".into()), body)
        };
        let ext = serde_json::json!({"twr": {"pid": 1},
            "claude": {"model": "claude-sonnet-4-5-20250929"}});
        assert_eq!(
            summary(&started(ext)),
            "session=s1 model=claude-sonnet-4-5-20250929"
        );
        assert_eq!(
            summary(&started(serde_json::json!({"twr": {"pid": 1}}))),
            "session=s1"
        );
    }

    fn delta(thread: &str, seq: u64, turn: u64, t: &str) -> Event {
        Event::new(
            thread,
            seq,
            Some(turn),
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: t.into(),
                ..Default::default()
            }),
        )
    }

    fn stopped(thread: &str, reason: Option<&str>) -> Event {
        let ext = reason.map(|r| {
            let mut m = twr_core::event::JsonMap::new();
            m.insert("twr".into(), serde_json::json!({ "reason": r }));
            m
        });
        Event::new(
            thread,
            9,
            None,
            Source::Hub,
            EventBody::ThreadStopped(twr_core::event::SessionPayload {
                ext,
                ..Default::default()
            }),
        )
    }

    /// Streamed deltas of one turn print as one line; a final text, another turn or another
    /// thread ends it; `flush` prints what is held back.
    #[test]
    fn deltas_of_a_turn_are_one_line() {
        let names = HashMap::from([("t1".to_string(), "api".to_string())]);
        let mut p = Pretty::new(false);
        let mut lines = Vec::new();
        for (i, w) in ["Hello", ", ", "world", "!"].iter().enumerate() {
            lines.extend(p.push(None, &delta("t1", i as u64 + 1, 1, w), &names));
        }
        assert!(lines.is_empty() && p.holding());
        let fin = Event::new(
            "t1",
            5,
            Some(1),
            Source::Adapter("fake".into()),
            EventBody::AgentText(AgentText {
                text: "Hello, world!".into(),
                is_final: true,
                ..Default::default()
            }),
        );
        lines.extend(p.push(None, &fin, &names));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].ends_with("agent.text         #1 Hello, world!"),
            "{lines:?}"
        );
        assert!(lines[1].ends_with("#1 » Hello, world!"), "{lines:?}");
        assert!(!p.holding());
        // Another turn (or thread) starts a new line.
        let mut lines = p.push(None, &delta("t1", 6, 2, "a"), &names);
        lines.extend(p.push(None, &delta("t2", 1, 2, "b"), &names));
        lines.extend(p.flush(&names));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].ends_with("#2 a"));
        assert!(p.flush(&names).is_none());
    }

    /// The `thread.stopped` of a hub shutdown are one (dimmed) summary line; other stops print
    /// as usual.
    #[test]
    fn shutdown_stops_are_one_summary_line() {
        let names = HashMap::from([
            ("t1".to_string(), "api".to_string()),
            ("t2".to_string(), "web".to_string()),
        ]);
        let mut p = Pretty::new(true);
        let mut lines = p.push(Some(7), &stopped("t1", Some(SHUTDOWN)), &names);
        lines.extend(p.push(Some(8), &stopped("t2", Some(SHUTDOWN)), &names));
        assert!(lines.is_empty());
        lines.extend(p.push(Some(9), &stopped("t1", None), &names));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("\x1b[2m    7 "), "{lines:?}");
        assert!(
            lines[0].contains("stopped 2 threads: api, web"),
            "{lines:?}"
        );
        assert!(lines[1].contains("thread.stopped"), "{lines:?}");
        let mut p = Pretty::new(false);
        assert!(p
            .push(None, &stopped("t1", Some(SHUTDOWN)), &names)
            .is_empty());
        let l = p.flush(&names).unwrap();
        assert!(l.ends_with("hub shutdown       stopped api"), "{l}");
    }
}

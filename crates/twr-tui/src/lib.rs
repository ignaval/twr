//! twr terminal UI: the stack, grid and decision audit views over the hub control stream.
//!
//! - [`app`]: pure state (registry, attention stack, the main pane's transcript, view state)
//!   and key, mouse and paste handling that returns [`app::Action`]s
//! - [`edit`]: the input line's editor (one line of text, wrapped in a box that grows)
//! - [`icon`]: the working icon (the breathing cursor of `twr_`) and the clock that moves it
//! - [`notify`]: terminal notifications (`[tui.notify]`) for what needs the user
//! - [`select`]: mouse selection inside one pane, and copying it
//! - [`ui`]: pure rendering of that state
//! - [`run`]: the thin IO loop that connects to the hub, feeds streams into the state, executes
//!   actions and redraws
//!
//! The TUI is only a view: reports, routing and attention all live in the hub, so nothing here is
//! required for the report path to work without a screen.

pub mod app;
#[cfg(test)]
mod bench;
pub mod edit;
pub mod icon;
pub mod keys;
pub mod md;
pub mod notify;
pub mod order;
pub mod queue;
pub mod select;
pub mod shell;
pub mod slash;
pub mod suggest;
pub mod ui;
pub mod workspace;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event as TermEvent, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use futures::{FutureExt, Stream, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use twr_core::blob::{has_capped, hydrate_deep, BlobFetch};
use twr_core::client::{Client, HubClosed};
use twr_core::event::EventBody;
use twr_core::proto::StreamItem;

use crate::app::{Action, AppState, Picker};

type ItemStream = Pin<Box<dyn Stream<Item = anyhow::Result<StreamItem>> + Send>>;

/// The request the input line sends: `say` with `to: "orchestrator"` (the hub skips its
/// routing and hands the line to the orchestrator; `seen_gseq`, the control position on
/// screen, bounds what the orchestrator can relay an answer to), or, in a dive, `thread.send`
/// to the thread dived into.
pub fn submit_request(action: &Action) -> Option<(&'static str, Value)> {
    match action {
        Action::Say { text, seen_gseq } => Some((
            "say",
            json!({ "message": text, "seen_gseq": seen_gseq, "to": "orchestrator" }),
        )),
        Action::Send { thread, text } => {
            Some(("thread.send", json!({"thread": thread, "prompt": text})))
        }
        _ => None,
    }
}

/// The `project` of a `/new`: a path (it has a `/`, or is `.` / `..` / `~`) is made absolute
/// here (the hub resolves paths in its own working directory; `~` is the home directory);
/// anything else is sent as is (a configured project name).
pub fn project_param(project: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let path = match (project.strip_prefix('~'), &home) {
        (Some(rest), Some(h)) if rest.is_empty() || rest.starts_with('/') => {
            h.join(rest.trim_start_matches('/'))
        }
        _ if project.contains('/') || project == "." || project == ".." => PathBuf::from(project),
        _ => return project.to_string(),
    };
    match std::fs::canonicalize(&path) {
        Ok(abs) => abs.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// Whether a `say` or `thread.send` result left the prompt queued (its thread is busy), and
/// why it could not start, if it could not.
fn queued_note(result: &Value) -> Option<String> {
    if let Some(e) = result["dispatch_error"].as_str() {
        return Some(format!("queued, but it could not start: {e}"));
    }
    (result["turn"].is_null() && result["pending"].as_u64().is_some_and(|n| n > 0))
        .then(|| "queued: it is busy and reads this next".to_string())
}

/// Whether `result` is a `trash.empty` dry run's (what it would purge, nothing purged).
fn is_dry_run(result: &Value) -> bool {
    result["dry_run"].as_bool() == Some(true)
}

/// A status-bar line for what `say`, `thread.send`, `thread.set_mode`, `thread.set_model`,
/// `thread.new`, `thread.stop`, `thread.park`, `thread.resume`, `thread.delete`,
/// `trash.empty`, `trash.restore` or `orchestrator.use` did (`None` for other methods, and for a `thread.send` that started a
/// turn: the transcript shows it).
pub fn result_status(method: &str, result: &Value) -> Option<String> {
    // The hub's advisory on a call that makes, changes, restarts or starts a thread's runtime
    // (its box reports the runtime's CLI missing): said after what was done, alone when
    // nothing else is said (a `thread.send` that started a turn).
    let warning = matches!(
        method,
        "thread.new" | "thread.set_model" | "thread.set_mode" | "thread.send"
    )
    .then(|| result["warning"].as_str())
    .flatten()
    .filter(|w| !w.trim().is_empty());
    match (method_status(method, result), warning) {
        (Some(status), Some(w)) => Some(format!("{status}; warning: {w}")),
        (Some(status), None) => Some(status),
        (None, Some(w)) => Some(format!("warning: {w}")),
        (None, None) => None,
    }
}

/// [`result_status`] before the hub's advisory.
fn method_status(method: &str, result: &Value) -> Option<String> {
    match method {
        "say" => {}
        "thread.send" => return queued_note(result),
        "thread.set_mode" => {
            let name = result["thread_name"]
                .as_str()
                .or(result["name"].as_str())
                .unwrap_or("the thread");
            let mode = result["mode"].as_str().unwrap_or("?");
            if result["changed"].as_bool() == Some(false) {
                return Some(format!("{name} is already in mode {mode}"));
            }
            let mut msg = format!("{name} is now in mode {mode}");
            msg.push_str(match result["applies"].as_str() {
                Some("now") => "; its runtime restarts in it now",
                Some("after_turn") => "; its runtime restarts in it after the current turn",
                Some("next_start") => "; from its next start",
                Some("never") => "; its runtime applies no mode",
                _ => "",
            });
            if result["stops_background_tasks"].as_bool() == Some(true) {
                msg.push_str("; its background tasks end with it");
            }
            if let Some(e) = result["dispatch_error"].as_str() {
                msg.push_str(&format!("; restarting failed: {e}"));
            }
            return Some(msg);
        }
        "thread.set_model" => {
            let name = result["thread_name"].as_str().unwrap_or("the thread");
            let value = |k: &str| result[k].as_str().unwrap_or("default").to_string();
            let now = format!("model {}, effort {}", value("model"), value("effort"));
            if result["changed"].as_bool() == Some(false) {
                return Some(format!("{name} already has {now}"));
            }
            // A move to another runtime: the hub's message says when its session starts.
            if result["runtime_changed"].as_bool() == Some(true) {
                let runtime = result["runtime"].as_str().unwrap_or("?");
                let mut msg = result["message"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{name} moves to {runtime} with {now}"));
                if result["applies"].as_str() == Some("after_turn") {
                    msg.push_str("; the current turn finishes on the previous runtime first");
                }
                if result["stops_background_tasks"].as_bool() == Some(true) {
                    msg.push_str("; its background tasks end with it");
                }
                if let Some(e) = result["dispatch_error"].as_str() {
                    msg.push_str(&format!("; dispatching after the move failed: {e}"));
                }
                return Some(msg);
            }
            let mut msg = format!("{name} now has {now}");
            msg.push_str(match result["applies"].as_str() {
                Some("now") => "; its runtime restarts with them now",
                Some("after_turn") => "; its runtime restarts with them after the current turn",
                Some("next_start") => "; from its next start",
                _ => "",
            });
            if let Some(e) = result["dispatch_error"].as_str() {
                msg.push_str(&format!("; restarting failed: {e}"));
            }
            return Some(msg);
        }
        "thread.rename" => {
            let name = result["thread_name"].as_str().unwrap_or("?");
            if result["changed"].as_bool() == Some(false) {
                return Some(format!("{name} already has that name"));
            }
            return Some(
                result["message"]
                    .as_str()
                    .map_or_else(|| format!("renamed to {name}"), str::to_string),
            );
        }
        "thread.new" => {
            let found = result["project_found"]["repo"]
                .as_str()
                .map(|repo| format!(" in {repo}"))
                .unwrap_or_default();
            return Some(format!(
                "created {} (runtime {}){found}",
                result["name"].as_str().unwrap_or("?"),
                result["runtime"].as_str().unwrap_or("?")
            ));
        }
        "thread.stop" => {
            let mut msg = if result["box_offline"].as_bool() == Some(true) {
                "stopping once its box's runner reconnects (box offline)".to_string()
            } else if result["stopping"].as_bool() == Some(true) {
                "stopping".to_string()
            } else {
                "stopped".to_string()
            };
            if let Some(n) = result["dropped_prompts"].as_u64().filter(|n| *n > 0) {
                msg.push_str(&format!("; dropped {n} queued prompt(s)"));
            }
            return Some(msg);
        }
        "thread.park" => {
            let mut msg = "parked".to_string();
            if result["kept_dirty"].as_bool() == Some(true) {
                msg.push_str("; worktree kept (uncommitted changes)");
            }
            return Some(msg);
        }
        "thread.resume" => return Some("resumed".into()),
        "thread.trim" => {
            let name = result["thread_name"].as_str().unwrap_or("?");
            let deleted = result["deleted"].as_array().map_or(0, Vec::len);
            let mut msg = if deleted == 0 {
                format!("{name}: no build output to trim")
            } else {
                format!(
                    "trimmed {name}: {} freed",
                    twr_core::git::human_bytes(result["freed"].as_u64().unwrap_or(0))
                )
            };
            // What did not go as it should (an entry that stays aside, and where).
            for n in result["notes"].as_array().into_iter().flatten() {
                if let Some(n) = n.as_str() {
                    msg.push_str("; ");
                    msg.push_str(n);
                }
            }
            return Some(msg);
        }
        // `Delete` to a card a thread of which was deleted and restored since: nothing went.
        "attention.respond" => {
            let changed: Vec<&str> = result["data"]["ext"]["twr"]["changed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if changed.is_empty() {
                return None;
            }
            return Some(format!("nothing deleted: {}", changed.join("; ")));
        }
        // The hub says what was purged or restored (and how to resume a restored thread).
        // A dry run opens the question instead (`AppState::apply_result`).
        "trash.empty" if is_dry_run(result) => return None,
        "trash.empty" | "trash.restore" => {
            return result["message"].as_str().map(str::to_string);
        }
        // Nothing is deleted yet: the card waits for the user's answer.
        "thread.delete" => {
            let names: Vec<&str> = result["threads"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|t| t["thread_name"].as_str())
                .collect();
            let names = match names.as_slice() {
                [] => result["thread_name"]
                    .as_str()
                    .unwrap_or("the thread")
                    .to_string(),
                [one] => one.to_string(),
                many => format!(
                    "{} and {}",
                    many[..many.len() - 1].join(", "),
                    many[many.len() - 1]
                ),
            };
            let already = if result["existing"].as_bool() == Some(true) {
                " already"
            } else {
                ""
            };
            return Some(format!(
                "the delete card for {names} is{already} waiting for you; nothing is deleted \
                 until you answer Delete on it"
            ));
        }
        "orchestrator.reset" => {
            let mut msg = match result["start_error"].as_str() {
                Some(e) => {
                    format!("conversation cleared; the new orchestrator could not start yet: {e}")
                }
                None => "conversation cleared; fresh session, no handoff (history kept)".into(),
            };
            if let Some(w) = result["warning"].as_str() {
                msg.push_str(&format!("; warning: {w}"));
            }
            return Some(msg);
        }
        "orchestrator.use" => {
            let h = &result["handoff"];
            let handoff = if !h.is_object() {
                String::new()
            } else if h["summary"].as_bool() == Some(true) {
                "; handed over with the previous one's summary".into()
            } else {
                format!(
                    "; handed over without a summary ({})",
                    h["summary_note"].as_str().unwrap_or("skipped")
                )
            };
            let mut msg = format!(
                "the orchestrator is now {} (a new session{handoff})",
                result["runtime"].as_str().unwrap_or("?")
            );
            if let Some(w) = result["warning"].as_str() {
                msg.push_str(&format!("; warning: {w}"));
            }
            if let Some(e) = result["start_error"].as_str() {
                msg.push_str(&format!("; it could not start yet: {e}"));
            }
            return Some(msg);
        }
        _ => return None,
    }
    let name = result["thread_name"]
        .as_str()
        .or(result["thread"].as_str())
        .unwrap_or("?");
    Some(match result["action"].as_str().unwrap_or_default() {
        "send" => format!("sent to {name}"),
        "respond" => format!(
            "answered {name}: {}",
            result["answer"].as_str().unwrap_or("?")
        ),
        "command" => format!(
            "applied: {}",
            result["applied"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; "))
                .unwrap_or_default()
        ),
        "new_thread" => format!("created {name}"),
        "delegate" => match queued_note(result) {
            Some(q) => format!("to the orchestrator: {q}"),
            None => "sent to the orchestrator".to_string(),
        },
        "ask" => "which thread? answer on the twr asks card".to_string(),
        other => other.to_string(),
    })
}

/// Whether the status-bar line for `method`'s `result` ([`result_status`]) is news only for a
/// moment ([`AppState::set_status`]): a message `say` sent to a thread or handed to the
/// orchestrator, queued or not. Not when it could not start, nor an answer, a created thread
/// or a "which thread?" question.
pub fn transient_status(method: &str, result: &Value) -> bool {
    method == "say"
        && matches!(
            result["action"].as_str(),
            Some("send" | "delegate" | "command")
        )
        && result["dispatch_error"].is_null()
}

/// The status-bar line for a failed hub call: `method: error`, except a message to an
/// orchestrator that cannot run, which says what to do from the TUI.
pub fn error_status(method: &str, error: &str) -> String {
    if method == "say" && error.contains("no orchestrator is available") {
        return "no orchestrator can run: nothing was sent. Switch it with o or /orch".into();
    }
    format!("{method}: {error}")
}

/// The hub request for an [`Action`], or `None` for actions handled locally.
pub fn action_request(action: &Action) -> Option<(&'static str, Value)> {
    match action {
        Action::Quit | Action::Copy { .. } | Action::Mouse(_) | Action::Workspace { .. } => None,
        // The revision of the card on screen: an item raised again since (it may ask
        // something else now) is not answered by this key (`conflict`).
        Action::Respond {
            attn_id,
            revision,
            decision,
        } => {
            let mut p =
                json!({"attn_id": attn_id, "answer": {"decision": decision, "message": null}});
            if *revision > 0 {
                p["revision"] = json!(revision);
            }
            Some(("attention.respond", p))
        }
        Action::Say { .. } | Action::Send { .. } => submit_request(action),
        Action::Withdraw { thread, msg_ref } => Some((
            "message.withdraw",
            json!({"thread": thread, "ref": msg_ref}),
        )),
        Action::SendNow { thread, msg_ref } => Some((
            "message.send_now",
            json!({"thread": thread, "ref": msg_ref}),
        )),
        Action::ShellRun {
            thread,
            command,
            cwd,
            share,
        } => {
            let mut p = json!({ "command": command, "share": share });
            if let Some(t) = thread {
                p["thread"] = json!(t);
            }
            if let Some(c) = cwd {
                p["cwd"] = json!(c);
            }
            Some(("shell.run", p))
        }
        Action::ShellCd { cwd, dir } => Some(("shell.cd", json!({ "cwd": cwd, "dir": dir }))),
        Action::JobCancel { job } => Some(("job.cancel", json!({ "job": job }))),
        Action::SetMode { thread, mode } => {
            Some(("thread.set_mode", json!({"thread": thread, "mode": mode})))
        }
        Action::Rename { thread, name } => {
            Some(("thread.rename", json!({"thread": thread, "name": name})))
        }
        // An omitted field keeps the thread's.
        Action::SetModel {
            thread,
            model,
            effort,
            runtime,
        } => {
            let mut p = json!({ "thread": thread });
            if let Some(r) = runtime {
                p["runtime"] = json!(r);
            }
            if let Some(m) = model {
                p["model"] = json!(m);
            }
            if let Some(e) = effort {
                p["effort"] = json!(e);
            }
            Some(("thread.set_model", p))
        }
        Action::Dismiss { attn_id, revision } => {
            let mut p = json!({ "attn_id": attn_id });
            if *revision > 0 {
                p["revision"] = json!(revision);
            }
            Some(("attention.dismiss", p))
        }
        Action::Stop { thread } => Some(("thread.stop", json!({"thread": thread}))),
        Action::Delete { threads } => Some(("thread.delete", json!({"threads": threads}))),
        Action::TrashList => Some(("trash.list", Value::Null)),
        Action::TrashChoose { items, all } => Some((
            "trash.empty",
            json!({"items": items, "all": all, "dry_run": true}),
        )),
        // Exactly the ids the question listed: never `all` or a name again.
        Action::TrashEmpty { items } => {
            Some(("trash.empty", json!({"items": items, "all": false})))
        }
        Action::TrashRestore { item, as_name } => {
            let mut p = json!({ "item": item });
            if let Some(n) = as_name {
                p["as"] = json!(n);
            }
            Some(("trash.restore", p))
        }
        Action::Park { thread } => Some(("thread.park", json!({"thread": thread}))),
        Action::Resume { thread } => Some(("thread.resume", json!({"thread": thread}))),
        Action::Trim { thread } => Some(("thread.trim", json!({"thread": thread}))),
        Action::ResetOrchestrator => Some(("orchestrator.reset", json!({}))),
        Action::UseOrchestrator {
            runtime,
            model,
            summary,
        } => {
            let mut p = json!({ "runtime": runtime });
            if let Some(m) = model {
                p["model"] = json!(m);
            }
            if !summary {
                p["summary"] = json!(false);
            }
            Some(("orchestrator.use", p))
        }
        Action::NewThread {
            project,
            runtime,
            name,
        } => {
            let mut p = json!({ "project": project_param(project) });
            if let Some(r) = runtime {
                p["runtime"] = json!(r);
            }
            if let Some(n) = name {
                p["name"] = json!(n);
            }
            Some(("thread.new", p))
        }
    }
}

/// Restores the terminal when dropped (normal exit and error paths): mouse capture, bracketed
/// paste, focus reporting and the keyboard protocol off, then ratatui's restore. Panics go
/// through the hook [`run`] installs.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        release_terminal_modes();
        ratatui::restore();
    }
}

/// Whether [`enhance_keyboard`] pushed the keyboard protocol's flags, for
/// [`release_terminal_modes`] to pop them once.
static KEYBOARD_PUSHED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether [`enhance_keyboard`] asked tmux for modifyOtherKeys, for
/// [`release_terminal_modes`] to turn it off once.
static MODIFY_OTHER_KEYS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Asks the terminal to report keys unambiguously (the kitty keyboard protocol's first flag),
/// when it says it supports that: Shift-Enter then arrives apart from Enter, and breaks the
/// input line instead of sending it. Asked before the input thread starts reading, as the
/// answer comes in on the same input. Elsewhere Shift-Enter is a plain Enter (Alt-Enter and
/// Ctrl-J break the line there), but for a tmux that sends it in a form crossterm reads: it
/// is asked for modifyOtherKeys ([`keys`]). Returns what that leaves the TUI knowing.
fn enhance_keyboard() -> keys::Keys {
    use std::io::Write;
    use std::sync::atomic::Ordering;
    if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        && crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok()
    {
        KEYBOARD_PUSHED.store(true, Ordering::SeqCst);
        return keys::Keys {
            shift_enter: true,
            notice: None,
        };
    }
    let mut notice = None;
    if std::env::var_os("TMUX").is_some_and(|t| !t.is_empty()) {
        let extended = keys::tmux_option("extended-keys");
        let format = keys::tmux_option("extended-keys-format");
        match keys::tmux_plan(extended.as_ref(), format.as_ref()) {
            keys::TmuxPlan::Request => {
                let mut out = std::io::stdout();
                if out
                    .write_all(keys::MODIFY_OTHER_KEYS_ON.as_bytes())
                    .and_then(|()| out.flush())
                    .is_ok()
                {
                    MODIFY_OTHER_KEYS.store(true, Ordering::SeqCst);
                }
            }
            keys::TmuxPlan::Unreadable => notice = Some(keys::TMUX_NOTICE),
            keys::TmuxPlan::Nothing => {}
        }
    }
    if keys::under_mosh() {
        notice = Some(keys::MOSH_NOTICE);
    }
    // Whether Shift-Enter gets through tmux depends on the terminal around it too: the first
    // one that arrives says so ([`app::AppState::newline`]).
    keys::Keys {
        shift_enter: false,
        notice,
    }
}

/// Clears the screen the TUI drew on (the first time only: the panic hook and then the guard
/// both come here, and the second time the screen is the restored one, holding the panic's
/// message), then turns mouse capture, bracketed paste and focus reporting off (harmless when
/// they are not on), and pops the keyboard protocol's flags if they were pushed (while still
/// on the alternate screen, whose flags they are). The clear is for a terminal without an
/// alternate screen (or one that keeps it on exit): nothing of the TUI stays behind.
fn release_terminal_modes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static CLEARED: AtomicBool = AtomicBool::new(false);
    if !CLEARED.swap(true, Ordering::SeqCst) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
            crossterm::cursor::MoveTo(0, 0)
        );
    }
    if KEYBOARD_PUSHED.swap(false, Ordering::SeqCst) {
        let _ = crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    if MODIFY_OTHER_KEYS.swap(false, Ordering::SeqCst) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out
            .write_all(keys::MODIFY_OTHER_KEYS_OFF.as_bytes())
            .and_then(|()| out.flush());
    }
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        DisableFocusChange
    );
}

/// Clears the whole screen and has the next draw redraw every cell. Not `Terminal::clear`: it
/// first asks the terminal where its cursor is, and the key stream, reading the same input,
/// can take the answer, so the question times out ("The cursor position could not be read
/// within a normal duration") and the TUI stops.
pub fn repaint<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
) -> Result<(), B::Error> {
    terminal.backend_mut().clear()?;
    // Both buffers blank: the next draw writes everything it draws.
    terminal.swap_buffers();
    Ok(())
}

/// Polling period of `thread.activity` (what each thread is doing, for the cards).
const ACTIVITY_EVERY: Duration = Duration::from_secs(2);

/// How long quitting waits for the input history's last lines to be written.
const HISTORY_FLUSH: Duration = Duration::from_secs(2);

/// The entries of the history file at `path` (none when it cannot be read), oldest first.
/// A line that is not a JSON string (torn by a crash, not UTF-8) costs only itself.
pub fn load_history(path: &Path) -> Vec<String> {
    std::fs::read(path)
        .unwrap_or_default()
        .split(|b| *b == b'\n')
        .filter_map(|l| serde_json::from_slice::<String>(l).ok())
        .collect()
}

/// Whether the file at `path` is not empty and its last byte is not a line break.
fn ends_unfinished(path: &Path) -> std::io::Result<bool> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    if f.metadata()?.len() == 0 {
        return Ok(false);
    }
    f.seek(SeekFrom::End(-1))?;
    let mut last = [0u8; 1];
    f.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Longest line the input history keeps, in bytes: a huge paste is sent, not remembered.
pub const HISTORY_ENTRY_MAX: usize = 16 * 1024;

/// Appends `entries` to the history file at `path`, kept to the newest
/// [`app::HISTORY_MAX`] entries (rewritten when it grows past twice that). Several TUIs share
/// the file: each save holds an exclusive lock on `<path>.lock` for its append and rewrite,
/// and a rewrite goes through a temporary file of its own.
pub fn save_history(path: &Path, entries: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        twr_core::paths::ensure_private_dir(dir).map_err(std::io::Error::other)?;
    }
    let lock = twr_core::paths::open_private_append(&path.with_extension("lock"))?;
    lock.lock()?;
    let mut f = twr_core::paths::open_private_append(path)?;
    // A last line a crash left unfinished is ended first: the new lines start their own.
    if ends_unfinished(path)? {
        writeln!(f)?;
    }
    for e in entries.iter().filter(|e| e.len() <= HISTORY_ENTRY_MAX) {
        writeln!(
            f,
            "{}",
            serde_json::to_string(e).map_err(std::io::Error::other)?
        )?;
    }
    drop(f);
    let all = load_history(path);
    if all.len() > 2 * app::HISTORY_MAX {
        let keep = &all[all.len() - app::HISTORY_MAX..];
        let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
        let rewrite = || -> std::io::Result<()> {
            let mut f = twr_core::paths::open_private_append(&tmp)?;
            f.set_len(0)?;
            for e in keep {
                writeln!(
                    f,
                    "{}",
                    serde_json::to_string(e).map_err(std::io::Error::other)?
                )?;
            }
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = rewrite() {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    // The lock goes with `lock`.
    Ok(())
}

/// Takes the mouse from the terminal (`on`) or gives it back.
fn set_mouse_capture(on: bool) -> std::io::Result<()> {
    if on {
        crossterm::execute!(std::io::stdout(), EnableMouseCapture)
    } else {
        crossterm::execute!(std::io::stdout(), DisableMouseCapture)
    }
}

/// How long a local clipboard command may take before it is killed.
pub const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether `program` is an executable file in a directory of `PATH`.
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path)
            .filter(|d| !d.as_os_str().is_empty())
            .any(|d| {
                std::fs::metadata(d.join(program))
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    })
}

/// Copies `text`: OSC 52 to the terminal (works over SSH in most terminals; inside tmux it
/// needs `set -g set-clipboard on`), and, when the session has a local clipboard command
/// ([`select::clipboard_command`]), pipes it there on a task of its own, killed after
/// [`CLIPBOARD_TIMEOUT`]. A failing command is reported on `status`.
fn copy_text(text: String, status: StatusOut) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out
        .write_all(select::osc52(&text).as_bytes())
        .and_then(|()| out.flush());
    let Some((program, args)) = select::clipboard_command(
        |k| std::env::var(k).ok(),
        on_path,
        cfg!(target_os = "macos"),
    ) else {
        return;
    };
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let run = async {
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take() {
                stdin.write_all(text.as_bytes()).await?;
            }
            let st = child.wait().await?;
            if st.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!("exited with {st}")))
            }
        };
        let why = match tokio::time::timeout(CLIPBOARD_TIMEOUT, run).await {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => format!("no answer within {}s", CLIPBOARD_TIMEOUT.as_secs()),
        };
        status.send((format!("copying with {program}: {why}"), false));
    });
}

/// A status-bar line from a task off the input loop, and whether it is transient (what a
/// `say` did: [`AppState::set_status`]).
type StatusMsg = (String, bool);

/// Client name the TUI announces with `hello` (events it causes are `source = user:tui`).
pub const CLIENT_NAME: &str = "tui";

/// How long connecting to the hub (connect + `hello`) or subscribing may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one fetch of a capped attention text may take before it counts as failed (and is
/// tried again later).
const HYDRATE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the control log replay's progress is drawn while it runs.
const LOADING_EVERY: Duration = Duration::from_millis(100);

/// Most items of a stream (control, transcript) applied in one pass of the TUI's loop,
/// between draws.
const CONTROL_BATCH: usize = 512;

/// How many of the latest events of the main pane's thread the transcript replays.
pub const TRANSCRIPT_TAIL: u64 = 2000;

/// A `since_seq` past any thread's head: a subscription from it replays nothing, and its ack
/// tells the head.
const PROBE_SINCE: u64 = i64::MAX as u64;

/// Opens a connection to the hub and introduces it as the TUI. Every connection the TUI opens
/// goes through here: on the socket of a workspace opened without `--socket`, the hub must
/// serve that workspace ([`expect_workspace`]), whichever connection of its view it is.
async fn connect(socket: &Path) -> anyhow::Result<Client> {
    within(CONNECT_TIMEOUT, "connecting to the hub", async {
        let mut c = Client::connect(socket).await?;
        let hello = c
            .hello(json!({ "client": CLIENT_NAME }))
            .await
            .context("greeting the hub")?;
        if let Some(expected) = expected_workspace(socket) {
            check_hub_workspace(&hello, &expected, socket)?;
        }
        Ok(c)
    })
    .await
}

/// The workspace the hub on each socket must serve, by socket ([`expect_workspace`]).
static EXPECTED: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, String>>> =
    std::sync::Mutex::new(None);

/// From now on, a connection to `socket` (a workspace's, opened without `--socket`) must
/// reach a hub that serves `workspace`.
fn expect_workspace(socket: &Path, workspace: &str) {
    let mut map = EXPECTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.get_or_insert_with(Default::default)
        .insert(socket.to_path_buf(), workspace.to_string());
}

/// The workspace the hub on `socket` must serve, if one was set ([`expect_workspace`]).
fn expected_workspace(socket: &Path) -> Option<String> {
    let map = EXPECTED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.as_ref()?.get(socket).cloned()
}

/// `fut`, or an error once `limit` passed.
async fn within<T>(
    limit: Duration,
    what: &str,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| anyhow::anyhow!("{what}: no answer within {}s", limit.as_secs_f64()))?
}

/// Where a transcript subscription starts, given the seq the transcript on hand ends at
/// (`known`, 0 for none) and the thread's `head`: right after `known` when that leaves at most
/// [`TRANSCRIPT_TAIL`] events to replay, else at the last [`TRANSCRIPT_TAIL`] events (and the
/// transcript starts over, [`app::Transcript::resume`]).
pub fn resume_since(known: u64, head: u64) -> u64 {
    let tail = head.saturating_sub(TRANSCRIPT_TAIL);
    if (tail..=head).contains(&known) {
        known
    } else {
        tail
    }
}

/// Which thread the main pane's transcript subscription follows, and the generation of the
/// last subscription asked for: every switch asks anew, so a reply to an earlier request is
/// told apart and dropped. An A → B → A switch has two requests for A in flight; the older
/// reply's head can be behind what the newer stream applied, which would read as a shorter log
/// and empty the transcript ([`app::Transcript::resume`]).
#[derive(Debug, Default)]
struct Follow {
    thread: Option<String>,
    generation: u64,
}

impl Follow {
    /// Follows `main`, the main pane's thread: the generation of the subscription to ask for
    /// when it is not the thread followed, else `None`.
    fn follow(&mut self, main: Option<String>) -> Option<u64> {
        if main == self.thread {
            return None;
        }
        self.thread = main;
        self.generation += 1;
        Some(self.generation)
    }

    /// Whether a reply to the subscription asked for `thread` as `generation` is the one to
    /// take: the last one asked, for the thread still followed.
    fn current(&self, thread: &str, generation: u64) -> bool {
        self.generation == generation && self.thread.as_deref() == Some(thread)
    }
}

/// Subscribes to `thread` after seq `known`, the end of the transcript on hand, or from its
/// last [`TRANSCRIPT_TAIL`] events ([`resume_since`]; the head comes from a first
/// subscription that replays nothing). Returns the stream, the seq it starts after and the
/// thread's head when it was made.
async fn subscribe_thread(
    socket: &Path,
    thread: &str,
    known: u64,
    limit: Duration,
) -> anyhow::Result<(ItemStream, u64, u64)> {
    within(limit, "subscribing to the transcript", async {
        let probe = connect(socket)
            .await?
            .subscribe(
                "thread.subscribe",
                json!({"thread": thread, "since_seq": PROBE_SINCE}),
            )
            .await?;
        let head = probe.ack()["head"].as_u64().unwrap_or(0);
        drop(probe);
        let since = resume_since(known, head);
        let sub = connect(socket)
            .await?
            .subscribe(
                "thread.subscribe",
                json!({"thread": thread, "since_seq": since}),
            )
            .await?;
        let head = sub.ack()["head"].as_u64().unwrap_or(head);
        let blobs = connect(socket).await?;
        let items = read_ahead(hydrating(
            Box::pin(sub.into_stream()),
            blobs,
            socket,
            HYDRATE_TIMEOUT,
        ));
        Ok((items, since, head))
    })
    .await
}

/// The item with its capped payload fields restored, or `None` when it has none.
///
/// The reducers must never fold previews: a question whose options were moved to the blob
/// store would otherwise show no options and be unanswerable.
pub async fn hydrate_item<F: BlobFetch>(
    item: &StreamItem,
    fetch: &mut F,
) -> anyhow::Result<Option<StreamItem>> {
    // Most items have nothing moved: told without building their `data` (a replay asks this
    // of every item).
    if !item.event.body.maybe_capped() {
        return Ok(None);
    }
    let mut data = item.event.body.data()?;
    if !has_capped(&data) {
        return Ok(None);
    }
    hydrate_deep(&mut data, fetch).await?;
    let mut full = item.clone();
    full.event.body = EventBody::from_kind_data(item.event.kind(), data)?;
    Ok(Some(full))
}

/// `stream` with every item hydrated ([`hydrate_item`]) over `blobs`, in order, each within
/// `limit`. An item that cannot be hydrated is passed on as it is; the connection it was
/// fetched over is then made again on `socket` for the next one ([`Blobs`]).
fn hydrating(stream: ItemStream, blobs: Client, socket: &Path, limit: Duration) -> ItemStream {
    let blobs = Blobs {
        client: Some(blobs),
        socket: socket.to_path_buf(),
    };
    Box::pin(futures::stream::unfold(
        (stream, blobs),
        move |(mut stream, mut blobs)| async move {
            let next = match stream.next().await? {
                Ok(item) => Ok(blobs.hydrate(item, limit).await),
                Err(e) => Err(e),
            };
            Some((next, (stream, blobs)))
        },
    ))
}

/// `stream` read on a task of its own, ahead of the loop that takes it, in order, its error
/// or end last: reading and decoding a replay of thousands takes longer than applying it, and
/// runs meanwhile on another core, while the loop stays free to draw and read keys. What is
/// ready goes over in batches of up to [`READ_AHEAD`] items (at most [`READ_AHEAD_BATCHES`]
/// batches ahead), so the loop wakes once for many. Once the stream returned is dropped, the
/// task ends at once, and the subscription with it (its connection closes), as it would
/// without the task: it may wait for a live item that never comes.
fn read_ahead(mut stream: ItemStream) -> ItemStream {
    let (tx, rx) = mpsc::channel::<Vec<anyhow::Result<StreamItem>>>(READ_AHEAD_BATCHES);
    tokio::spawn(async move {
        loop {
            let first = tokio::select! {
                item = stream.next() => item,
                () = tx.closed() => return,
            };
            let Some(first) = first else {
                return;
            };
            let mut done = first.is_err();
            let mut batch = vec![first];
            while !done && batch.len() < READ_AHEAD {
                match stream.next().now_or_never() {
                    Some(Some(item)) => {
                        done = item.is_err();
                        batch.push(item);
                    }
                    Some(None) => done = true,
                    None => break,
                }
            }
            if tx.send(batch).await.is_err() || done {
                return;
            }
        }
    });
    Box::pin(futures::stream::unfold(
        (rx, std::collections::VecDeque::new()),
        |(mut rx, mut ready)| async move {
            if ready.is_empty() {
                ready.extend(rx.recv().await?);
            }
            let item = ready.pop_front()?;
            Some((item, (rx, ready)))
        },
    ))
}

/// Most items [`read_ahead`] hands over at once.
const READ_AHEAD: usize = 512;

/// How many batches [`read_ahead`] reads ahead.
const READ_AHEAD_BATCHES: usize = 4;

/// The blob connection of a stream being hydrated ([`hydrating`]). A fetch that failed or ran
/// out of time may have left part of a reply unread on it, which the next one would read as
/// its own: it is dropped then, and the next fetch connects again.
struct Blobs {
    client: Option<Client>,
    socket: PathBuf,
}

impl Blobs {
    /// `item` hydrated within `limit` ([`hydrate_item`]), else as it is.
    async fn hydrate(&mut self, item: StreamItem, limit: Duration) -> StreamItem {
        match within(
            limit,
            "fetching the full event text",
            hydrate_item(&item, self),
        )
        .await
        {
            Ok(Some(full)) => full,
            Ok(None) => item,
            Err(_) => {
                self.client = None;
                item
            }
        }
    }
}

impl BlobFetch for Blobs {
    async fn fetch(&mut self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let client = match &mut self.client {
            Some(client) => client,
            None => self.client.insert(connect(&self.socket).await?),
        };
        client.blob_get(blob_ref).await
    }
}

/// Sleeps until `at`; pends forever when there is no `at`.
/// Tells the hub on `socket` what `rx` says of the user's attention to the TUI
/// (`notify.looking`), on a connection of its own, made at the first word and again after it
/// failed; until `rx`'s sender is gone (the view ended), when the connection closes with it. A
/// hub without the method (an older one) is told nothing more.
async fn tell_attendance(socket: PathBuf, mut rx: tokio::sync::watch::Receiver<Option<bool>>) {
    let mut client: Option<Client> = None;
    while rx.changed().await.is_ok() {
        let Some(looking) = *rx.borrow_and_update() else {
            continue;
        };
        if client.is_none() {
            client = connect(&socket).await.ok();
        }
        let Some(c) = client.as_mut() else {
            continue;
        };
        match c
            .call("notify.looking", json!({ "looking": looking }))
            .await
        {
            Ok(_) => {}
            Err(e)
                if e.downcast_ref::<twr_core::proto::RpcError>()
                    .is_some_and(|r| r.code == twr_core::proto::codes::METHOD_NOT_FOUND) =>
            {
                return
            }
            Err(_) => client = None,
        }
    }
}

async fn sleep_opt(at: Option<std::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
        None => std::future::pending().await,
    }
}

/// Applies control `item` to `state` and shows it to `notifier`: an item of the initial
/// replay as the past, a later one as news. A duplicate (already applied) is neither.
fn apply_control(
    state: &mut AppState,
    notifier: &mut notify::Notifier,
    item: &StreamItem,
    now: std::time::Instant,
) {
    let new = item.gseq.is_none_or(|g| g > state.last_gseq);
    let replay = state.catch_up.is_some();
    state.apply_event(item);
    if !new {
        return;
    }
    let (ev, registry, attention) = (&item.event, &state.registry, &state.attention);
    if replay {
        notifier.recall(ev, item.gseq, registry, attention);
    } else {
        notifier.observe(ev, item.gseq, registry, attention, now);
    }
}

/// The control stream ended (`why`): no more replay to wait for, so what is known is drawn,
/// with why; and nothing held is notified, since nothing can check it again.
fn control_lost(state: &mut AppState, notifier: &mut notify::Notifier, why: String) {
    state.control_ended();
    state.view.status = Some(why);
    notifier.lost();
}

/// Next item of an optional stream; pends forever when there is none.
async fn next_opt(s: &mut Option<ItemStream>) -> Option<anyhow::Result<StreamItem>> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

fn handle_terminal_event(
    state: &mut AppState,
    event: TermEvent,
    screen: &ratatui::buffer::Buffer,
    has_drawn: bool,
) -> Vec<Action> {
    use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};

    // Discard input until the replayed state has actually been shown. Buffering a command
    // or its confirmation would let it run later without the user seeing what it answers.
    // Only quitting works: Ctrl-C (a typed `q` is a letter).
    if !has_drawn || state.catch_up.is_some() {
        return match event {
            TermEvent::Key(k)
                if k.kind != KeyEventKind::Release
                    && k.code == KeyCode::Char('c')
                    && k.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                vec![Action::Quit]
            }
            _ => Vec::new(),
        };
    }
    match event {
        TermEvent::Key(k) => state.handle_key(k),
        TermEvent::Mouse(m) => state.handle_mouse(m, screen),
        TermEvent::Paste(text) => {
            state.handle_paste(&text);
            Vec::new()
        }
        // The selection was drawn on the old layout.
        TermEvent::Resize(..) => {
            state.view.selection = None;
            Vec::new()
        }
        _ => Vec::new(), // focus and friends: just redraw
    }
}

/// A terminal event and when the input thread read it.
type Stamped = (std::time::Instant, std::io::Result<TermEvent>);

/// How long the input thread waits for an event before it checks whether to stop.
const INPUT_POLL: Duration = Duration::from_millis(50);

/// The terminal's reader: a thread of its own, the only reader, handing each event over with
/// the time it was read. Whether a key was typed ahead of the first frame goes by that time,
/// never by when the loop got to it (after a long draw, say). Dropping it stops and joins the
/// thread, so nothing reads the terminal once it is the user's again.
struct Input {
    rx: mpsc::UnboundedReceiver<Stamped>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Input {
    /// Reads with `next`, which waits up to the given time for an event (`None`: none came).
    /// The thread ends after a read error, or once the receiver is closed.
    fn spawn<F>(mut next: F) -> anyhow::Result<Input>
    where
        F: FnMut(Duration) -> std::io::Result<Option<TermEvent>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<Stamped>();
        let thread = std::thread::Builder::new()
            .name("twr-input".into())
            .spawn(move || {
                while !tx.is_closed() {
                    let event = match next(INPUT_POLL) {
                        Ok(None) => continue,
                        Ok(Some(event)) => Ok(event),
                        Err(e) => Err(e),
                    };
                    let failed = event.is_err();
                    if tx.send((std::time::Instant::now(), event)).is_err() || failed {
                        return;
                    }
                }
            })
            .context("starting the input thread")?;
        Ok(Input {
            rx,
            thread: Some(thread),
        })
    }

    /// The terminal's events, read with crossterm.
    fn terminal() -> anyhow::Result<Input> {
        Input::spawn(|timeout| {
            if crossterm::event::poll(timeout)? {
                crossterm::event::read().map(Some)
            } else {
                Ok(None)
            }
        })
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.rx.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// After the first frame, keys count once the terminal has been quiet this long: what comes
/// before was typed while the screen was blank, or too soon after the frame to answer it.
const TYPED_AHEAD_QUIET: Duration = Duration::from_millis(150);
/// The status while the rest of a line typed ahead is dropped.
const TYPED_AHEAD_LINE: &str = "dropping a line typed before the screen was ready, up to its Enter";
/// The status once it is.
const TYPED_AHEAD_DROPPED: &str = "dropped a line typed before the screen was ready: type it again";

/// The input typed ahead of the first frame, by the time each event was read ([`Input`]). From the first frame on, keys and pastes are dropped (all but quitting:
/// [`handle_terminal_event`] before a frame) until [`TYPED_AHEAD_QUIET`] goes by without one;
/// each dropped key extends the window. If the window ends in the middle of a line, the rest
/// of that line is dropped too, through its Enter, so a line is dropped whole, never only its
/// first words. Esc does not end it: it never throws the line typed away, so typing goes on
/// in the same line after it. Other events (the mouse, key releases) are dropped in the window
/// without extending it, and are not part of a line.
#[derive(Debug, PartialEq, Eq)]
enum TypedAhead {
    /// Before the first frame, where the guard before a frame drops everything: `line`,
    /// whether the keys dropped so far left a line unfinished.
    Blank { line: bool },
    /// Keys count.
    Off,
    /// From the first frame: `last` is its time, then the last dropped key's; `line`, whether
    /// the keys dropped so far left a line unfinished.
    Window {
        last: std::time::Instant,
        line: bool,
    },
    /// The window went quiet in the middle of a line: its rest is dropped.
    Line,
}

impl Default for TypedAhead {
    fn default() -> Self {
        TypedAhead::Blank { line: false }
    }
}

impl TypedAhead {
    /// The first frame is on screen: a line left unfinished before it is still dropped whole.
    fn start(&mut self, now: std::time::Instant) {
        *self = TypedAhead::Window {
            last: now,
            line: matches!(self, TypedAhead::Blank { line: true }),
        };
    }

    /// Whether `event`, read at `at`, was typed ahead and is dropped, and what the status bar
    /// says about it.
    fn drops(&mut self, event: &TermEvent, at: std::time::Instant) -> (bool, Option<&'static str>) {
        use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
        // A key's release (reported by terminals that report them) types nothing.
        let press =
            |e: &TermEvent| matches!(e, TermEvent::Key(k) if k.kind != KeyEventKind::Release);
        let key = press(event) || matches!(event, TermEvent::Paste(_));
        // Only a plain Enter ends the line: Shift-Enter and Alt-Enter break it
        // ([`app::AppState::newline`]) and Esc keeps it, so it goes on.
        let ends = press(event)
            && matches!(event, TermEvent::Key(k) if k.code == KeyCode::Enter
                && !k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT));
        match *self {
            TypedAhead::Blank { .. } => {
                if key {
                    *self = TypedAhead::Blank { line: !ends };
                }
                return (true, None);
            }
            TypedAhead::Off => return (false, None),
            TypedAhead::Window { last, .. }
                if at.saturating_duration_since(last) < TYPED_AHEAD_QUIET =>
            {
                if key {
                    *self = TypedAhead::Window {
                        last: last.max(at),
                        line: !ends,
                    };
                }
                return (true, None);
            }
            TypedAhead::Window { line: false, .. } => {
                *self = TypedAhead::Off;
                return (false, None);
            }
            TypedAhead::Window { line: true, .. } => *self = TypedAhead::Line,
            TypedAhead::Line => {}
        }
        if ends {
            *self = TypedAhead::Off;
            (true, Some(TYPED_AHEAD_DROPPED))
        } else if key {
            (true, Some(TYPED_AHEAD_LINE))
        } else {
            (false, None)
        }
    }
}

/// The workspaces a TUI can switch between (`twr ws`): the CLI provides them, as it knows
/// where they are and how to start their hubs.
pub trait Workspaces: Send + Sync {
    /// Their names, in the tab strip's order (`default` first).
    fn names(&self) -> anyhow::Result<Vec<String>>;
    /// The socket of workspace `name`'s hub, whether it runs or not.
    fn socket(&self, name: &str) -> anyhow::Result<PathBuf>;
    /// Makes sure workspace `name`'s hub answers, starting it when none runs; its socket.
    fn open(&self, name: &str) -> futures::future::BoxFuture<'static, anyhow::Result<PathBuf>>;
    /// Creates workspace `name`.
    fn create(&self, name: &str) -> anyhow::Result<()>;
}

/// The workspace a TUI opens on, and the ones it can switch to.
pub struct Switching {
    /// The workspace of the socket the TUI opens on.
    pub current: String,
    /// All of them.
    pub workspaces: std::sync::Arc<dyn Workspaces>,
}

/// How often the tab strip asks the other workspaces' hubs how they are.
const TABS_EVERY: Duration = Duration::from_secs(5);

/// How often the TUI reads this machine's load for its status bar ([`twr_core::load`]).
pub const LOAD_EVERY: Duration = Duration::from_secs(5);

/// How often the TUI asks its hub (`hub.info`) whether a box is low on disk (`disk`) and how
/// its remote boxes are (`boxes`: a thread's box token).
pub const INFO_EVERY: Duration = Duration::from_secs(10);

/// How long the tab strip waits for one hub's answer.
const TAB_TIMEOUT: Duration = Duration::from_secs(1);

/// A hub connected to and caught up with as far as the TUI starts on it: its `hub.info` and
/// `orchestrator.show`, the connection for calls, and the control stream with the head its
/// replay runs up to.
struct Session {
    socket: PathBuf,
    calls: Client,
    info: Value,
    show: Value,
    control: ItemStream,
    head: u64,
}

impl Session {
    /// Connects to the hub on `socket`: what [`run`] needs before it draws anything of it.
    /// With `expected`, the hub must serve that workspace ([`check_hub_workspace`]).
    async fn connect(socket: &Path, expected: Option<&str>) -> anyhow::Result<Session> {
        if let Some(expected) = expected {
            expect_workspace(socket, expected);
        }
        let mut calls = connect(socket).await?;
        twr_core::trace::mark("tui: connected");
        // Identify the connected hub, even when the client used an explicit socket.
        let info = within(CONNECT_TIMEOUT, "asking for the instance", async {
            calls.call("hub.info", Value::Null).await
        })
        .await?;
        if let Some(expected) = expected {
            check_hub_workspace(&info, expected, socket)?;
        }
        // The runtimes `/orch` completes, and whether the agent was ever chosen.
        twr_core::trace::mark("tui: hub.info answered");
        let show = within(CONNECT_TIMEOUT, "asking for the orchestrator", async {
            calls.call("orchestrator.show", Value::Null).await
        })
        .await
        .unwrap_or(Value::Null);
        let control = within(
            CONNECT_TIMEOUT,
            "subscribing to the control stream",
            async {
                connect(socket)
                    .await?
                    .subscribe("control.subscribe", json!({"since": 0}))
                    .await
                    .context("subscribing to the control stream")
            },
        )
        .await?;
        // The replay up to the ack's head is the initial state; focus waits for it.
        let head = control.ack()["head"].as_u64().unwrap_or(0);
        twr_core::trace::mark_with(|| format!("tui: control subscribed (head {head})"));
        let blobs = connect(socket).await?;
        let control = read_ahead(hydrating(
            Box::pin(control.into_stream()),
            blobs,
            socket,
            HYDRATE_TIMEOUT,
        ));
        Ok(Session {
            socket: socket.to_path_buf(),
            calls,
            info,
            show,
            control,
            head,
        })
    }
}

/// Creates workspace `name` when asked, opens its hub and connects to it.
async fn open_workspace(ws: &dyn Workspaces, name: &str, create: bool) -> anyhow::Result<Session> {
    if create {
        create_workspace(ws, name)?;
    }
    let socket = ws.open(name).await?;
    Session::connect(&socket, Some(name)).await
}

/// Creates workspace `name`; one made meanwhile (another terminal, or before the tabs knew)
/// is fine: it is opened as it is.
fn create_workspace(ws: &dyn Workspaces, name: &str) -> anyhow::Result<()> {
    match ws.create(name) {
        Ok(()) => Ok(()),
        Err(_) if ws.names().is_ok_and(|n| n.iter().any(|w| w == name)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Refuses a hub that serves another workspace than `expected` (its `workspace`, in `info`
/// as `hello` and `hub.info` say it): it took that workspace's socket. One that says none
/// predates workspaces and serves `default` only.
fn check_hub_workspace(info: &Value, expected: &str, socket: &Path) -> anyhow::Result<()> {
    match info["workspace"].as_str() {
        Some(w) if w != expected => anyhow::bail!(
            "the hub at {} serves workspace {w}, not {expected}",
            socket.display()
        ),
        None if expected != twr_core::paths::DEFAULT_WORKSPACE => anyhow::bail!(
            "the hub at {} serves no workspace {expected} (it is older than workspaces)",
            socket.display()
        ),
        _ => Ok(()),
    }
}

/// What the tab strip shows of the workspaces other than `current`: each hub asked how it is
/// ([`tab_hub`]), in the order [`Workspaces::names`] gives.
async fn poll_tabs(ws: &dyn Workspaces, current: Option<&str>) -> Vec<workspace::Tab> {
    let Ok(names) = ws.names() else {
        return Vec::new();
    };
    let mut tabs = Vec::new();
    for name in names {
        let hub = if Some(name.as_str()) == current {
            workspace::TabHub::Unknown
        } else {
            match ws.socket(&name) {
                Ok(socket) => tab_hub(&socket, &name).await,
                Err(_) => workspace::TabHub::Unknown,
            }
        };
        tabs.push(workspace::Tab { name, hub });
    }
    tabs
}

/// How the hub of workspace `name` on `socket` is, for its tab: stopped when nothing listens,
/// busy when it does not answer within [`TAB_TIMEOUT`] (or is another workspace's), else its
/// worker threads, how many run and the permissions and questions waiting on the user.
async fn tab_hub(socket: &Path, name: &str) -> workspace::TabHub {
    use workspace::TabHub;
    let Ok(Ok(mut c)) = tokio::time::timeout(TAB_TIMEOUT, Client::connect(socket)).await else {
        return TabHub::Stopped;
    };
    let answer = tokio::time::timeout(TAB_TIMEOUT, async {
        let hello = c.hello(json!({ "client": CLIENT_NAME })).await?;
        check_hub_workspace(&hello, name, socket)?;
        let threads = c.call("thread.list", Value::Null).await?;
        let items = c.call("attention.list", json!({ "all": true })).await?;
        anyhow::Ok((threads, items))
    })
    .await;
    let Ok(Ok((threads, items))) = answer else {
        return TabHub::Busy;
    };
    let workers: Vec<&Value> = threads
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["role"] != "orchestrator")
        .collect();
    let running = workers
        .iter()
        .filter(|t| matches!(t["status"].as_str(), Some("running" | "background")))
        .count();
    let of = |reason: &str| {
        items
            .as_array()
            .into_iter()
            .flatten()
            .filter(|i| i["reason"].as_str() == Some(reason))
            .count()
    };
    TabHub::Running {
        threads: workers.len(),
        running,
        permissions: of("permission"),
        questions: of("question"),
    }
}

/// A connection hub calls go over ([`serve_calls`]).
trait CallHub: Send {
    /// Calls `method` with `params`.
    fn call(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<Value>>;
}

impl CallHub for Client {
    fn call(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<Value>> {
        Box::pin(Client::call(self, method, params))
    }
}

/// A view's calls connection to the hub on `socket`, opened again after the hub closed it (it
/// stopped or restarted since: every later call failed with "Broken pipe" before). A call the
/// hub closed the connection on before it reached it ([`HubClosed::resendable`]: nothing was
/// done) is sent once more on the new connection, so a prompt is never sent twice; one the
/// hub may have done is not, and says so.
struct HubCalls {
    socket: PathBuf,
    client: Option<Client>,
}

impl CallHub for HubCalls {
    fn call(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<Value>> {
        Box::pin(async move {
            let client = match &mut self.client {
                Some(c) => c,
                None => self.client.insert(
                    connect(&self.socket)
                        .await
                        .context("connecting to the hub again")?,
                ),
            };
            let e = match client.call(method, params.clone()).await {
                Err(e) if e.downcast_ref::<HubClosed>().is_some() => e,
                r => return r,
            };
            self.client = None;
            if !e
                .downcast_ref::<HubClosed>()
                .is_some_and(HubClosed::resendable)
            {
                return Err(e);
            }
            let mut client = match connect(&self.socket).await {
                Ok(c) => c,
                Err(again) => return Err(anyhow::anyhow!("{e:#} (connecting again: {again:#})")),
            };
            let r = client.call(method, params).await;
            if !r
                .as_ref()
                .is_err_and(|e| e.downcast_ref::<HubClosed>().is_some())
            {
                self.client = Some(client);
            }
            r
        })
    }
}

/// Where a task of one workspace's view says what happened: its status bar (`tx`) while the
/// view lasts; once it is gone (a switch to another workspace), what is not transient goes to
/// the next view's (`late`, with the workspace it came from). The commands a message quotes
/// (`` `twr ...` ``) get the view's `selection` ([`workspace::attach_via`]).
#[derive(Clone)]
struct StatusOut {
    tx: mpsc::UnboundedSender<StatusMsg>,
    late: Option<(mpsc::UnboundedSender<StatusMsg>, String)>,
    selection: String,
}

impl StatusOut {
    fn send(&self, (msg, transient): StatusMsg) {
        let msg = twr_core::paths::localize_commands(&msg, &self.selection);
        if let Err(mpsc::error::SendError((msg, transient))) = self.tx.send((msg, transient)) {
            forward_late(&self.late, msg, transient);
        }
    }
}

/// Starts the thread that writes the input history to `path`, in order ([`save_history`]),
/// saying a failure on `status`. Returns where to send the lines added, and the thread.
fn spawn_history_writer(
    path: PathBuf,
    status: StatusOut,
) -> (
    std::sync::mpsc::Sender<Vec<String>>,
    Option<std::thread::JoinHandle<()>>,
) {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<String>>();
    let writer = std::thread::Builder::new()
        .name("twr-history".into())
        .spawn(move || {
            for added in rx {
                if let Err(e) = save_history(&path, &added) {
                    status.send((format!("input history: {e}"), false));
                }
            }
        })
        .ok();
    (tx, writer)
}

/// `msg` for the next view's status bar (`late`: its sender and the workspace `msg` came
/// from), unless it is transient (news only for a moment) or empty.
fn forward_late(
    late: &Option<(mpsc::UnboundedSender<StatusMsg>, String)>,
    msg: String,
    transient: bool,
) {
    if let (Some((late, workspace)), false) = (late, transient || msg.is_empty()) {
        let _ = late.send((format!("workspace {workspace}: {msg}"), false));
    }
}

/// A view's status bar (`rx`) is gone: closed first, so what its tasks say from now on goes
/// to the next view ([`StatusOut`]), then what it had not shown yet goes there too
/// ([`forward_late`]), including a message a task was sending as it closed (`recv` waits for
/// it, `try_recv` would not).
async fn forward_unread(
    rx: &mut mpsc::UnboundedReceiver<StatusMsg>,
    late: &Option<(mpsc::UnboundedSender<StatusMsg>, String)>,
) {
    rx.close();
    while let Some((msg, transient)) = rx.recv().await {
        forward_late(late, msg, transient);
    }
}

/// Starts the hub calls of one workspace's view ([`serve_calls`]) once `previous`, its earlier
/// view's (left by a switch and still sending what it had been asked), is done: one
/// workspace's calls go out in the order they were asked, across switches.
fn spawn_calls(
    previous: Option<tokio::task::JoinHandle<()>>,
    calls: impl CallHub + 'static,
    rx: mpsc::UnboundedReceiver<(&'static str, Value)>,
    status: StatusOut,
    results: ResultsOut,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Some(previous) = previous {
            let _ = previous.await;
        }
        serve_calls(calls, rx, status, results).await;
    })
}

/// Stops every view's calls in `callers` (quitting): also those a later view of their
/// workspace waits for, and those of workspaces in the background.
fn abort_all(callers: &mut Vec<tokio::task::AbortHandle>) {
    for c in callers.drain(..) {
        c.abort();
    }
}

/// The end of a view's calls (`caller`): leaving for another workspace, what was asked of
/// this hub is still sent (a prompt, an answer), and the next view of `workspace` waits for
/// it (`callers`); quitting stops it.
fn end_calls(
    switching: bool,
    caller: tokio::task::JoinHandle<()>,
    workspace: Option<&str>,
    callers: &mut std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
) {
    match (switching, workspace) {
        (true, Some(w)) => {
            callers.insert(w.to_string(), caller);
        }
        _ => caller.abort(),
    }
}

/// The calls whose results the view follows a message with: what was sent and its ref, and
/// taking it back ([`AppState::apply_result`]). A failed one comes as `{failed: true, ref}`
/// (`ref`: the one it named, if any).
const FOLLOWED: [&str; 3] = ["say", "thread.send", "message.withdraw"];

/// A results channel of a view ([`ResultsOut`]).
type ResultTx = mpsc::UnboundedSender<(&'static str, Value)>;
type ResultRx = mpsc::UnboundedReceiver<(&'static str, Value)>;

/// Where one workspace's view's calls ([`serve_calls`]) put what they returned beyond the
/// status bar: `view`, what only that view shows (the trash list, ...), gone with it;
/// `followed`, the [`FOLLOWED`] results, which belong to the workspace: one that comes after a
/// switch waits for the next view of that workspace, which the view left took its tracking
/// along to ([`crate::workspace::SavedView`]).
#[derive(Clone)]
struct ResultsOut {
    view: ResultTx,
    followed: ResultTx,
}

/// Runs the hub calls of one workspace's view (`rx`), in order, over `calls`, until `rx`
/// closes and what it holds is sent: what a call did and its errors go to the status bar
/// (`status`, which hands them to the next view once this one is gone), results the view
/// shows to `results`. So a prompt or answer asked for before a switch is still sent, and its
/// failure still shown.
async fn serve_calls(
    mut calls: impl CallHub,
    mut rx: mpsc::UnboundedReceiver<(&'static str, Value)>,
    status: StatusOut,
    results: ResultsOut,
) {
    while let Some((method, params)) = rx.recv().await {
        let named = params["ref"].clone();
        let named_job = params["job"].clone();
        match calls.call(method, params).await {
            // What a message did ("sent to the orchestrator") is news only for a moment.
            Ok(r) => {
                if let Some(msg) = result_status(method, &r) {
                    status.send((msg, transient_status(method, &r)));
                }
                if FOLLOWED.contains(&method) {
                    let _ = results.followed.send((method, r));
                } else if method == "orchestrator.reset"
                    || method == "thread.activity"
                    || method == "thread.queue"
                    || method == "message.send_now"
                    || method == "shell.run"
                    || method == "shell.cd"
                    || method == "job.log"
                    || method == "hub.info"
                    || method == "trash.list"
                    || (method == "trash.empty" && is_dry_run(&r))
                {
                    let _ = results.view.send((method, r));
                }
            }
            // A failed poll says nothing: the next one tries again.
            Err(_)
                if method == "thread.activity"
                    || method == "hub.info"
                    || method == "thread.queue" =>
            {
                let _ = results.view.send((method, Value::Null));
            }
            // A failed `!cd`: said, and commands run where they did.
            Err(e) if method == "shell.cd" => {
                status.send((error_status(method, &format!("{e:#}")), false));
                let _ = results.view.send((method, json!({"failed": true})));
            }
            // A command's log that can't be read: it shows what its end recorded.
            Err(_) if method == "job.log" => {
                let _ = results
                    .view
                    .send((method, json!({"failed": true, "job": named_job})));
            }
            Err(e) => {
                status.send((error_status(method, &format!("{e:#}")), false));
                if FOLLOWED.contains(&method) {
                    let failed = json!({"failed": true, "ref": named});
                    let _ = results.followed.send((method, failed));
                }
            }
        }
    }
}

/// Why [`drive`] returned.
enum End {
    /// The user quit.
    Quit,
    /// The user switched to workspace `to`, whose hub `next` is connected: the view of the
    /// workspace left, the tabs and whether the TUI has the mouse go with it.
    Switch {
        to: String,
        next: Box<Session>,
        view: Box<workspace::SavedView>,
        tabs: Vec<workspace::Tab>,
        mouse: bool,
    },
}

/// What lasts across the workspaces a TUI shows: the `[tui]` settings, the clock of the
/// working icon, and the workspaces with how many items each one had waiting when last seen.
struct Shared {
    /// The calls each workspace left by a switch is still sending ([`end_calls`]).
    callers: std::collections::HashMap<String, tokio::task::JoinHandle<()>>,
    /// Every view's calls ever started: quitting stops all that still run ([`abort_all`]).
    all_callers: Vec<tokio::task::AbortHandle>,
    /// Errors of calls a workspace left asked for, once another one shows ([`serve_calls`]).
    late_tx: mpsc::UnboundedSender<StatusMsg>,
    late_rx: mpsc::UnboundedReceiver<StatusMsg>,
    /// Each workspace left by a switch: its followed results' channel ([`ResultsOut`]).
    followed: std::collections::HashMap<String, (ResultTx, ResultRx)>,
    tui: twr_core::config::TuiConfig,
    epoch: std::time::Instant,
    workspaces: Option<std::sync::Arc<dyn Workspaces>>,
    waiting_seen: std::collections::HashMap<String, usize>,
    /// What the terminal tells apart ([`enhance_keyboard`]); its notice goes to the first
    /// workspace shown, and a Shift-Enter seen in one is known in every other.
    keys: keys::Keys,
    /// `/compact on|off`, for every workspace shown ([`crate::app::ViewState::compact`]).
    compact: Option<bool>,
    /// `/suggest on|off`, for every workspace shown ([`crate::suggest`]).
    suggest: Option<bool>,
}

/// Runs the TUI against the hub listening on `socket` until the user quits.
pub async fn run(socket: PathBuf) -> anyhow::Result<()> {
    run_with(socket, None, None).await
}

/// The `[tui]` settings: `given` (what the CLI read from the config of the workspace it
/// resolved), else read from the config the environment names.
pub fn tui_config(given: Option<twr_core::config::TuiConfig>) -> twr_core::config::TuiConfig {
    given.unwrap_or_else(|| {
        twr_core::paths::Paths::from_env()
            .and_then(|p| twr_core::config::Config::load_from(&p))
            .map(|c| c.tui)
            .unwrap_or_default()
    })
}

/// Runs the TUI against the hub listening on `socket` until the user quits; with `switching`,
/// on the workspace it names, with a tab per workspace to switch to ([`workspace`]). `tui`:
/// the `[tui]` settings ([`tui_config`]).
pub async fn run_with(
    socket: PathBuf,
    switching: Option<Switching>,
    tui: Option<twr_core::config::TuiConfig>,
) -> anyhow::Result<()> {
    let tui = tui_config(tui);
    // Connect before touching the terminal so connection errors print normally.
    // A workspace's socket must be that workspace's hub (`switching`: the socket was not given).
    let mut session =
        Session::connect(&socket, switching.as_ref().map(|s| s.current.as_str())).await?;

    let mut terminal = ratatui::try_init().context("initializing the terminal")?;
    twr_core::trace::mark("tui: terminal initialized");
    let guard = TerminalGuard;
    // A terminal without an alternate screen (or one that keeps what was on it) still shows
    // the shell's lines: the first frame draws only what is not blank, so they would stay.
    repaint(&mut terminal).context("clearing the screen")?;
    // ratatui's panic hook restores the terminal; this one runs first and gives the mouse and
    // paste modes back too.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        release_terminal_modes();
        hook(info);
    }));
    // Pastes arrive as one event: never as keys (a pasted line break would send the line, a
    // pasted digit would answer a card).
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste)
        .context("enabling bracketed paste")?;
    let keys = enhance_keyboard();
    if tui.mouse {
        set_mouse_capture(true).context("enabling the mouse")?;
    }
    // Terminal notifications go out only while the terminal is not focused, and the hub's
    // notification command keeps quiet only while it is: the terminal says when that changes.
    let mut notifier = notify::Notifier::new(&tui.notify, notify::Multiplexer::from_env());
    crossterm::execute!(std::io::stdout(), EnableFocusChange)
        .context("enabling focus reporting")?;
    let mut input = Input::terminal()?;
    let (late_tx, late_rx) = mpsc::unbounded_channel::<StatusMsg>();
    let mut ctx = Shared {
        callers: std::collections::HashMap::new(),
        all_callers: Vec::new(),
        late_tx,
        late_rx,
        followed: std::collections::HashMap::new(),
        tui,
        // The working icon's clock: every icon's frame comes from the time since this instant.
        epoch: std::time::Instant::now(),
        workspaces: switching.as_ref().map(|s| s.workspaces.clone()),
        waiting_seen: std::collections::HashMap::new(),
        keys,
        compact: None,
        suggest: None,
    };
    let mut current = switching.map(|s| s.current);
    // The views of the workspaces left, by name.
    let mut saved: std::collections::HashMap<String, workspace::SavedView> =
        std::collections::HashMap::new();
    // The tabs are known from the start (their hubs are asked at the first poll), so `/ws`
    // never takes an existing workspace for a new one.
    let tabs = ctx
        .workspaces
        .as_ref()
        .and_then(|ws| ws.names().ok())
        .unwrap_or_default()
        .into_iter()
        .map(|name| workspace::Tab {
            name,
            hub: workspace::TabHub::Unknown,
        })
        .collect();
    let mut carried = Carried {
        mouse: ctx.tui.mouse,
        view: None,
        tabs,
    };
    let (result, history_writer) = loop {
        ctx.all_callers.retain(|h| !h.is_finished());
        let (end, writer) = drive(
            &mut terminal,
            &mut input,
            &mut notifier,
            &mut ctx,
            session,
            current.clone(),
            std::mem::take(&mut carried),
        )
        .await;
        match end {
            Ok(End::Switch {
                to,
                next,
                view,
                tabs,
                mouse,
            }) => {
                // The lines sent last in the workspace left are written before its history
                // is read again.
                flush_history(writer);
                if let Some(left) = current.replace(to.clone()) {
                    saved.insert(left, *view);
                }
                carried = Carried {
                    mouse,
                    view: saved.remove(&to),
                    tabs,
                };
                session = *next;
                notifier = notifier.renewed();
            }
            Ok(End::Quit) => break (Ok(()), writer),
            Err(e) => break (Err(e), writer),
        }
    };
    // Nothing is sent once the user quit, from any workspace.
    abort_all(&mut ctx.all_callers);

    // Nothing reads the terminal once it is the user's again.
    drop(input);
    drop(terminal);
    // The terminal is the user's again before anything else waits.
    drop(guard);
    // The lines sent last are written before the TUI exits, unless another TUI holds the
    // file's lock past [`HISTORY_FLUSH`].
    flush_history(history_writer);
    result
}

/// Waits for the history writer to finish, at most [`HISTORY_FLUSH`].
fn flush_history(writer: Option<std::thread::JoinHandle<()>>) {
    if let Some(w) = writer {
        let until = std::time::Instant::now() + HISTORY_FLUSH;
        while !w.is_finished() && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// What one workspace's [`drive`] hands to the next: whether the TUI has the mouse, the view
/// kept of the workspace switched to (when it was shown before), and the tabs.
#[derive(Default)]
struct Carried {
    mouse: bool,
    view: Option<workspace::SavedView>,
    tabs: Vec<workspace::Tab>,
}

/// Shows `session`'s hub, the workspace `current` (`None`: a TUI opened on a socket), until
/// the user quits or switches workspaces. Returns why, with the input history's writer
/// (its sender is closed: it ends once it wrote what it has).
async fn drive(
    terminal: &mut ratatui::DefaultTerminal,
    input: &mut Input,
    notifier: &mut notify::Notifier,
    ctx: &mut Shared,
    session: Session,
    current: Option<String>,
    carried: Carried,
) -> (anyhow::Result<End>, Option<std::thread::JoinHandle<()>>) {
    let Session {
        socket,
        calls,
        info,
        show,
        control,
        head,
    } = session;
    let epoch = ctx.epoch;
    let instance = info["instance"].as_str().map(str::to_string);
    // The input history of the instance connected to, beside its database.
    let history_file = info["db"]
        .as_str()
        .and_then(|db| Path::new(db).parent())
        .map(|dir| dir.join("tui-history"));
    let orch_runtimes: Vec<String> = show["runtimes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect();
    let orch_installed: Vec<(String, bool)> = orch_runtimes
        .iter()
        .filter_map(|r| show["installed"][r].as_bool().map(|i| (r.clone(), i)))
        .collect();
    let orch_default = show["runtime"]
        .as_str()
        .map(|r| (r.to_string(), show["model"].as_str().map(str::to_string)));
    let mut control: Option<ItemStream> = Some(control);

    // Hub calls run on their own task so a slow reply never blocks input or redraws.
    let (call_tx, call_rx) = mpsc::unbounded_channel::<(&'static str, Value)>();
    let (err_tx, mut err_rx) = mpsc::unbounded_channel::<StatusMsg>();
    // Results the view shows beyond the status bar (the trash list, what `/trash empty`
    // would purge).
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<(&'static str, Value)>();
    // A `twr ...` command shown here reaches this hub: through its socket when it was given
    // (no workspaces to switch), else in its workspace.
    let selection = workspace::attach_via(
        ctx.workspaces.is_none().then_some(socket.as_path()),
        info["workspace"].as_str(),
        std::env::var_os(twr_core::paths::WORKSPACE_ENV).as_deref(),
    );
    let status = StatusOut {
        tx: err_tx,
        late: current.clone().map(|w| (ctx.late_tx.clone(), w)),
        selection: selection.clone(),
    };
    let previous = current.as_ref().and_then(|w| ctx.callers.remove(w));
    // The workspace's followed results: what came for it while another one showed is
    // waiting there.
    let (followed_tx, mut followed_rx) = current
        .as_ref()
        .and_then(|w| ctx.followed.remove(w))
        .unwrap_or_else(mpsc::unbounded_channel);
    let results = ResultsOut {
        view: result_tx,
        followed: followed_tx.clone(),
    };
    let calls = HubCalls {
        socket: socket.clone(),
        client: Some(calls),
    };
    let caller = spawn_calls(previous, calls, call_rx, status.clone(), results);
    ctx.all_callers.push(caller.abort_handle());

    let current_name = current.clone();
    let mut state = AppState::new();
    state.instance_name = instance;
    // The boxes as the hub said them on connecting: the first frame's box tokens.
    state.apply_boxes(&info);
    state.attach_via = selection;
    state.view.orch_runtimes = orch_runtimes;
    state.view.orch_installed = orch_installed;
    state.view.orch_default = orch_default;
    state.view.mouse = carried.mouse;
    state.view.animate = ctx.tui.animate;
    state.view.card_order = ctx.tui.card_order;
    state.view.compact_width = ctx.tui.compact_width;
    state.view.compact = ctx.compact;
    state.view.suggest = ctx.suggest.unwrap_or(ctx.tui.suggest);
    state.view.suggest_model = ctx.tui.suggest_model.clone();
    state.view.picker = Picker::from_show(&show);
    state.workspace = current;
    state.set_tabs(carried.tabs);
    if let Some(view) = carried.view {
        state.restore_view(view);
    }
    state.view.shift_enter = ctx.keys.shift_enter;
    if let Some(notice) = ctx.keys.notice.take() {
        state.view.status = Some(notice.into());
    }
    // Another workspace's frame may be on screen: the first one starts from a clear screen.
    state.view.repaint = true;
    state.begin_catch_up(head);
    // The history file is written on a thread of its own, in order: a save waits for the
    // file's lock (another TUI may hold it), and the input loop never does.
    // (One JSON string per line, oldest first: [`load_history`].)
    let mut history_writer = None;
    let history = history_file.map(|p| {
        state.load_history(load_history(&p));
        let (tx, writer) = spawn_history_writer(p, status.clone());
        history_writer = writer;
        tx
    });
    let mut activity_poll = tokio::time::interval(ACTIVITY_EVERY);
    activity_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The other workspaces' tabs: asked now, then every [`TABS_EVERY`].
    let mut tab_poll = tokio::time::interval(TABS_EVERY);
    tab_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut tabs_pending = false;
    // This machine's load: read now, then every [`LOAD_EVERY`].
    let mut load_poll = tokio::time::interval(LOAD_EVERY);
    load_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut load_pending = false;
    // What the hub was told of the user's attention to this TUI (`notify.looking`), on a
    // connection of its own: a slow call (a thread being made) never holds it up. It is told
    // at once, and its connection closes when this view ends (the hub forgets it then).
    let mut attendance = notify::Attendance::default();
    let (looking_tx, looking_rx) = tokio::sync::watch::channel(None::<bool>);
    tokio::spawn(tell_attendance(socket.clone(), looking_rx));
    {
        let now = std::time::Instant::now();
        if let Some(looking) = attendance.step(control.is_some() && notifier.attended(now), now) {
            let _ = looking_tx.send(Some(looking));
        }
    }
    // Whether a box is low on disk, and how the remote boxes are: asked now, then every
    // [`INFO_EVERY`].
    let mut info_poll = tokio::time::interval(INFO_EVERY);
    info_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let (load_tx, mut load_rx) = mpsc::unbounded_channel::<Option<twr_core::load::Load>>();
    let (tabs_tx, mut tabs_rx) = mpsc::unbounded_channel::<Vec<workspace::Tab>>();
    // A workspace switched to, once its hub is connected ([`open_workspace`]).
    type Opened = (String, anyhow::Result<Session>);
    let (open_tx, mut open_rx) = mpsc::unbounded_channel::<Opened>();
    // What the last frame showed: another thread or screen repaints the whole terminal.
    let mut shown: Option<(Option<String>, app::Screen)> = None;
    let mut typed_ahead = TypedAhead::default();
    // The screen as last drawn while a selection is on: its text is copied from there.
    let mut screen = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::default());
    let mut thread_sub: Option<ItemStream> = None;
    let mut follow = Follow::default();
    // Transcript subscriptions are made off the input loop; each arrives tagged with its
    // thread and generation, and one that is not the last asked for ([`Follow`]) is dropped.
    type Subscribed = (String, u64, anyhow::Result<(ItemStream, u64, u64)>);
    let (sub_tx, mut sub_rx) = mpsc::unbounded_channel::<Subscribed>();
    // Fetches of attention text the control stream left capped (its blob fetch failed), run
    // off the input loop and tried again until they succeed (`AppState::hydration_due`).
    type Hydrated = (String, u64, Result<StreamItem, String>);
    let (hyd_tx, mut hyd_rx) = mpsc::unbounded_channel::<Hydrated>();
    // Suggestion calls' answers: each call on a connection of its own, as it takes a second
    // or more and nothing else waits for it.
    let (sug_tx, mut sug_rx) = mpsc::unbounded_channel::<Value>();
    // A replayed transcript event was the last thing applied: the next pass skips its draw.
    // The pane shows the transcript as the replay began until it is all applied
    // (`Transcript::visible`), so nothing on screen changed, and a draw per event would slow
    // a replay of thousands down.
    let mut replaying = false;
    // `TWR_TRACE_STARTUP`: what the last frame traced showed, and how many control and
    // transcript items were applied.
    let mut traced: Option<(Option<String>, bool, bool)> = None;
    let (mut control_items, mut transcript_items) = (0u64, 0u64);
    // When the replay's progress was last drawn ([`ui::render_loading`]).
    let mut loading_drawn: Option<std::time::Instant> = None;

    let end: anyhow::Result<End> = async {
        loop {
            // The control log is replayed first, from its start: the threads, orchestrators and
            // conversations of the past go by. Nothing is drawn or followed until it is all
            // applied, so the first frame is the state of now.
            let replaying_control = state.catch_up.is_some();
            // Follow the main pane: one per-thread subscription, for the orchestrator or the
            // thread dived into.
            let follows = (!replaying_control)
                .then(|| follow.follow(state.main_thread()))
                .flatten();
            if let Some(generation) = follows {
                thread_sub = None;
                if let Some(t) = follow.thread.clone().filter(|t| t != twr_core::event::HUB_THREAD) {
                    // A transcript kept from an earlier visit only needs what came after it.
                    let known = state.transcript_subscribing(&t);
                    let (socket, tx) = (socket.clone(), sub_tx.clone());
                    tokio::spawn(async move {
                        let r = subscribe_thread(&socket, &t, known, CONNECT_TIMEOUT).await;
                        let _ = tx.send((t, generation, r));
                    });
                } else {
                    // Nothing to subscribe to: nothing to wait for.
                    state.transcript_stalled();
                }
            }
            // While the control log is replayed, how far it is, now and then: the view is
            // not drawn until it is all applied, and no key acts until then.
            if replaying_control
                && loading_drawn.is_none_or(|at: std::time::Instant| at.elapsed() >= LOADING_EVERY)
            {
                terminal.draw(|f| ui::render_loading(f, &state))?;
                if loading_drawn.is_none() {
                    twr_core::trace::mark("tui: loading frame");
                }
                loading_drawn = Some(std::time::Instant::now());
            }
            if !std::mem::take(&mut replaying) && !replaying_control {
                // The grid's Shift-arrows need the column count the grid is drawn with, the
                // input box the height it may grow in.
                let size = terminal.size()?;
                (state.view.width, state.view.height) = (size.width, size.height);
                // A terminal that lost track of what is on it (a multiplexer, a width it
                // measures differently) keeps stray cells a diff never rewrites: a new view
                // starts from a clear screen.
                let now_showing = (state.main_thread(), state.view.screen);
                if std::mem::take(&mut state.view.repaint)
                    || shown.as_ref().is_some_and(|s| *s != now_showing)
                {
                    repaint(terminal)?;
                }
                // The cards' order as of now: what waited may be due.
                state.settle_order(std::time::Instant::now());
                state.view.frame = icon::frame_at(epoch.elapsed());
                let mut panes = select::Panes::default();
                let drawing = std::time::Instant::now();
                let drawn = terminal.draw(|f| panes = ui::render(f, &state))?;
                if twr_core::trace::enabled() {
                    let drew = drawing.elapsed().as_micros();
                    let now = (
                        state.main_thread(),
                        state.transcript.is_some(),
                        state.transcript_catching_up(),
                    );
                    if traced.as_ref() != Some(&now) {
                        twr_core::trace::mark(&format!(
                            "tui: frame (main {}, transcript {}, {} control items, {} transcript items, drawn in {drew}us)",
                            now.0.as_deref().unwrap_or("none"),
                            match (now.1, now.2) {
                                (false, _) => "none",
                                (true, true) => "loading",
                                (true, false) => "shown",
                            },
                            control_items,
                            transcript_items,
                        ));
                        traced = Some(now);
                    }
                }
                if state.view.selection.is_some() {
                    screen = drawn.buffer.clone();
                }
                state.note_panes(panes);
                // The selection's cells to leave out come from the draw its text is copied
                // from.
                if let Some(sel) = state.view.selection.as_mut() {
                    sel.refresh(&state.view.panes);
                }
                // What this frame drew in full is all an option key may answer.
                state.note_drawn(std::time::Instant::now());
                if shown.replace(now_showing).is_none() {
                    typed_ahead.start(std::time::Instant::now());
                }
            }
            for (attn, revision, raise) in state.hydration_due(std::time::Instant::now()) {
                let (socket, tx) = (socket.clone(), hyd_tx.clone());
                tokio::spawn(async move {
                    let r = within(HYDRATE_TIMEOUT, "fetching the full attention text", async {
                        let mut blobs = connect(&socket).await?;
                        Ok(hydrate_item(&raise, &mut blobs).await?.unwrap_or(raise))
                    })
                    .await
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send((attn, revision, r));
                });
            }
            // The main pane's thread's queue, read when it changed and while it moves.
            if let Some(thread) = state.queue_due(std::time::Instant::now()) {
                let _ = call_tx.send(("thread.queue", json!({ "thread": thread })));
            }
            // The output of `!` commands on the main pane's thread, and its folder.
            for job in state.shell_reads_due(std::time::Instant::now()) {
                let _ = call_tx.send((
                    "job.log",
                    json!({ "job": job, "lines": crate::shell::SHELL_READ_LINES }),
                ));
            }
            if state.shell_cwd_due() {
                let _ = call_tx.send(("shell.cd", json!({})));
            }
            if let Some(ask) = state.take_suggest_due() {
                let (socket, tx) = (socket.clone(), sug_tx.clone());
                tokio::spawn(async move {
                    let params = json!({"thread": ask.thread, "turn": ask.turn, "model": ask.model});
                    let r = async { connect(&socket).await?.call("suggest.next", params).await }.await;
                    let _ = tx.send(r.unwrap_or_else(|e| json!({"failed": true, "error": format!("{e:#}")})));
                });
            }
            // The next frame of a moving working icon; no timer when none was drawn.
            let tick = icon::next_tick(state.view.panes.animated, epoch, std::time::Instant::now());
            let retry = state
                .next_hydration()
                .map(tokio::time::Instant::from_std)
                .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));

            tokio::select! {
                key = input.rx.recv() => {
                    if let Some((at, Ok(event))) = &key {
                        match event {
                            TermEvent::FocusGained => notifier.focus(true),
                            TermEvent::FocusLost => {
                                notifier.focus(false);
                                // The pointer may have left with the focus: it holds nothing.
                                state.view.pointer = None;
                            }
                            TermEvent::Key(_) | TermEvent::Mouse(_) | TermEvent::Paste(_) => {
                                notifier.input(*at)
                            }
                            _ => {}
                        }
                        // The hub's notification command keeps quiet while the user attends to
                        // this: told at once when that changes.
                        if let Some(looking) = attendance.step(control.is_some() && notifier.attended(*at), *at) {
                            let _ = looking_tx.send(Some(looking));
                        }
                    }
                    if let Some((_, Ok(TermEvent::Key(k)))) = &key {
                        twr_core::trace::mark_with(|| format!("tui: key {:?}", k.code));
                    }
                    let actions = match key {
                        Some((at, Ok(event))) => {
                            let (ahead, status) = typed_ahead.drops(&event, at);
                            if let Some(status) = status {
                                state.view.status = Some(status.into());
                            }
                            let actions = handle_terminal_event(&mut state, event, &screen, shown.is_some() && !ahead);
                            ctx.keys.shift_enter |= state.view.shift_enter;
                            actions
                        }
                        Some((_, Err(e))) => return Err(anyhow::Error::new(e).context("reading terminal input")),
                        None => return Ok(End::Quit),
                    };
                    let added = std::mem::take(&mut state.view.history_new);
                    if let (Some(tx), false) = (&history, added.is_empty()) {
                        let _ = tx.send(added);
                    }
                    for action in actions {
                        match action {
                            Action::Quit => return Ok(End::Quit),
                            Action::Copy { text } => {
                                copy_text(text, status.clone());
                                continue;
                            }
                            Action::Mouse(on) => {
                                if let Err(e) = set_mouse_capture(on) {
                                    state.view.status = Some(format!("/mouse: {e}"));
                                }
                                continue;
                            }
                            // The workspace's hub is opened (started when needed) and
                            // connected to off the loop; this one stays until it is.
                            Action::Workspace { name, create } => {
                                if let Some(ws) = &ctx.workspaces {
                                    if !state.begin_switch(&name) {
                                        continue;
                                    }
                                    let (ws, tx) = (ws.clone(), open_tx.clone());
                                    tokio::spawn(async move {
                                        let r = open_workspace(ws.as_ref(), &name, create).await;
                                        let _ = tx.send((name, r));
                                    });
                                }
                                continue;
                            }
                            _ => {}
                        }
                        match action_request(&action) {
                            // A switch can wait minutes for the handoff summary, and a trim
                            // for a runtime to stop and its build output to go: each gets its
                            // own connection so other calls are not stuck behind it.
                            Some((method @ ("orchestrator.use" | "thread.trim"), params)) => {
                                let (socket, err_tx) = (socket.clone(), status.clone());
                                tokio::spawn(async move {
                                    let r = async {
                                        connect(&socket).await?.call(method, params).await
                                    }
                                    .await;
                                    let msg = match r {
                                        Ok(r) => result_status(method, &r).unwrap_or_default(),
                                        Err(e) => format!("{method}: {e:#}"),
                                    };
                                    err_tx.send((msg, false));
                                });
                            }
                            Some(req) => {
                                let _ = call_tx.send(req);
                            }
                            None => {}
                        }
                    }
                }
                item = next_opt(&mut control) => {
                    // What else came already is applied in the same pass, before a draw: a
                    // replay of thousands goes by in batches, not a pass of the loop each.
                    let mut next = Some(item);
                    let mut applied = 0;
                    while let Some(item) = next.take() {
                        match item {
                            Some(Ok(item)) => {
                                let replaying = state.catch_up.is_some();
                                apply_control(&mut state, notifier, &item, std::time::Instant::now());
                                control_items += 1;
                                if replaying && state.catch_up.is_none() {
                                    twr_core::trace::mark_with(|| {
                                        format!("tui: control replay applied ({control_items} items)")
                                    });
                                }
                            }
                            Some(Err(e)) => {
                                control_lost(&mut state, notifier, format!("control stream: {e:#}"));
                                control = None;
                                // What it can't show any more, the user doesn't see here.
                                if let Some(looking) = attendance.step(false, std::time::Instant::now()) {
                                    let _ = looking_tx.send(Some(looking));
                                }
                            }
                            None => {
                                control_lost(&mut state, notifier, "hub closed the control stream".into());
                                control = None;
                                if let Some(looking) = attendance.step(false, std::time::Instant::now()) {
                                    let _ = looking_tx.send(Some(looking));
                                }
                            }
                        }
                        applied += 1;
                        if applied < CONTROL_BATCH {
                            next = control.as_mut().and_then(|c| c.next().now_or_never());
                        }
                    }
                },
                item = next_opt(&mut thread_sub) => {
                    // Like the control stream's: what came already in the same pass.
                    let mut next = Some(item);
                    let mut applied = 0;
                    while let Some(item) = next.take() {
                        match item {
                            Some(Ok(item)) => {
                                state.apply_event(&item);
                                transcript_items += 1;
                                replaying = state.transcript_catching_up();
                            }
                            Some(Err(e)) => {
                                state.view.status = Some(format!("transcript: {e:#}"));
                                thread_sub = None;
                                state.transcript_stalled();
                                // What it settled on is drawn, though an item of this batch
                                // was still replayed.
                                replaying = false;
                            }
                            None => {
                                thread_sub = None;
                                state.transcript_stalled();
                                replaying = false;
                            }
                        }
                        applied += 1;
                        if applied < CONTROL_BATCH {
                            next = thread_sub.as_mut().and_then(|s| s.next().now_or_never());
                        }
                    }
                },
                Some((msg, transient)) = err_rx.recv() => {
                    state.set_status(msg, transient, std::time::Instant::now());
                }
                Some((method, result)) = result_rx.recv() => state.apply_result(method, &result),
                Some((method, result)) = followed_rx.recv() => state.apply_result(method, &result),
                // A transient status is due to clear.
                _ = sleep_opt(state.status_deadline()) => {
                    state.expire_status(std::time::Instant::now());
                }
                // A card move that waited is due, or the mouse's hold on the cards ended.
                _ = sleep_opt(state.order_deadline(std::time::Instant::now())) => {
                    state.settle_order(std::time::Instant::now());
                }
                // The queue is due to be read: loop to read it.
                _ = sleep_opt(state.queue_deadline(std::time::Instant::now())) => {}
                // A running command's output is due to be read: loop to read it.
                _ = sleep_opt(state.shell_deadline(std::time::Instant::now())) => {}
                // The double Esc's hint is due to go.
                _ = sleep_opt(state.unsend_deadline()) => {
                    state.expire_unsend(std::time::Instant::now());
                }
                Some(r) = sug_rx.recv() => state.apply_result("suggest.next", &r),
                Some((attn, revision, r)) = hyd_rx.recv() => {
                    state.hydrated(&attn, revision, r, std::time::Instant::now());
                }
                // A held notification is due: checked again, then sent with what else is due.
                _ = sleep_opt(notifier.next_due()) => {
                    let alert = notifier.flush(
                        &state.registry,
                        &state.attention,
                        std::time::Instant::now(),
                    );
                    if let Some(bytes) = alert {
                        use std::io::Write;
                        let mut out = std::io::stdout();
                        // A failed write shows in the status bar; nothing else depends on it.
                        if let Err(e) = out.write_all(bytes.as_bytes()).and_then(|()| out.flush()) {
                            state.view.status = Some(format!("notification: {e}"));
                        }
                    }
                }
                // A failed fetch is due again: loop to start it.
                _ = tokio::time::sleep_until(retry) => {}
                // The user's attention to this lapsed, or it is time to say it lasts.
                _ = sleep_opt(attendance.next_check(notifier)) => {
                    let now = std::time::Instant::now();
                    if let Some(looking) = attendance.step(control.is_some() && notifier.attended(now), now) {
                        let _ = looking_tx.send(Some(looking));
                    }
                }
                // The working icon's next frame: loop to draw it.
                _ = sleep_opt(tick) => {}
                _ = activity_poll.tick() => {
                    if !state.activity_pending {
                        state.activity_pending = true;
                        let _ = call_tx.send(("thread.activity", Value::Null));
                    }
                }
                // Read off the loop: a few small files, but a `sysctl` on macOS.
                _ = load_poll.tick(), if !load_pending => {
                    load_pending = true;
                    let tx = load_tx.clone();
                    tokio::task::spawn_blocking(move || {
                        let _ = tx.send(twr_core::load::Load::sample());
                    });
                }
                Some(load) = load_rx.recv() => {
                    load_pending = false;
                    state.load = load;
                }
                _ = info_poll.tick() => {
                    // (Recorded as sent now: its answer is older than a runner event after.)
                    if state.send_info() {
                        let _ = call_tx.send(("hub.info", Value::Null));
                    }
                }
                // The other workspaces' hubs, for their tabs.
                _ = tab_poll.tick(), if ctx.workspaces.is_some() && !tabs_pending => {
                    if let Some(ws) = ctx.workspaces.clone() {
                        tabs_pending = true;
                        let (current, tx) = (state.workspace.clone(), tabs_tx.clone());
                        tokio::spawn(async move {
                            let _ = tx.send(poll_tabs(ws.as_ref(), current.as_deref()).await);
                        });
                    }
                }
                Some(tabs) = tabs_rx.recv() => {
                    tabs_pending = false;
                    let alerts = workspace::waiting_alerts(
                        &mut ctx.waiting_seen,
                        &tabs,
                        state.workspace.as_deref(),
                        notifier,
                        std::time::Instant::now(),
                    );
                    for bytes in alerts {
                        use std::io::Write;
                        let mut out = std::io::stdout();
                        if let Err(e) = out.write_all(bytes.as_bytes()).and_then(|()| out.flush()) {
                            state.view.status = Some(format!("notification: {e}"));
                        }
                    }
                    state.set_tabs(tabs);
                }
                // An error of a call the workspace left had asked for.
                Some((msg, transient)) = ctx.late_rx.recv() => {
                    state.set_status(msg, transient, std::time::Instant::now());
                }
                Some((name, r)) = open_rx.recv() => match r {
                    Ok(next) => {
                        // What the user saw of this workspace's waiting items: no alert for
                        // them once it is in the background.
                        if let Some(current) = &state.workspace {
                            let hub = state.tab_hub(&workspace::Tab {
                                name: current.clone(),
                                hub: workspace::TabHub::Unknown,
                            });
                            if let workspace::TabHub::Running { permissions, questions, .. } = hub {
                                let waiting = notifier.waiting_count(permissions, questions);
                                ctx.waiting_seen.insert(current.clone(), waiting);
                            }
                        }
                        return Ok(End::Switch {
                            to: name,
                            next: Box::new(next),
                            view: Box::new(state.take_view()),
                            tabs: state.tabs.clone(),
                            mouse: state.view.mouse,
                        });
                    }
                    Err(e) => {
                        state.switching = None;
                        state.view.status = Some(format!("workspace {name}: {e:#}"));
                    }
                },
                Some((t, generation, r)) = sub_rx.recv() => {
                    if follow.current(&t, generation) {
                        match r {
                            Ok((s, since, head)) => {
                                twr_core::trace::mark_with(|| {
                                    format!("tui: transcript subscribed ({t} after {since}, head {head})")
                                });
                                transcript_items = 0;
                                if let Some(tr) = state.transcript.as_mut().filter(|tr| tr.thread == t) {
                                    tr.resume(since, head);
                                }
                                thread_sub = Some(s);
                            }
                            Err(e) => {
                                state.view.status = Some(format!("transcript: {e:#}"));
                                state.transcript_stalled();
                            }
                        }
                    }
                },
            }
        }
    }
    .await;

    // `/compact on|off` holds in the next workspace shown too.
    ctx.compact = state.view.compact;
    ctx.suggest = Some(state.view.suggest);
    // Leaving this hub: its attendance connection closes, and the hub forgets it.
    drop(looking_tx);
    drop(call_tx);
    let switching = matches!(end, Ok(End::Switch { .. }));
    end_calls(switching, caller, current_name.as_deref(), &mut ctx.callers);
    // What still comes of this workspace's messages waits for its next view.
    if let (true, Some(w)) = (switching, current_name.as_ref()) {
        ctx.followed.insert(w.clone(), (followed_tx, followed_rx));
    }
    // What this view's status bar had not shown yet goes to the next one's.
    if switching {
        forward_unread(&mut err_rx, &status.late).await;
    }
    // The writer ends once it wrote what it has.
    drop(history);
    (end, history_writer)
}

#[cfg(test)]
mod tests {
    /// A hub that records the calls it gets, fails `fail` (and a `message.withdraw` whose
    /// params say `fail`), answers `slow` once its oneshot fires, and a `message.withdraw`
    /// with its params.
    struct Recording(
        std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
        Option<tokio::sync::oneshot::Receiver<()>>,
    );

    impl super::CallHub for Recording {
        fn call(
            &mut self,
            method: &'static str,
            params: serde_json::Value,
        ) -> futures::future::BoxFuture<'_, anyhow::Result<serde_json::Value>> {
            self.0.lock().unwrap().push(method);
            let gate = (method == "slow").then(|| self.1.take()).flatten();
            Box::pin(async move {
                if let Some(gate) = gate {
                    let _ = gate.await;
                }
                match method {
                    "fail" => Err(anyhow::anyhow!("no such thread")),
                    "message.withdraw" if params["fail"] == true => {
                        Err(anyhow::anyhow!("connection lost"))
                    }
                    "message.withdraw" => Ok(params),
                    _ => Ok(serde_json::Value::Null),
                }
            })
        }
    }

    /// The view's calls connection after the hub stopped and another started (a restart, an
    /// upgrade): the prompt goes to the new hub, once, instead of "Broken pipe".
    #[tokio::test]
    async fn calls_reconnect_after_the_hub_restarted() {
        use super::CallHub;
        use twr_core::testkit::{HubScript, ScriptedHub};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = ScriptedHub::start(&socket, vec![HubScript::Stop]);
        let first = super::connect(&socket).await.unwrap();
        hub.hung_up().await;
        let mut calls = super::HubCalls {
            socket: socket.clone(),
            client: Some(first),
        };
        let params = json!({"thread": "w1", "prompt": "hi"});
        calls.call("thread.send", params.clone()).await.unwrap();
        calls.call("hub.info", Value::Null).await.unwrap();
        assert_eq!(
            hub.got(),
            [vec!["hello"], vec!["hello", "thread.send", "hub.info"]]
        );
    }

    /// A prompt the hub had when it went away (it may have been sent) is not sent again: the
    /// error says to check; the next call connects again.
    #[tokio::test]
    async fn a_call_the_hub_may_have_done_is_not_resent() {
        use super::CallHub;
        use twr_core::testkit::{HubScript, ScriptedHub};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = ScriptedHub::start(&socket, vec![HubScript::Crash]);
        let mut calls = super::HubCalls {
            socket: socket.clone(),
            client: Some(super::connect(&socket).await.unwrap()),
        };
        let params = json!({"thread": "w1", "prompt": "hi"});
        let e = calls.call("thread.send", params).await.unwrap_err();
        assert!(format!("{e:#}").contains("it may have been done"), "{e:#}");
        assert_eq!(hub.got(), [vec!["hello", "thread.send"]]);
        calls.call("hub.info", Value::Null).await.unwrap();
        assert_eq!(
            hub.got(),
            [vec!["hello", "thread.send"], vec!["hello", "hub.info"]]
        );
    }

    /// No hub to connect to again: the error says why the call failed and why connecting
    /// again did.
    #[tokio::test]
    async fn a_hub_that_stopped_for_good_is_said_so() {
        use super::CallHub;
        use twr_core::testkit::{HubScript, ScriptedHub};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("hub.sock");
        let hub = ScriptedHub::start(&socket, vec![HubScript::Stop]);
        let first = super::connect(&socket).await.unwrap();
        hub.hung_up().await;
        std::fs::remove_file(&socket).unwrap();
        let mut calls = super::HubCalls {
            socket: socket.clone(),
            client: Some(first),
        };
        let e = calls
            .call("thread.send", json!({"thread": "w1", "prompt": "hi"}))
            .await
            .unwrap_err();
        let shown = format!("{e:#}");
        assert!(
            shown.starts_with(
                "the hub closed the connection before thread.send reached it (nothing was \
                 done): the hub is stopping; is it running? `twr info` (connecting again: "
            ),
            "{shown}"
        );
    }

    /// Calls asked for before a switch are still sent once the view that asked is gone, and
    /// a failure among them is shown in the next view, naming its workspace.
    #[tokio::test]
    async fn calls_left_queued_by_a_switch_are_sent() {
        use tokio::sync::mpsc;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let (status_tx, status_rx) = mpsc::unbounded_channel();
        let results = super::ResultsOut {
            view: mpsc::unbounded_channel().0,
            followed: mpsc::unbounded_channel().0,
        };
        let (late_tx, mut late_rx) = mpsc::unbounded_channel();
        tx.send(("say", serde_json::Value::Null)).unwrap();
        tx.send(("fail", serde_json::Value::Null)).unwrap();
        // The switch: the view is gone, its sender closed.
        drop(tx);
        drop(status_rx);
        let status = super::StatusOut {
            tx: status_tx,
            late: Some((late_tx, "a".into())),
            selection: " -w a".into(),
        };
        super::serve_calls(Recording(seen.clone(), None), rx, status, results).await;
        assert_eq!(*seen.lock().unwrap(), ["say", "fail"]);
        let (msg, transient) = late_rx.try_recv().unwrap();
        assert!(msg.starts_with("workspace a: "), "{msg}");
        assert!(msg.contains("no such thread"), "{msg}");
        assert!(!transient);
    }

    /// The answers a view follows its messages with (`say`, `thread.send`,
    /// `message.withdraw`) go to its workspace's channel, which outlives the view: one that
    /// comes after a switch waits there for the workspace's next view. A failed one names the
    /// ref it was about.
    #[tokio::test]
    async fn followed_answers_outlive_their_view() {
        use tokio::sync::mpsc;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        let (view_tx, view_rx) = mpsc::unbounded_channel();
        let (followed_tx, mut followed_rx) = mpsc::unbounded_channel();
        tx.send((
            "message.withdraw",
            serde_json::json!({"thread": "t", "ref": 7}),
        ))
        .unwrap();
        tx.send((
            "message.withdraw",
            serde_json::json!({"thread": "t", "ref": 8, "fail": true}),
        ))
        .unwrap();
        // The switch: the view and its own results are gone.
        drop(tx);
        drop(view_rx);
        let status = super::StatusOut {
            tx: mpsc::unbounded_channel().0,
            late: None,
            selection: String::new(),
        };
        let results = super::ResultsOut {
            view: view_tx,
            followed: followed_tx,
        };
        super::serve_calls(Recording(seen, None), rx, status, results).await;
        let (method, r) = followed_rx.try_recv().unwrap();
        assert_eq!((method, r["ref"].as_i64()), ("message.withdraw", Some(7)));
        let (_, r) = followed_rx.try_recv().unwrap();
        assert_eq!(r, serde_json::json!({"failed": true, "ref": 8}));
    }

    /// A view left by a switch keeps sending its calls (it is not stopped), and the next view
    /// of the same workspace sends its own only after them: one workspace's calls go out in
    /// the order they were asked. Quitting stops them.
    #[tokio::test]
    async fn a_workspaces_calls_keep_their_order_across_switches() {
        use std::collections::HashMap;
        use tokio::sync::{mpsc, oneshot};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (release, blocked) = oneshot::channel::<()>();
        let status = |late: &mpsc::UnboundedSender<_>| super::StatusOut {
            tx: mpsc::unbounded_channel().0,
            late: Some((late.clone(), "a".into())),
            selection: String::new(),
        };
        let (late_tx, _late_rx) = mpsc::unbounded_channel();
        // The first view of a: a slow call, then P1 behind it.
        let (tx1, rx1) = mpsc::unbounded_channel();
        tx1.send(("slow", serde_json::Value::Null)).unwrap();
        tx1.send(("p1", serde_json::Value::Null)).unwrap();
        let first = super::spawn_calls(
            None,
            Recording(seen.clone(), Some(blocked)),
            rx1,
            status(&late_tx),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        // Switched away and back while the slow call runs.
        drop(tx1);
        let mut callers = HashMap::new();
        super::end_calls(true, first, Some("a"), &mut callers);
        let previous = callers.remove("a");
        assert!(
            previous.is_some(),
            "kept running, for the next view to wait for"
        );
        let (tx2, rx2) = mpsc::unbounded_channel();
        tx2.send(("p2", serde_json::Value::Null)).unwrap();
        drop(tx2);
        let second = super::spawn_calls(
            previous,
            Recording(seen.clone(), None),
            rx2,
            status(&late_tx),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(*seen.lock().unwrap(), ["slow"], "p2 waits for p1");
        release.send(()).unwrap();
        second.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), ["slow", "p1", "p2"]);
        // Quitting stops a view's calls.
        let (tx3, rx3) = mpsc::unbounded_channel::<(&'static str, serde_json::Value)>();
        let third = super::spawn_calls(
            None,
            Recording(seen.clone(), None),
            rx3,
            status(&late_tx),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        let handle = third.abort_handle();
        super::end_calls(false, third, Some("a"), &mut callers);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(callers.is_empty());
        assert!(
            handle.is_finished(),
            "stopped, though its sender is still open"
        );
        drop(tx3);
    }

    /// What the status bar of a view left by a switch had not shown goes to the next one,
    /// naming the workspace; what is transient does not.
    #[tokio::test]
    async fn unread_errors_go_to_the_next_view() {
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::unbounded_channel();
        tx.send(("say: no such thread".to_string(), false)).unwrap();
        tx.send(("sent to the orchestrator".to_string(), true))
            .unwrap();
        let (late_tx, mut late_rx) = mpsc::unbounded_channel();
        super::forward_unread(&mut rx, &Some((late_tx, "a".into()))).await;
        assert_eq!(
            late_rx.try_recv().unwrap(),
            ("workspace a: say: no such thread".to_string(), false)
        );
        assert!(late_rx.try_recv().is_err());
    }

    /// Once a view's status bar is gone, what its tasks say still reaches the next view: also
    /// what they say after its unread messages were handed over, and the history writer's
    /// failures. The commands a message quotes get the view's selection.
    #[tokio::test]
    async fn what_is_said_after_a_switch_reaches_the_next_view() {
        use tokio::sync::mpsc;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (late_tx, mut late_rx) = mpsc::unbounded_channel();
        let status = super::StatusOut {
            tx,
            late: Some((late_tx, "a".into())),
            selection: " -w a".into(),
        };
        status.send(("before (`twr trash list` shows them)".into(), false));
        super::forward_unread(&mut rx, &status.late).await;
        status.send(("after".into(), false));
        assert_eq!(
            late_rx.try_recv().unwrap().0,
            "workspace a: before (`twr -w a trash list` shows them)"
        );
        assert_eq!(late_rx.try_recv().unwrap().0, "workspace a: after");
        // The history writer, failing once its view is gone.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir").join("tui-history");
        std::fs::write(dir.path().join("not-a-dir"), "").unwrap();
        let (tx, writer) = super::spawn_history_writer(file, status.clone());
        tx.send(vec!["a line".into()]).unwrap();
        drop(tx);
        writer.unwrap().join().unwrap();
        let (msg, _) = late_rx.try_recv().unwrap();
        assert!(msg.starts_with("workspace a: input history: "), "{msg}");
    }

    /// A workspace `/ws` offered to create because the tabs didn't know it yet, made
    /// meanwhile: it is opened as it is.
    #[test]
    fn a_workspace_made_meanwhile_is_opened() {
        struct Made;
        impl super::Workspaces for Made {
            fn names(&self) -> anyhow::Result<Vec<String>> {
                Ok(vec!["default".into(), "a".into()])
            }
            fn socket(&self, _: &str) -> anyhow::Result<std::path::PathBuf> {
                unreachable!()
            }
            fn open(
                &self,
                _: &str,
            ) -> futures::future::BoxFuture<'static, anyhow::Result<std::path::PathBuf>>
            {
                unreachable!()
            }
            fn create(&self, name: &str) -> anyhow::Result<()> {
                anyhow::bail!("workspace {name} already exists")
            }
        }
        assert!(super::create_workspace(&Made, "a").is_ok());
        let e = super::create_workspace(&Made, "b").unwrap_err().to_string();
        assert!(e.contains("already exists"), "{e}");
    }

    /// Messages sent while the view hands its status bar over all arrive through the late
    /// channel, drained or sent after: none is lost between the two.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_message_is_lost_during_the_handover() {
        use tokio::sync::mpsc;
        for _ in 0..20 {
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (late_tx, mut late_rx) = mpsc::unbounded_channel();
            let status = super::StatusOut {
                tx,
                late: Some((late_tx, "a".into())),
                selection: String::new(),
            };
            let senders: Vec<_> = (0..4)
                .map(|n| {
                    let status = status.clone();
                    tokio::spawn(async move {
                        for i in 0..500 {
                            status.send((format!("{n}:{i}"), false));
                            if i % 50 == 0 {
                                tokio::task::yield_now().await;
                            }
                        }
                    })
                })
                .collect();
            tokio::task::yield_now().await;
            super::forward_unread(&mut rx, &status.late).await;
            for s in senders {
                s.await.unwrap();
            }
            drop(status);
            let mut got = 0;
            while late_rx.recv().await.is_some() {
                got += 1;
            }
            assert_eq!(got, 2000);
        }
    }

    /// Quitting stops every view's calls: a predecessor a later view waits for, and those of
    /// a workspace in the background.
    #[tokio::test]
    async fn quitting_stops_every_views_calls() {
        use tokio::sync::{mpsc, oneshot};
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (_release, blocked) = oneshot::channel::<()>();
        let status = || super::StatusOut {
            tx: mpsc::unbounded_channel().0,
            late: None,
            selection: String::new(),
        };
        let (tx1, rx1) = mpsc::unbounded_channel();
        tx1.send(("slow", serde_json::Value::Null)).unwrap();
        let first = super::spawn_calls(
            None,
            Recording(seen.clone(), Some(blocked)),
            rx1,
            status(),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        let first_handle = first.abort_handle();
        let (_tx2, rx2) = mpsc::unbounded_channel();
        let second = super::spawn_calls(
            Some(first),
            Recording(seen.clone(), None),
            rx2,
            status(),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        let (_tx3, rx3) = mpsc::unbounded_channel();
        let background = super::spawn_calls(
            None,
            Recording(seen.clone(), None),
            rx3,
            status(),
            super::ResultsOut {
                view: mpsc::unbounded_channel().0,
                followed: mpsc::unbounded_channel().0,
            },
        );
        let mut all = vec![
            first_handle.clone(),
            second.abort_handle(),
            background.abort_handle(),
        ];
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        super::abort_all(&mut all);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(first_handle.is_finished(), "the predecessor too");
        assert!(second.is_finished() && background.is_finished());
        drop(tx1);
    }

    /// Every connection to a workspace's socket checks the hub serves that workspace.
    #[tokio::test]
    async fn every_connection_checks_its_workspace() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("a.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let result = serde_json::json!({"client": "tui", "workspace": "b"});
                        let frame = format!(
                            "{}\n",
                            serde_json::json!({"id": req["id"], "result": result})
                        );
                        w.write_all(frame.as_bytes()).await.unwrap();
                    }
                });
            }
        });
        assert!(super::connect(&sock).await.is_ok(), "nothing expected yet");
        super::expect_workspace(&sock, "a");
        let e = super::connect(&sock).await.unwrap_err().to_string();
        assert!(e.contains("serves workspace b, not a"), "{e}");
    }

    /// The `[tui]` settings the CLI read win over the environment's config.
    #[test]
    fn the_given_tui_settings_are_used() {
        let given = twr_core::config::TuiConfig {
            mouse: false,
            ..Default::default()
        };
        assert_eq!(super::tui_config(Some(given.clone())), given);
    }

    use super::*;

    /// A notifier that rings for the default events, focused or not.
    fn bell() -> notify::Notifier {
        use twr_core::config::{NotifyWhen, TerminalAlert, TuiNotifyConfig};
        let cfg = TuiNotifyConfig {
            terminal: TerminalAlert::Bell,
            when: NotifyWhen::Always,
            events: twr_core::notify::NotifyEvent::DEFAULTS.to_vec(),
        };
        notify::Notifier::new(&cfg, notify::Multiplexer::None)
    }

    fn control_item(gseq: u64, event: &twr_core::event::Event) -> StreamItem {
        StreamItem {
            stream: twr_core::proto::StreamKind::Control,
            gseq: Some(gseq),
            event: event.clone(),
        }
    }

    #[test]
    fn the_replay_never_notifies_but_a_reply_it_left_running_does() {
        use twr_core::event::{AttentionReason, Grade, Source, ThreadCreated, TurnResult};
        use twr_core::testkit::ThreadScript;

        let t0 = std::time::Instant::now();
        let mut n = bell();
        let mut state = AppState::new();
        let mut o = ThreadScript::new("o1");
        let mut api = ThreadScript::new("t1");
        let mut said = o.turn_started("what is api doing?");
        said.source = Source::User("tui".into());
        let past = [
            o.event(EventBody::ThreadCreated(ThreadCreated {
                name: "orch".into(),
                ext: json!({"twr": {"role": "orchestrator"}})
                    .as_object()
                    .cloned(),
                ..Default::default()
            })),
            api.created("api"),
            api.turn_started("go"),
            api.raise("p1", AttentionReason::Permission),
            api.turn_ended(TurnResult::Completed),
            api.report(Some(1), Grade::Success),
            said,
        ];
        state.begin_catch_up(past.len() as u64);
        for (g, ev) in (1..).zip(&past) {
            apply_control(&mut state, &mut n, &control_item(g, ev), t0);
        }
        assert!(state.catch_up.is_none());
        assert_eq!(n.next_due(), None, "the past is no news");
        // A duplicate is no news either.
        apply_control(&mut state, &mut n, &control_item(4, &past[3]), t0);
        assert_eq!(n.next_due(), None);
        // The orchestrator replies to the message sent before the TUI opened.
        let ended = o.turn_ended(TurnResult::Completed);
        apply_control(&mut state, &mut n, &control_item(8, &ended), t0);
        let at = t0 + Duration::from_secs(2);
        assert_eq!(
            n.flush(&state.registry, &state.attention, at).as_deref(),
            Some("\x07")
        );
    }

    #[test]
    fn a_lost_control_stream_drops_what_was_held() {
        use twr_core::event::AttentionReason;
        use twr_core::testkit::ThreadScript;

        let t0 = std::time::Instant::now();
        let mut n = bell();
        let mut state = AppState::new();
        state.begin_catch_up(0);
        let mut api = ThreadScript::new("t1");
        for (g, ev) in (1..).zip([
            api.created("api"),
            api.turn_started("go"),
            api.raise("p1", AttentionReason::Permission),
        ]) {
            apply_control(&mut state, &mut n, &control_item(g, &ev), t0);
        }
        assert!(n.next_due().is_some());
        control_lost(&mut state, &mut n, "hub closed the control stream".into());
        assert_eq!(n.next_due(), None);
        let at = t0 + Duration::from_secs(2);
        assert_eq!(n.flush(&state.registry, &state.attention, at), None);
        assert_eq!(
            state.view.status.as_deref(),
            Some("hub closed the control stream")
        );
    }

    #[test]
    fn startup_input_cannot_purge_trash_before_replay_and_a_draw() {
        use app::tests::key;
        use crossterm::event::KeyCode;
        use ratatui::{backend::TestBackend, buffer::Buffer, layout::Rect, Terminal};

        let screen = Buffer::empty(Rect::default());
        let chosen = json!({
            "dry_run": true,
            "size": "1 KiB",
            "items": [{"item": "trash-1", "short": "trash-1", "thread_name": "api"}],
        });
        // Neither replay completion alone nor a draw alone makes input safe.
        for (head, has_drawn) in [(1, false), (0, false), (1, true)] {
            for pasted in [true, false] {
                let mut state = AppState::new();
                state.begin_catch_up(head);
                let mut events = if pasted {
                    vec![TermEvent::Paste("/trash empty --all".into())]
                } else {
                    "/trash empty --all"
                        .chars()
                        .map(|c| TermEvent::Key(key(KeyCode::Char(c))))
                        .collect()
                };
                events.extend([
                    TermEvent::Key(key(KeyCode::Enter)),
                    TermEvent::Key(key(KeyCode::Char('y'))),
                ]);
                let mut requests = Vec::new();
                for event in events {
                    for action in handle_terminal_event(&mut state, event, &screen, has_drawn) {
                        if let Some((method, params)) = action_request(&action) {
                            // Reproduce the hub returning a confirmation before the next key.
                            if method == "trash.empty" && params["dry_run"] == true {
                                state.apply_result(method, &chosen);
                            }
                            requests.push((method, params));
                        }
                    }
                }
                assert!(
                    requests.is_empty(),
                    "commands ran before a frame: {requests:?}"
                );
                assert!(
                    state.view.input.is_empty(),
                    "startup input must be discarded"
                );
                assert!(state.view.history_new.is_empty());
                assert!(state.view.confirm.is_none());
                assert!(state.view.action.is_none(), "no popup opened");
            }
        }

        // Once replay and a real draw complete, the same command works normally and its
        // confirmation is visible before `y` can purge the listed item.
        let mut state = AppState::new();
        state.begin_catch_up(0);
        (state.view.width, state.view.height) = (120, 30);
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|f| {
                ui::render(f, &state);
            })
            .unwrap();
        assert!(handle_terminal_event(
            &mut state,
            TermEvent::Paste("/trash empty --all".into()),
            &screen,
            true
        )
        .is_empty());
        let actions = handle_terminal_event(
            &mut state,
            TermEvent::Key(key(KeyCode::Enter)),
            &screen,
            true,
        );
        assert_eq!(
            actions,
            [Action::TrashChoose {
                items: Vec::new(),
                all: true
            }]
        );
        state.apply_result("trash.empty", &chosen);
        terminal
            .draw(|f| {
                ui::render(f, &state);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("purge trash-1 (api), 1 KiB for good"),
            "{text}"
        );
        assert_eq!(
            handle_terminal_event(
                &mut state,
                TermEvent::Key(key(KeyCode::Char('y'))),
                &screen,
                true
            ),
            [Action::TrashEmpty {
                items: vec!["trash-1".into()]
            }]
        );
    }

    /// Before the first frame only Ctrl-C quits: a typed `q` is a letter and is dropped like
    /// the rest. Key releases do nothing.
    #[test]
    fn ctrl_c_works_before_the_first_frame_but_q_and_key_releases_do_not() {
        use app::tests::{ctrl, key};
        use crossterm::event::{KeyCode, KeyEventKind};
        use ratatui::{buffer::Buffer, layout::Rect};

        let screen = Buffer::empty(Rect::default());
        for head in [0, 1] {
            for (quit, quits) in [
                (key(KeyCode::Char('q')), false),
                (ctrl('c'), true),
                (ctrl('q'), false),
            ] {
                let mut state = AppState::new();
                state.begin_catch_up(head);
                let mut release = quit;
                release.kind = KeyEventKind::Release;
                assert!(
                    handle_terminal_event(&mut state, TermEvent::Key(release), &screen, false)
                        .is_empty()
                );
                let want: &[Action] = if quits { &[Action::Quit] } else { &[] };
                assert_eq!(
                    handle_terminal_event(&mut state, TermEvent::Key(quit), &screen, false),
                    want,
                    "{quit:?}"
                );
                assert!(state.view.input.is_empty());
            }
        }
    }

    /// Feeds `events`, read at the given times, through `typed_ahead` and the state as the
    /// loop does; returns what was sent.
    fn feed(
        state: &mut AppState,
        typed_ahead: &mut TypedAhead,
        events: Vec<(std::time::Instant, TermEvent)>,
    ) -> Vec<Action> {
        let screen = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::default());
        let mut actions = Vec::new();
        for (at, event) in events {
            let (ahead, status) = typed_ahead.drops(&event, at);
            if let Some(status) = status {
                state.view.status = Some(status.into());
            }
            actions.extend(handle_terminal_event(state, event, &screen, !ahead));
        }
        actions
    }

    /// The keys of `line` and an Enter, read `gap` apart from `from` on.
    fn typed_at(
        line: &str,
        from: std::time::Instant,
        gap: Duration,
    ) -> Vec<(std::time::Instant, TermEvent)> {
        use app::tests::key;
        use crossterm::event::KeyCode;
        line.chars()
            .map(|c| TermEvent::Key(key(KeyCode::Char(c))))
            .chain([TermEvent::Key(key(KeyCode::Enter))])
            .enumerate()
            .map(|(i, e)| (from + gap * i as u32, e))
            .collect()
    }

    /// Keys read before the first frame, or as fast as they were typed ahead right after it
    /// (a slow reader handing them over 100 ms apart), are dropped with their Enter: neither
    /// `/trash empty --all`, Enter, `y` nor a line typed straight through the start sends
    /// anything, not even its last words. Read before the frame means dropped however late
    /// the loop gets to it (a long draw in between). After a quiet [`TYPED_AHEAD_QUIET`] a line
    /// goes out, once the line a `y` typed ahead left open has been dropped.
    #[test]
    fn input_typed_ahead_of_the_first_frame_is_dropped_until_it_goes_quiet() {
        use app::tests::key;
        use crossterm::event::KeyCode;
        use std::time::Instant;

        let t0 = Instant::now();
        let before = t0 - Duration::from_millis(500);
        let mut waiting = typed_at("/trash empty --all", before, Duration::ZERO);
        waiting.push((before, TermEvent::Key(key(KeyCode::Char('y')))));
        let pasted = vec![
            (before, TermEvent::Paste("/trash empty --all".into())),
            (before, TermEvent::Key(key(KeyCode::Enter))),
            (before, TermEvent::Key(key(KeyCode::Char('y')))),
        ];
        let step = Duration::from_millis(100);
        let through = typed_at("please do not allow api", t0 + step, step);
        for ahead in [waiting, pasted, through] {
            let mut state = AppState::new();
            let mut typed_ahead = TypedAhead::default();
            typed_ahead.start(t0);
            let last = ahead.last().unwrap().0.max(t0);
            // A `y` typed ahead began a line of its own.
            let open =
                !matches!(&ahead.last().unwrap().1, TermEvent::Key(k) if k.code == KeyCode::Enter);
            assert_eq!(
                feed(&mut state, &mut typed_ahead, ahead),
                [],
                "nothing sent"
            );
            assert!(state.view.input.is_empty() && state.view.history_new.is_empty());
            assert!(state.view.confirm.is_none(), "nothing to confirm");
            assert_eq!(state.view.status, None);
            let mut from = last + TYPED_AHEAD_QUIET;
            if open {
                let rest = typed_at("/trash empty --all", from, step);
                from = rest.last().unwrap().0 + step;
                assert_eq!(feed(&mut state, &mut typed_ahead, rest), []);
                assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_DROPPED));
            }
            let after = typed_at("/trash empty --all", from, step);
            assert_eq!(
                feed(&mut state, &mut typed_ahead, after),
                [Action::TrashChoose {
                    items: Vec::new(),
                    all: true
                }]
            );
            assert_eq!(typed_ahead, TypedAhead::Off);
        }
    }

    /// A line typed ahead with a pause in it is still dropped whole: once the window goes
    /// quiet in the middle of it, its rest is dropped up to its Enter, and the status bar says
    /// why. Then keys count again.
    #[test]
    fn a_line_typed_ahead_is_dropped_whole_across_a_pause() {
        use std::time::Instant;

        let t0 = Instant::now();
        let step = Duration::from_millis(50);
        let mut state = AppState::new();
        let mut typed_ahead = TypedAhead::default();
        typed_ahead.start(t0);
        let mut events = typed_at("please do not", t0, step);
        events.pop(); // no Enter yet
        let resume = events.last().unwrap().0 + Duration::from_millis(400);
        let rest = typed_at("allow api", resume, step);
        let (rest, last) = rest.split_at(rest.len() - 1);
        events.extend(rest.iter().cloned());
        assert_eq!(feed(&mut state, &mut typed_ahead, events), []);
        assert_eq!(typed_ahead, TypedAhead::Line);
        assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_LINE));
        assert!(state.view.input.is_empty());
        assert_eq!(feed(&mut state, &mut typed_ahead, last.to_vec()), []);
        assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_DROPPED));
        assert!(state.view.input.is_empty(), "the Enter dropped too");
        let after = typed_at("/trash empty --all", last[0].0 + step, step);
        assert_eq!(
            feed(&mut state, &mut typed_ahead, after),
            [Action::TrashChoose {
                items: Vec::new(),
                all: true
            }]
        );
    }

    /// Esc does not end a line typed ahead: it never throws away what was typed, so typing
    /// goes on in the same line after it, and the rest of that line is dropped with it, up
    /// to its Enter.
    #[test]
    fn esc_does_not_end_a_line_typed_ahead() {
        use app::tests::key;
        use crossterm::event::KeyCode;
        use std::time::Instant;

        let t0 = Instant::now();
        let step = Duration::from_millis(50);
        let mut state = AppState::new();
        let mut typed_ahead = TypedAhead::default();
        typed_ahead.start(t0);
        let mut events = typed_at("please do not", t0, step);
        events.pop(); // no Enter yet
        let resume = events.last().unwrap().0 + Duration::from_millis(400);
        events.push((resume, TermEvent::Key(key(KeyCode::Esc))));
        let rest = typed_at("allow api", resume + step, step);
        let (rest, last) = rest.split_at(rest.len() - 1);
        events.extend(rest.iter().cloned());
        assert_eq!(feed(&mut state, &mut typed_ahead, events), []);
        assert_eq!(typed_ahead, TypedAhead::Line, "the Esc ended nothing");
        assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_LINE));
        assert!(state.view.input.is_empty(), "\"allow api\" dropped too");
        assert_eq!(feed(&mut state, &mut typed_ahead, last.to_vec()), []);
        assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_DROPPED));
        assert_eq!(typed_ahead, TypedAhead::Off);
    }

    /// A line break (Shift-Enter, Alt-Enter) in a line typed ahead does not end it: the rest
    /// is dropped up to the Enter that would have sent it.
    #[test]
    fn a_line_break_typed_ahead_does_not_end_the_line() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::time::Instant;

        let t0 = Instant::now();
        let ms = Duration::from_millis;
        for m in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut typed_ahead = TypedAhead::Window {
                last: t0,
                line: true,
            };
            let a = TermEvent::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
            let brk = TermEvent::Key(KeyEvent::new(KeyCode::Enter, m));
            let enter = TermEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            // In the window: still a line after the break.
            assert_eq!(typed_ahead.drops(&brk, t0 + ms(10)), (true, None));
            assert_eq!(
                typed_ahead,
                TypedAhead::Window {
                    last: t0 + ms(10),
                    line: true
                },
                "{m:?}"
            );
            // After a pause: the break is dropped with the rest, up to the Enter.
            assert_eq!(
                typed_ahead.drops(&a, t0 + ms(400)),
                (true, Some(TYPED_AHEAD_LINE))
            );
            assert_eq!(
                typed_ahead.drops(&brk, t0 + ms(410)),
                (true, Some(TYPED_AHEAD_LINE))
            );
            assert_eq!(typed_ahead, TypedAhead::Line, "{m:?}");
            assert_eq!(
                typed_ahead.drops(&enter, t0 + ms(420)),
                (true, Some(TYPED_AHEAD_DROPPED))
            );
            assert_eq!(typed_ahead, TypedAhead::Off);
        }
    }

    /// A line begun before the first frame, while the replay went by, is dropped whole too:
    /// its rest typed after the frame and a pause is dropped up to its Enter. One finished
    /// before the frame leaves nothing to drop.
    #[test]
    fn a_line_begun_before_the_first_frame_is_dropped_whole() {
        use std::time::Instant;

        let step = Duration::from_millis(50);
        let t0 = Instant::now();
        for finished in [false, true] {
            let mut state = AppState::new();
            let mut typed_ahead = TypedAhead::default();
            let mut before = typed_at("please do not", t0 - Duration::from_secs(1), step);
            if !finished {
                before.pop();
            }
            assert_eq!(feed(&mut state, &mut typed_ahead, before), []);
            assert_eq!(typed_ahead, TypedAhead::Blank { line: !finished });
            typed_ahead.start(t0);
            let rest = typed_at("allow api", t0 + Duration::from_millis(300), step);
            let sent = feed(&mut state, &mut typed_ahead, rest);
            if finished {
                assert!(matches!(sent.as_slice(), [Action::Say { .. }]), "{sent:?}");
            } else {
                assert_eq!(sent, []);
                assert_eq!(state.view.status.as_deref(), Some(TYPED_AHEAD_DROPPED));
                assert_eq!(typed_ahead, TypedAhead::Off);
            }
        }
    }

    /// Dropping the input stops and joins its thread: nothing reads the terminal afterwards,
    /// so keys typed once it is the user's again stay for the shell.
    #[test]
    fn the_input_thread_stops_reading_when_dropped() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let reads = Arc::new(AtomicUsize::new(0));
        let r = reads.clone();
        let input = Input::spawn(move |timeout| {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(timeout.min(Duration::from_millis(2)));
            Ok(None)
        })
        .unwrap();
        while reads.load(Ordering::SeqCst) < 2 {
            std::thread::sleep(Duration::from_millis(1));
        }
        drop(input);
        let n = reads.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(reads.load(Ordering::SeqCst), n, "read after the drop");
    }

    /// Events come over with the time they were read; a read error is the last of them.
    #[test]
    fn the_input_thread_stamps_events_and_ends_after_an_error() {
        use app::tests::key;
        use crossterm::event::KeyCode;
        use std::collections::VecDeque;

        let mut queue: VecDeque<std::io::Result<Option<TermEvent>>> = VecDeque::from([
            Ok(Some(TermEvent::Key(key(KeyCode::Char('a'))))),
            Ok(None),
            Ok(Some(TermEvent::Paste("b".into()))),
            Err(std::io::Error::other("gone")),
        ]);
        let before = std::time::Instant::now();
        let mut input = Input::spawn(move |_| queue.pop_front().unwrap_or(Ok(None))).unwrap();
        let (a, ev) = input.rx.blocking_recv().unwrap();
        assert!(matches!(ev, Ok(TermEvent::Key(_))));
        let (b, ev) = input.rx.blocking_recv().unwrap();
        assert!(matches!(ev, Ok(TermEvent::Paste(p)) if p == "b"));
        assert!(before <= a && a <= b);
        assert!(input.rx.blocking_recv().unwrap().1.is_err());
        assert!(input.rx.blocking_recv().is_none(), "the thread ended");
    }

    /// Only keys and pastes extend the window or make a line: the mouse moving inside the
    /// window is dropped without keeping it open, and while the rest of a line is dropped a
    /// click still works.
    #[test]
    fn only_keys_and_pastes_extend_the_typed_ahead_window() {
        use app::tests::key;
        use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
        use std::time::Instant;

        let moved = TermEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        let k = TermEvent::Key(key(KeyCode::Char('k')));
        let ms = Duration::from_millis;
        let mut typed_ahead = TypedAhead::default();
        let t0 = Instant::now();
        typed_ahead.start(t0);
        assert_eq!(typed_ahead.drops(&moved, t0 + ms(100)), (true, None));
        assert_eq!(typed_ahead.drops(&k, t0 + TYPED_AHEAD_QUIET), (false, None));
        assert_eq!(typed_ahead, TypedAhead::Off, "for good");

        typed_ahead.start(t0);
        assert_eq!(typed_ahead.drops(&k, t0), (true, None));
        let later = t0 + ms(400);
        assert_eq!(typed_ahead.drops(&moved, later), (false, None));
        assert_eq!(typed_ahead, TypedAhead::Line);
        assert_eq!(typed_ahead.drops(&k, later), (true, Some(TYPED_AHEAD_LINE)));
    }

    /// Key releases type nothing: they neither make nor end a line, nor extend the window,
    /// before the first frame, in the window or in the rest of a line.
    #[test]
    fn key_releases_leave_the_typed_ahead_state_alone() {
        use app::tests::key;
        use crossterm::event::{KeyCode, KeyEventKind};
        use std::time::Instant;

        let release = |code| {
            let mut k = key(code);
            k.kind = KeyEventKind::Release;
            TermEvent::Key(k)
        };
        let ms = Duration::from_millis;
        let t0 = Instant::now();
        // Blank: an Enter's release does not finish a line, a letter's does not begin one.
        for line in [false, true] {
            let mut typed_ahead = TypedAhead::Blank { line };
            for code in [KeyCode::Enter, KeyCode::Esc, KeyCode::Char('a')] {
                assert_eq!(typed_ahead.drops(&release(code), t0), (true, None));
                assert_eq!(typed_ahead, TypedAhead::Blank { line });
            }
        }
        // Window: dropped, but the window and the line stay as they were.
        for line in [false, true] {
            let mut typed_ahead = TypedAhead::Window { last: t0, line };
            for code in [KeyCode::Enter, KeyCode::Char('a')] {
                assert_eq!(
                    typed_ahead.drops(&release(code), t0 + ms(100)),
                    (true, None)
                );
                assert_eq!(typed_ahead, TypedAhead::Window { last: t0, line });
            }
        }
        // Line: a release passes (the key handlers ignore it) and does not end the line.
        let mut typed_ahead = TypedAhead::Line;
        assert_eq!(
            typed_ahead.drops(&release(KeyCode::Enter), t0),
            (false, None)
        );
        assert_eq!(typed_ahead, TypedAhead::Line);
        // A finished line stays finished through its last key's release.
        let mut typed_ahead = TypedAhead::Window {
            last: t0,
            line: true,
        };
        let enter = TermEvent::Key(key(KeyCode::Enter));
        assert_eq!(typed_ahead.drops(&enter, t0 + ms(10)), (true, None));
        assert_eq!(
            typed_ahead.drops(&release(KeyCode::Char('i')), t0 + ms(20)),
            (true, None)
        );
        assert_eq!(
            typed_ahead.drops(&release(KeyCode::Char('x')), t0 + ms(400)),
            (false, None)
        );
        assert_eq!(typed_ahead, TypedAhead::Off);
    }

    /// A Ctrl-C typed ahead still quits, also in the rest of a line; `q` is dropped like any
    /// letter.
    #[test]
    fn quitting_typed_ahead_still_quits() {
        use app::tests::{ctrl, key};
        use crossterm::event::KeyCode;
        use std::time::Instant;

        let t0 = Instant::now();
        for (quit, quits) in [(ctrl('c'), true), (key(KeyCode::Char('q')), false)] {
            for line in [false, true] {
                let mut state = AppState::new();
                let mut typed_ahead = TypedAhead::default();
                typed_ahead.start(t0);
                let at = if line {
                    typed_ahead = TypedAhead::Line;
                    t0 + Duration::from_secs(1)
                } else {
                    t0
                };
                let want: &[Action] = if quits { &[Action::Quit] } else { &[] };
                assert_eq!(
                    feed(
                        &mut state,
                        &mut typed_ahead,
                        vec![(at, TermEvent::Key(quit))]
                    ),
                    want,
                    "{quit:?} {line}"
                );
                assert!(state.view.input.is_empty());
            }
        }
    }

    #[test]
    fn a_repaint_asks_the_terminal_nothing_and_redraws_everything() {
        use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
        use ratatui::buffer::Cell;
        use ratatui::layout::{Position, Size};
        /// A terminal that never says where its cursor is (its answer went to the key
        /// stream), counting what was drawn.
        struct Mute(TestBackend, usize);
        impl Backend for Mute {
            type Error = std::io::Error;
            fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
            where
                I: Iterator<Item = (u16, u16, &'a Cell)>,
            {
                let cells: Vec<_> = content.collect();
                self.1 += cells.len();
                self.0
                    .draw(cells.into_iter())
                    .map_err(std::io::Error::other)
            }
            fn hide_cursor(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn show_cursor(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn get_cursor_position(&mut self) -> std::io::Result<Position> {
                Err(std::io::Error::other(
                    "the cursor position could not be read",
                ))
            }
            fn set_cursor_position<P: Into<Position>>(&mut self, p: P) -> std::io::Result<()> {
                self.0.set_cursor_position(p).map_err(std::io::Error::other)
            }
            fn clear(&mut self) -> std::io::Result<()> {
                self.0.clear().map_err(std::io::Error::other)
            }
            fn clear_region(&mut self, t: ClearType) -> std::io::Result<()> {
                self.0.clear_region(t).map_err(std::io::Error::other)
            }
            fn size(&self) -> std::io::Result<Size> {
                self.0.size().map_err(std::io::Error::other)
            }
            fn window_size(&mut self) -> std::io::Result<WindowSize> {
                self.0.window_size().map_err(std::io::Error::other)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut t = ratatui::Terminal::new(Mute(TestBackend::new(10, 2), 0)).unwrap();
        let fill = |f: &mut ratatui::Frame| {
            f.render_widget(ratatui::widgets::Paragraph::new("hello"), f.area());
        };
        t.draw(fill).unwrap();
        t.draw(fill).unwrap();
        let before = t.backend().1;
        repaint(&mut t).unwrap();
        t.draw(fill).unwrap();
        assert_eq!(
            t.backend().1 - before,
            5,
            "every drawn cell is written again"
        );
        assert!(
            t.clear().is_err(),
            "ratatui's own clear would have stopped the TUI"
        );
    }

    #[test]
    fn the_input_history_file_round_trips_and_stays_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("tui-history");
        assert!(load_history(&path).is_empty());
        save_history(&path, &["a \"quoted\" line".into(), "/help".into()]).unwrap();
        assert_eq!(load_history(&path), ["a \"quoted\" line", "/help"]);
        let many: Vec<String> = (0..2 * app::HISTORY_MAX).map(|i| i.to_string()).collect();
        save_history(&path, &many).unwrap();
        let kept = load_history(&path);
        assert_eq!(kept.len(), app::HISTORY_MAX);
        assert_eq!(kept.last().map(String::as_str), Some("999"));
        // A torn line (not UTF-8) costs only itself; a huge entry is not kept.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b"\"\xC3\n\"torn").unwrap();
        drop(f);
        save_history(&path, &["after".into(), "x".repeat(HISTORY_ENTRY_MAX + 1)]).unwrap();
        let kept = load_history(&path);
        assert_eq!(kept.len(), app::HISTORY_MAX + 1);
        assert_eq!(kept.last().map(String::as_str), Some("after"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "private: {mode:o}");
    }

    #[test]
    fn new_thread_paths_are_absolute_and_names_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let abs = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(project_param("api"), "api");
        assert_eq!(
            project_param(&format!("{}/.", dir.path().display())),
            abs.to_string_lossy()
        );
        let cwd = std::fs::canonicalize(".").unwrap();
        assert_eq!(project_param("."), cwd.to_string_lossy());
        if let Some(home) = std::env::var_os("HOME") {
            let home = std::fs::canonicalize(home).unwrap();
            assert_eq!(project_param("~"), home.to_string_lossy());
        }
        // A path that does not exist is sent as typed (the hub says what is wrong).
        assert_eq!(project_param("./nope/x"), "./nope/x");
    }

    /// `Delete` to a delete card a thread of which was deleted and restored since: the status
    /// bar says nothing was deleted, and why; any other answer says nothing.
    #[test]
    fn a_changed_delete_card_says_nothing_went() {
        let ev = json!({"kind": "attention.resolved", "data": {"attn_id": "c1",
            "answer": {"decision": "Delete"},
            "ext": {"twr": {"changed": ["a was deleted and restored since (now b)"]}}}});
        assert_eq!(
            result_status("attention.respond", &ev).as_deref(),
            Some("nothing deleted: a was deleted and restored since (now b)")
        );
        let plain = json!({"kind": "attention.resolved", "data": {"attn_id": "c1"}});
        assert!(result_status("attention.respond", &plain).is_none());
    }

    #[test]
    fn requests_match_the_socket_api() {
        let (m, p) = action_request(&Action::Respond {
            attn_id: "a1".into(),
            revision: 7,
            decision: "allow".into(),
        })
        .unwrap();
        assert_eq!(m, "attention.respond");
        assert_eq!(
            p,
            json!({"attn_id": "a1", "revision": 7, "answer": {"decision": "allow", "message": null}})
        );
        // An unknown revision (0) is left out: the hub then answers only an item never raised
        // again.
        let (_, p) = action_request(&Action::Respond {
            attn_id: "a1".into(),
            revision: 0,
            decision: "allow".into(),
        })
        .unwrap();
        assert!(p.get("revision").is_none(), "{p}");
        let (m, p) = action_request(&Action::Send {
            thread: "t1".into(),
            text: "hi".into(),
        })
        .unwrap();
        assert_eq!(m, "thread.send");
        assert_eq!(p, json!({"thread": "t1", "prompt": "hi"}));
        let (m, p) = action_request(&Action::Say {
            text: "yes".into(),
            seen_gseq: 42,
        })
        .unwrap();
        assert_eq!(m, "say");
        // Always to the orchestrator, with the control position on screen.
        assert_eq!(
            p,
            json!({"message": "yes", "seen_gseq": 42, "to": "orchestrator"})
        );
        let (_, p) = action_request(&Action::Say {
            text: "deploy".into(),
            seen_gseq: 0,
        })
        .unwrap();
        assert_eq!(
            p,
            json!({"message": "deploy", "seen_gseq": 0, "to": "orchestrator"})
        );
        assert_eq!(
            result_status("say", &json!({"action": "send", "thread_name": "api"})).as_deref(),
            Some("sent to api")
        );
        assert!(result_status("thread.send", &json!({})).is_none());
        let (m, p) = action_request(&Action::UseOrchestrator {
            runtime: "codex".into(),
            model: Some("o5".into()),
            summary: true,
        })
        .unwrap();
        assert_eq!(
            (m, p),
            (
                "orchestrator.use",
                json!({"runtime": "codex", "model": "o5"})
            )
        );
        let (_, p) = action_request(&Action::UseOrchestrator {
            runtime: "grok-build".into(),
            model: None,
            summary: false,
        })
        .unwrap();
        assert_eq!(p, json!({"runtime": "grok-build", "summary": false}));
        assert_eq!(
            result_status("orchestrator.use", &json!({"runtime": "codex"})).as_deref(),
            Some("the orchestrator is now codex (a new session)")
        );
        assert_eq!(
            result_status(
                "orchestrator.use",
                &json!({"runtime": "codex", "handoff": {"summary": true}})
            )
            .as_deref(),
            Some("the orchestrator is now codex (a new session; handed over with the previous one's summary)")
        );
        assert_eq!(
            result_status(
                "orchestrator.use",
                &json!({"runtime": "codex", "handoff": {"summary": false,
                        "summary_note": "the previous orchestrator was not running"}})
            )
            .as_deref(),
            Some("the orchestrator is now codex (a new session; handed over without a summary (the previous orchestrator was not running))")
        );
        let (m, p) = action_request(&Action::NewThread {
            project: "api".into(),
            runtime: Some("codex".into()),
            name: Some("api2".into()),
        })
        .unwrap();
        assert_eq!(
            (m, p),
            (
                "thread.new",
                json!({"project": "api", "runtime": "codex", "name": "api2"})
            )
        );
        assert_eq!(
            result_status("thread.new", &json!({"name": "api2", "runtime": "codex"})).as_deref(),
            Some("created api2 (runtime codex)")
        );
        // The hub's advisory (the box reports the CLI missing) follows what was done.
        let w = "box gpu reports no `codex` on its PATH; the thread may fail to start: run `twr \
                 box doctor gpu`";
        assert_eq!(
            result_status(
                "thread.new",
                &json!({"name": "api2", "runtime": "codex", "warning": w})
            ),
            Some(format!("created api2 (runtime codex); warning: {w}"))
        );
        assert_eq!(
            result_status(
                "thread.set_model",
                &json!({"thread_name": "api2", "model": "o3", "applies": "next_start",
                        "warning": w})
            ),
            Some(format!(
                "api2 now has model o3, effort default; from its next start; warning: {w}"
            ))
        );
        // A mode change that restarts the runtime, and a send that starts it: also when the
        // send itself says nothing (it started a turn).
        assert_eq!(
            result_status(
                "thread.set_mode",
                &json!({"thread_name": "api2", "mode": "auto", "previous": "manual",
                        "changed": true, "applies": "now", "warning": w})
            )
            .unwrap(),
            format!(
                "{}; warning: {w}",
                result_status(
                    "thread.set_mode",
                    &json!({"thread_name": "api2", "mode": "auto", "previous": "manual",
                            "changed": true, "applies": "now"})
                )
                .unwrap()
            )
        );
        assert_eq!(
            result_status(
                "thread.send",
                &json!({"thread": "t1", "turn": 1, "warning": w})
            ),
            Some(format!("warning: {w}"))
        );
        assert_eq!(
            result_status("thread.send", &json!({"thread": "t1", "turn": 1})),
            None
        );
        // Only on those results.
        assert_eq!(
            result_status("thread.stop", &json!({"warning": w})).as_deref(),
            Some("stopped")
        );
        let (m, p) = action_request(&Action::Stop {
            thread: "t1".into(),
        })
        .unwrap();
        assert_eq!(m, "thread.stop");
        assert_eq!(p, json!({"thread": "t1"}));
        let (m, p) = action_request(&Action::Park {
            thread: "t1".into(),
        })
        .unwrap();
        assert_eq!((m, p), ("thread.park", json!({"thread": "t1"})));
        let (m, p) = action_request(&Action::Trim {
            thread: "t1".into(),
        })
        .unwrap();
        assert_eq!((m, p), ("thread.trim", json!({"thread": "t1"})));
        assert_eq!(
            result_status(
                "thread.trim",
                &json!({"thread_name": "api", "deleted": ["target/"], "freed": 2048})
            )
            .as_deref(),
            Some("trimmed api: 2 KB freed")
        );
        assert_eq!(
            result_status("thread.trim", &json!({"thread_name": "api", "deleted": []})).as_deref(),
            Some("api: no build output to trim")
        );
        assert_eq!(
            result_status(
                "thread.trim",
                &json!({"thread_name": "api", "deleted": ["target/"], "freed": 2048,
                        "notes": ["its build output target/ stays at .twr-build-output.0/0"]})
            )
            .as_deref(),
            Some(
                "trimmed api: 2 KB freed; its build output target/ stays at .twr-build-output.0/0"
            )
        );
        let (m, p) = action_request(&Action::Resume {
            thread: "t1".into(),
        })
        .unwrap();
        assert_eq!((m, p), ("thread.resume", json!({"thread": "t1"})));
        assert_eq!(
            result_status("thread.park", &json!({"kept_dirty": true})).as_deref(),
            Some("parked; worktree kept (uncommitted changes)")
        );
        assert_eq!(
            result_status("thread.resume", &json!({})).as_deref(),
            Some("resumed")
        );
        // A stop says whether the runtime is still stopping, and why it waits.
        let stop = |r: Value| result_status("thread.stop", &r);
        assert_eq!(
            stop(json!({"stopping": false, "dropped_prompts": 0})).as_deref(),
            Some("stopped")
        );
        assert_eq!(
            stop(json!({"stopping": true, "dropped_prompts": 2})).as_deref(),
            Some("stopping; dropped 2 queued prompt(s)")
        );
        assert_eq!(
            stop(json!({"stopping": true, "box_offline": true, "dropped_prompts": 0})).as_deref(),
            Some("stopping once its box's runner reconnects (box offline)")
        );
        assert!(action_request(&Action::Quit).is_none());
        assert_eq!(
            error_status(
                "say",
                "conflict: no orchestrator is available (runtime \"codex\" is not available)"
            ),
            "no orchestrator can run: nothing was sent. Switch it with o or /orch"
        );
        assert_eq!(
            error_status("thread.stop", "not_found: x"),
            "thread.stop: not_found: x"
        );
        // `say` to the orchestrator, and `thread.send` in a dive: queued when busy.
        assert_eq!(
            result_status(
                "say",
                &json!({"action": "delegate", "thread": "t-o", "turn": 3, "pending": 0})
            )
            .as_deref(),
            Some("sent to the orchestrator")
        );
        assert_eq!(
            result_status(
                "say",
                &json!({"action": "delegate", "thread": "t-o", "turn": null, "pending": 1})
            )
            .as_deref(),
            Some("to the orchestrator: queued: it is busy and reads this next")
        );
        // What a message did is transient; what could not start, an answer, a new thread or
        // a "which thread?" is not, nor any other method's status.
        for (method, r, transient) in [
            ("say", json!({"action": "delegate", "turn": 3}), true),
            (
                "say",
                json!({"action": "delegate", "turn": null, "pending": 1}),
                true,
            ),
            ("say", json!({"action": "send", "thread_name": "web"}), true),
            (
                "say",
                json!({"action": "command", "applied": ["api: allow"]}),
                true,
            ),
            (
                "say",
                json!({"action": "delegate", "dispatch_error": "boom"}),
                false,
            ),
            (
                "say",
                json!({"action": "respond", "answer": "allow"}),
                false,
            ),
            ("say", json!({"action": "new_thread"}), false),
            ("say", json!({"action": "ask"}), false),
            ("thread.send", json!({"turn": null, "pending": 2}), false),
        ] {
            assert_eq!(transient_status(method, &r), transient, "{method} {r}");
        }
        assert_eq!(
            result_status(
                "thread.send",
                &json!({"thread": "t1", "turn": 2, "pending": 0})
            ),
            None
        );
        assert_eq!(
            result_status(
                "thread.send",
                &json!({"thread": "t1", "turn": null, "pending": 2})
            )
            .as_deref(),
            Some("queued: it is busy and reads this next")
        );
        // `thread.set_mode`: what changed and when the runtime picks it up.
        let set = |r: Value| result_status("thread.set_mode", &r);
        assert_eq!(
            set(json!({"thread": "t1", "thread_name": "web", "mode": "auto",
                       "previous": "manual", "changed": true, "applies": "after_turn"}))
            .as_deref(),
            Some("web is now in mode auto; its runtime restarts in it after the current turn")
        );
        assert_eq!(
            set(json!({"thread": "t1", "thread_name": "web", "mode": "auto",
                       "changed": false, "message": "web is already in auto mode"}))
            .as_deref(),
            Some("web is already in mode auto")
        );
        assert_eq!(
            set(
                json!({"thread_name": "web", "mode": "plan", "changed": true,
                       "applies": "now", "dispatch_error": "boom"})
            )
            .as_deref(),
            Some(
                "web is now in mode plan; its runtime restarts in it now; restarting failed: boom"
            )
        );
    }

    /// `thread.set_model`: the model and effort now (null: `default`), and when the runtime
    /// takes them.
    #[test]
    fn set_model_results_say_what_and_when() {
        let set = |r: Value| result_status("thread.set_model", &r);
        assert_eq!(
            set(
                json!({"thread": "t1", "thread_name": "web", "model": "claude-fable-5-1",
                       "effort": "high", "previous_model": null, "previous_effort": null,
                       "changed": true, "applies": "after_turn"})
            )
            .as_deref(),
            Some(
                "web now has model claude-fable-5-1, effort high; its runtime restarts with \
                 them after the current turn"
            )
        );
        assert_eq!(
            set(json!({"thread_name": "web", "model": null, "effort": "low",
                       "changed": true, "applies": "now", "dispatch_error": "boom"}))
            .as_deref(),
            Some(
                "web now has model default, effort low; its runtime restarts with them now; \
                 restarting failed: boom"
            )
        );
        assert_eq!(
            set(
                json!({"thread_name": "web", "model": "opus", "effort": null,
                       "changed": true, "applies": "next_start"})
            )
            .as_deref(),
            Some("web now has model opus, effort default; from its next start")
        );
        assert_eq!(
            set(
                json!({"thread_name": "web", "model": "opus", "effort": null,
                       "changed": false, "message": "web already runs with model opus"})
            )
            .as_deref(),
            Some("web already has model opus, effort default")
        );
        // A move to another runtime: the hub's message, and a turn that finishes first.
        let moved = json!({"thread_name": "web", "runtime": "codex",
                           "previous_runtime": "claude-code", "runtime_changed": true,
                           "model": "gpt-6.1-sol", "effort": "xhigh", "changed": true,
                           "applies": "after_turn",
                           "message": "web moves from claude-code to codex: a new codex session \
                                       starts with its next turn"});
        assert_eq!(
            set(moved.clone()).as_deref(),
            Some(
                "web moves from claude-code to codex: a new codex session starts with its next \
                 turn; the current turn finishes on the previous runtime first"
            )
        );
        let mut bare = moved;
        bare.as_object_mut().unwrap().remove("message");
        bare["applies"] = json!("next_start");
        bare["dispatch_error"] = json!("boom");
        assert_eq!(
            set(bare).as_deref(),
            Some(
                "web moves to codex with model gpt-6.1-sol, effort xhigh; dispatching after the \
                 move failed: boom"
            )
        );
    }

    /// `thread.set_model` carries the runtime only when one is given; `thread.delete` the
    /// thread, and its result says the card waits for the user.
    #[test]
    fn runtime_moves_and_deletes_are_requested_and_reported() {
        let (m, p) = action_request(&Action::SetModel {
            thread: "t1".into(),
            model: None,
            effort: Some("xhigh".into()),
            runtime: Some("codex".into()),
        })
        .unwrap();
        assert_eq!(
            (m, p),
            (
                "thread.set_model",
                json!({"thread": "t1", "effort": "xhigh", "runtime": "codex"})
            )
        );
        let (m, p) = action_request(&Action::Delete {
            threads: vec!["t1".into(), "t2".into()],
        })
        .unwrap();
        assert_eq!((m, p), ("thread.delete", json!({"threads": ["t1", "t2"]})));
        let card = |existing: bool| {
            result_status(
                "thread.delete",
                &json!({"status": "needs_confirmation", "thread": "t1", "thread_name": "web",
                        "threads": [{"thread": "t1", "thread_name": "web", "pending": false}],
                        "attn_id": "a1", "revision": 7, "existing": existing,
                        "options": ["Delete", "Cancel"]}),
            )
        };
        assert_eq!(
            card(false).as_deref(),
            Some(
                "the delete card for web is waiting for you; nothing is deleted until you \
                 answer Delete on it"
            )
        );
        assert_eq!(
            card(true).as_deref(),
            Some(
                "the delete card for web is already waiting for you; nothing is deleted until \
                 you answer Delete on it"
            )
        );
        // Several threads on one card: all named.
        let names = |n: &[&str]| {
            let threads: Vec<Value> = n
                .iter()
                .map(|t| json!({"thread": "x", "thread_name": t}))
                .collect();
            result_status(
                "thread.delete",
                &json!({"status": "needs_confirmation", "thread": "x", "thread_name": n[0],
                        "threads": threads, "existing": false}),
            )
        };
        assert_eq!(
            names(&["web", "api", "docs"]).as_deref(),
            Some(
                "the delete card for web, api and docs is waiting for you; nothing is deleted \
                 until you answer Delete on it"
            )
        );
        assert!(names(&["web", "api"])
            .unwrap()
            .starts_with("the delete card for web and api is"));
    }

    /// [`read_ahead`] passes every item on, in order, its error last; and dropping what it
    /// returns drops the stream it reads at once, though that one waits for an item that
    /// never comes (an idle thread's transcript: its subscription must not outlive the view).
    #[tokio::test]
    async fn read_ahead_keeps_the_order_and_lets_go_of_its_stream() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use twr_core::testkit::ThreadScript;
        let mut t = ThreadScript::new("t1");
        let items: Vec<anyhow::Result<StreamItem>> = (0..2000)
            .map(|i| {
                Ok(StreamItem {
                    stream: twr_core::proto::StreamKind::Thread,
                    gseq: None,
                    event: t.text(&format!("line {i}")),
                })
            })
            .chain(std::iter::once(Err(anyhow::anyhow!("lost"))))
            .collect();
        let read: Vec<_> = read_ahead(Box::pin(futures::stream::iter(items)))
            .collect()
            .await;
        assert_eq!(read.len(), 2001);
        for (i, item) in read[..2000].iter().enumerate() {
            assert_eq!(item.as_ref().unwrap().event.seq, i as u64 + 1);
        }
        assert!(read[2000].is_err());

        /// Says when it is dropped.
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let gone = Arc::new(AtomicBool::new(false));
        let guard = Dropped(gone.clone());
        let first = Ok(StreamItem {
            stream: twr_core::proto::StreamKind::Thread,
            gseq: None,
            event: t.text("only this"),
        });
        let idle = futures::stream::once(async move { first })
            .chain(futures::stream::pending())
            .map(move |item| {
                let _held = &guard;
                item
            });
        let mut s = read_ahead(Box::pin(idle));
        assert!(s.next().await.unwrap().is_ok());
        assert!(!gone.load(Ordering::SeqCst));
        drop(s);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !gone.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the stream read is dropped with the one returned");
    }

    #[tokio::test]
    async fn capped_control_items_are_hydrated_before_they_are_applied() {
        use std::sync::Arc;
        use twr_core::blob::{cap_body, BlobStore, MemoryBlobStore};
        use twr_core::event::{AttentionRaised, AttentionReason, Event, Source, ThreadCreated};
        use twr_core::proto::StreamKind;

        let store = Arc::new(MemoryBlobStore::new());
        let options: Vec<String> = (0..8).map(|i| format!("{i}{}", "o".repeat(599))).collect();
        let raised = EventBody::AttentionRaised(AttentionRaised {
            attn_id: "q1".into(),
            reason: AttentionReason::Question,
            summary: "Pick one?".into(),
            options: options.clone(),
            ..Default::default()
        });
        let capped = cap_body(&raised, store.as_ref()).unwrap();
        let item = |gseq: u64, body: EventBody, stream: StreamKind| StreamItem {
            stream,
            gseq: Some(gseq),
            event: Event::new("t1", gseq, Some(1), Source::Adapter("fake".into()), body),
        };
        let created = item(
            1,
            EventBody::ThreadCreated(ThreadCreated {
                name: "one".into(),
                ..Default::default()
            }),
            StreamKind::Control,
        );
        let raise = item(2, capped.clone(), StreamKind::Control);
        let mut fetch = {
            let store = Arc::clone(&store);
            move |r: String| {
                let store = Arc::clone(&store);
                async move { store.get(&r) }
            }
        };
        // Uncapped items need no fetch. Thread items restore their fields too.
        assert!(hydrate_item(&created, &mut fetch).await.unwrap().is_none());
        let thread_item = item(2, capped, StreamKind::Thread);
        assert_eq!(
            hydrate_item(&thread_item, &mut fetch)
                .await
                .unwrap()
                .unwrap()
                .event
                .body,
            raised
        );

        // Applied as is, the question has no options and cannot be answered.
        let mut st = AppState::new();
        st.apply_event(&created);
        st.apply_event(&raise);
        assert!(st.top_attention("t1").unwrap().options.is_empty());

        let full = hydrate_item(&raise, &mut fetch).await.unwrap().unwrap();
        assert_eq!(full.event.body, raised);
        let mut st = AppState::new();
        st.apply_event(&created);
        st.apply_event(&full);
        assert_eq!(st.top_attention("t1").unwrap().options, options);
        // Eight options of 600 characters fit neither on a card nor in the action popup: no
        // option key answers what the user could not read in full.
        st.set_focus(Some("t1".into()));
        crate::app::tests::popup_open(&mut st);
        let actions = st.handle_key(crate::app::tests::key(crossterm::event::KeyCode::Char('3')));
        assert!(actions.is_empty(), "{actions:?}");
        assert_eq!(st.view.status.as_deref(), Some(crate::app::NOT_ON_SCREEN));
    }

    #[tokio::test]
    async fn a_large_report_survives_streaming_finalization_and_replay() {
        use crate::app::tests::{thread_item, with_orch, without_worker_requests};
        use std::sync::Arc;
        use twr_core::blob::{cap_body, BlobStore, MemoryBlobStore, PREVIEW_MAX};
        use twr_core::event::AgentText;
        use twr_core::testkit::ThreadScript;

        let report = format!(
            "<thread name=\"alpha\">\n```\n{}\n```\n</thread>",
            (0..100)
                .map(|i| format!("line {i:03}: {}", "worker report ".repeat(14)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(report.len() > 16_000);
        let store = Arc::new(MemoryBlobStore::new());
        let mut script = ThreadScript::new("t-orch");
        script.at(500);
        let mut st = without_worker_requests(with_orch());
        st.transcript.as_mut().unwrap().settle();
        st.view.width = 80;
        let delta = script.event(EventBody::AgentText(AgentText {
            text: report.clone(),
            delta: true,
            ..Default::default()
        }));
        st.apply_event(&thread_item(&delta));
        let before = ui::block_rows(&st);
        let key = before
            .iter()
            .find_map(|r| r.2.as_deref().and_then(ui::linked_block))
            .unwrap()
            .to_string();
        st.view.long_expanded.insert(key.clone());
        let full_rows = ui::block_rows(&st);
        let final_body = EventBody::AgentText(AgentText {
            text: report.clone(),
            is_final: true,
            ..Default::default()
        });
        let final_event = script.event(cap_body(&final_body, store.as_ref()).unwrap());
        let capped = thread_item(&final_event);
        let EventBody::AgentText(p) = &capped.event.body else {
            panic!()
        };
        assert_eq!(p.text.len(), PREVIEW_MAX);
        let mut fetch = {
            let store = Arc::clone(&store);
            move |r: String| {
                let store = Arc::clone(&store);
                async move { store.get(&r) }
            }
        };
        let hydrated = hydrate_item(&capped, &mut fetch).await.unwrap().unwrap();
        assert_eq!(hydrated.event.body, final_body);
        st.apply_event(&hydrated);
        assert_eq!(ui::block_rows(&st), full_rows);
        assert!(st.view.long_expanded.contains(&key));
        st.view.long_expanded.clear();
        assert_eq!(ui::block_rows(&st), before);

        // Replaying only the final event still provides the entire report for expansion.
        let mut replay = without_worker_requests(with_orch());
        replay.transcript.as_mut().unwrap().settle();
        replay.apply_event(&hydrated);
        let entry = replay.transcript.as_ref().unwrap().entries.last().unwrap();
        assert!(matches!(entry, app::Entry::Text { text, open: false, .. } if text == &report));

        // A failed fetch keeps the streamed text instead of replacing it with the preview.
        let mut failed = without_worker_requests(with_orch());
        failed.transcript.as_mut().unwrap().settle();
        failed.apply_event(&thread_item(&delta));
        failed.apply_event(&capped);
        assert!(
            matches!(&failed.transcript.as_ref().unwrap().entries[0], app::Entry::Text { text, open: false, .. } if text == &report)
        );
        assert!(
            matches!(failed.transcript.as_ref().unwrap().entries.last(), Some(app::Entry::Notice { text, .. }) if text.contains("could not load"))
        );
    }

    /// A fetch cut off by its time limit in the middle of the hub's reply leaves the rest of
    /// that reply on its connection: the next item's fetch connects again rather than read it
    /// as its own reply, and gets its full text.
    #[tokio::test]
    async fn a_fetch_cut_off_mid_reply_does_not_spoil_the_next_one() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use twr_core::blob::{cap_body, BlobStore, MemoryBlobStore};
        use twr_core::event::AgentText;
        use twr_core::proto::StreamKind;
        use twr_core::testkit::ThreadScript;

        let store = Arc::new(MemoryBlobStore::new());
        let mut script = ThreadScript::new("t-orch");
        let finals: Vec<(EventBody, StreamItem)> = (0..2)
            .map(|n| {
                let body = EventBody::AgentText(AgentText {
                    text: format!("{n}{}", "x".repeat(20_000)),
                    is_final: true,
                    ..Default::default()
                });
                let event = script.event(cap_body(&body, store.as_ref()).unwrap());
                assert!(has_capped(&event.body.data().unwrap()));
                let item = StreamItem {
                    stream: StreamKind::Thread,
                    gseq: None,
                    event,
                };
                (body, item)
            })
            .collect();
        // A hub that sends half of its first `blob.get` reply, the rest only after a while.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hub.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let cut = Arc::new(AtomicBool::new(true));
        let blobs = Arc::clone(&store);
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (store, cut) = (Arc::clone(&blobs), Arc::clone(&cut));
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let get = req["method"] == "blob.get";
                        let result = if get {
                            let r = req["params"]["ref"].as_str().unwrap();
                            let data = String::from_utf8(store.get(r).unwrap().unwrap()).unwrap();
                            json!({"data": data, "encoding": "utf8", "size": data.len(), "eof": true})
                        } else {
                            json!({})
                        };
                        let frame = format!("{}\n", json!({"id": req["id"], "result": result}));
                        if get && cut.swap(false, Ordering::SeqCst) {
                            let half = frame.len() / 2;
                            w.write_all(&frame.as_bytes()[..half]).await.unwrap();
                            tokio::time::sleep(Duration::from_millis(600)).await;
                            let _ = w.write_all(&frame.as_bytes()[half..]).await;
                        } else {
                            w.write_all(frame.as_bytes()).await.unwrap();
                        }
                    }
                });
            }
        });
        let client = connect(&sock).await.unwrap();
        let items: Vec<anyhow::Result<StreamItem>> =
            finals.iter().map(|(_, item)| Ok(item.clone())).collect();
        let mut out = hydrating(
            Box::pin(futures::stream::iter(items)),
            client,
            &sock,
            Duration::from_millis(200),
        );
        // Cut off: as it came, its preview.
        let first = out.next().await.unwrap().unwrap();
        assert_eq!(first.event.body, finals[0].1.event.body);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let second = out.next().await.unwrap().unwrap();
        assert_eq!(second.event.body, finals[1].0);
    }

    /// Every connection the TUI opens starts with `hello {client: "tui"}`.
    #[tokio::test]
    async fn connections_introduce_themselves() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hub.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<Value>>();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let tx = tx.clone();
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = BufReader::new(r).lines();
                    let mut seen = Vec::new();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: Value = serde_json::from_str(&line).unwrap();
                        let result = match req["method"].as_str() {
                            Some("thread.subscribe") => json!({"thread": "t1", "head": 5000}),
                            _ => json!({}),
                        };
                        let resp = json!({"id": req["id"], "result": result});
                        seen.push(req);
                        w.write_all(format!("{resp}\n").as_bytes()).await.unwrap();
                    }
                    let _ = tx.send(seen);
                });
            }
        });
        let mut c = connect(&sock).await.unwrap();
        c.call("thread.list", Value::Null).await.unwrap();
        drop(c);
        let (sub, since, head) = subscribe_thread(&sock, "t1", 0, CONNECT_TIMEOUT)
            .await
            .unwrap();
        drop(sub);
        // The transcript replays the thread's last TRANSCRIPT_TAIL events only.
        assert_eq!((since, head), (5000 - TRANSCRIPT_TAIL, 5000));
        let conns = [
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
        ];
        for seen in &conns {
            assert_eq!(seen[0]["method"], "hello");
            assert_eq!(
                seen[0]["params"],
                json!({"client": "tui", "protocol": twr_core::proto::PROTOCOL_VERSION})
            );
        }
        let requests: Vec<(&str, Value)> = conns
            .iter()
            .filter(|seen| seen.len() > 1) // The blob client has only introduced itself.
            .map(|seen| {
                (
                    seen[1]["method"].as_str().unwrap(),
                    seen[1]["params"]["since_seq"].clone(),
                )
            })
            .collect();
        assert!(requests.contains(&("thread.list", Value::Null)));
        assert!(requests.contains(&("thread.subscribe", json!(PROBE_SINCE))));
        assert!(requests.contains(&("thread.subscribe", json!(5000 - TRANSCRIPT_TAIL))));

        // A transcript kept from an earlier visit resumes right after its last seq.
        let (sub, since, _) = subscribe_thread(&sock, "t1", 4990, CONNECT_TIMEOUT)
            .await
            .unwrap();
        drop(sub);
        assert_eq!(since, 4990);
        let mut subs: Vec<_> = vec![
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
            rx.recv().await.unwrap(),
        ]
        .into_iter()
        .filter(|seen| seen.len() > 1)
        .collect();
        subs.sort_by_key(|seen| seen[1]["params"]["since_seq"].as_u64());
        assert_eq!(subs[0][1]["params"]["since_seq"], json!(4990));
        assert_eq!(subs[1][1]["params"]["since_seq"], json!(PROBE_SINCE));
    }

    /// Review: an A → B → A switch has two subscriptions for A in flight, and replies were
    /// told apart by thread only. The older reply, arriving last with its head behind what
    /// the newer stream had applied, read as a shorter log and emptied A's transcript, and its
    /// stream starts after that head, so the history never came back. Only the reply to the
    /// last request is taken.
    #[test]
    fn only_the_last_subscription_asked_for_is_taken() {
        use crate::app::Transcript;
        use twr_core::testkit::ThreadScript;

        let mut follow = Follow::default();
        let a_old = follow.follow(Some("a".into())).unwrap();
        assert_eq!(
            follow.follow(Some("a".into())),
            None,
            "same thread: no new request"
        );
        let b = follow.follow(Some("b".into())).unwrap();
        let a_new = follow.follow(Some("a".into())).unwrap();
        assert!(!follow.current("a", a_old));
        assert!(!follow.current("b", b));
        assert!(follow.current("a", a_new));
        assert_eq!(follow.follow(None), Some(a_new + 1));
        assert!(!follow.current("a", a_new));

        // Why: A's transcript ends at `kept` when the old request is made; the new one's
        // stream then applies the next event. The old reply (head `kept`) would drop the
        // history for good: its stream starts after `kept`.
        let mut s = ThreadScript::new("a");
        let evs = [s.turn_started("go"), s.text("one"), s.text("two")];
        let kept = evs[1].seq;
        let mut tr = Transcript::new("a");
        tr.resume(0, kept);
        tr.apply(&evs[0]);
        tr.apply(&evs[1]);
        tr.resume(kept, evs[2].seq);
        tr.apply(&evs[2]);
        assert_eq!(tr.entries.len(), 3);
        let mut stale = tr.clone();
        stale.resume(kept, kept);
        assert_eq!(
            stale.entries.len(),
            1,
            "only the notice: {:?}",
            stale.entries
        );
        assert_eq!(stale.last_seq, kept);
    }

    /// A subscription continues the transcript on hand when at most TRANSCRIPT_TAIL events
    /// came after it, and otherwise replays the last TRANSCRIPT_TAIL events.
    #[test]
    fn transcripts_resume_after_what_they_hold() {
        assert_eq!(resume_since(0, 10), 0);
        assert_eq!(resume_since(7, 10), 7);
        assert_eq!(resume_since(10, 10), 10);
        assert_eq!(resume_since(4990, 5000), 4990);
        assert_eq!(
            resume_since(5000 - TRANSCRIPT_TAIL, 5000),
            5000 - TRANSCRIPT_TAIL
        );
        // Too far behind: the last events only.
        assert_eq!(resume_since(0, 5000), 5000 - TRANSCRIPT_TAIL);
        assert_eq!(resume_since(100, 5000), 5000 - TRANSCRIPT_TAIL);
        // Past the head: not this log (the transcript starts over).
        assert_eq!(resume_since(6000, 5000), 5000 - TRANSCRIPT_TAIL);
        assert_eq!(resume_since(12, 10), 0);
    }

    /// A hub that accepts but never answers cannot hang the TUI: connecting and subscribing
    /// give up after their timeout.
    #[tokio::test]
    async fn a_silent_hub_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("hub.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = listener.accept().await {
                held.push(s);
            }
        });
        let start = std::time::Instant::now();
        let e = subscribe_thread(&sock, "t1", 0, Duration::from_millis(200))
            .await
            .err()
            .unwrap();
        assert!(format!("{e:#}").contains("no answer within"), "{e:#}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}

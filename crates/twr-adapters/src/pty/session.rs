//! DEGRADED heuristic session logic of the PTY adapter. Pure: the driver passes terminal
//! output, commands and the current time, and writes what [`Core`] returns. Every event it
//! produces carries `ext.pty.degraded = true`.
//!
//! Heuristics (all of them can be wrong; that is what "degraded" means):
//! - **Input gate (relay rule):** a prompt is only typed while no turn and no question is open
//!   and the program shows its own input prompt: after `settle` of quiet (or at the end of a
//!   turn) the unfinished last line is an input prompt (`>`, `$`, `❯`...) and the lines since
//!   twr last typed show no menu (`❯ 1.`, "Enter to confirm", "Esc to cancel", a numbered
//!   list right before the prompt), and nothing was printed since. Anything else at startup
//!   or between turns (a `[y/N]` consent prompt, a question, a menu, an unrecognized line, no
//!   output at all) is never answered with a task: after `idle` of quiet it raises
//!   `attention.raised{question}` (`ext.pty.held`) when a prompt is waiting, or whenever it
//!   looks like a question or a menu, and the queue is held until the program shows its
//!   input prompt (the user's answer is typed as given). The same gate applies to the first
//!   prompt of a resumed or retried process, which starts the command afresh.
//! - **Turn start:** a prompt is typed through the gate above, with exactly one submit: text +
//!   `\r` for one line. Several lines go as a bracketed paste (`ESC [200~` text, lines joined
//!   by `\r`, `ESC [201~`) + one `\r` only when the program enabled bracketed paste (it
//!   printed `ESC [?2004h` and not `ESC [?2004l` since); a program that did not would take
//!   every embedded `\r` as its own submit (a line `y` in a task could answer a later
//!   `[y/N]`), so for it the lines are FLATTENED into one line (line breaks become spaces).
//!   Typed text never carries control characters or escape sequences: ESC, C0/C1 controls
//!   and DEL are removed (tab becomes a space), so text cannot end a paste early
//!   (`ESC [201~`) or submit itself. The same applies to answers.
//! - **Echo:** the echo of typed text is dropped from the output: a completed line is dropped
//!   only if it is one of the next two lines and, with any paste markers removed and trimmed,
//!   equals a typed line or ends with the prompt tail that was on screen when it was typed
//!   followed by the typed line (`> ` + `task`). Anything else is output ("Successfully" is
//!   never taken for the echo of `y`).
//! - **Idle:** `idle` after the last byte of output, when the turn printed something visible:
//!   an unfinished last line that looks like a question (`?`, `:`, `[y/N]`, `(y/n)`...) or a
//!   menu / dialog marker ("Enter to confirm", `❯ 1.`) raises `attention.raised{question}`;
//!   anything else (the program's own `>` / `$` prompt, or nothing) ends the turn:
//!   `turn.ended{completed}` with `ext.pty.heuristic = true`.
//! - **Answer:** typed like a prompt (`allow` → `y`, `deny` → `n` unless a message is given).
//!   A question is withdrawn (`attention.resolved{by: timeout}`) when, before it is answered,
//!   the program completes a new visible line or shows a different unfinished line; a prompt
//!   redrawn identically (e.g. `\r` + the same text) keeps it open.
//! - **Exit:** an open turn ends with the process (`completed` on exit code 0, else `error`),
//!   unless twr stopped it (then the hub closes the turn).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use twr_core::adapter::Answer;
use twr_core::event::{
    AgentText, AttentionRaised, AttentionReason, AttentionResolved, EventBody, JsonMap, ResolvedBy,
    TurnEnded, TurnResult,
};

use super::screen::{
    classify_tail, is_menu, is_noise, menu_hint, AnsiStripper, LineBuffer, PasteMode, Tail,
    Utf8Decoder,
};
use crate::claude_code::translate::one_line;

/// Key of this runtime's extras (`data.ext.pty`).
pub const EXT: &str = "pty";

/// Chars of recent output kept for `Exited::stderr_tail`.
pub const RECENT_MAX: usize = 4096;

/// Chars kept of a turn's final text.
pub const FINAL_MAX: usize = 32 * 1024;

/// Timing knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// Quiet time after output before deciding "question" or "turn over".
    pub idle: Duration,
    /// Quiet time after the first output before the program counts as ready.
    pub settle: Duration,
    /// The program counts as ready after this long even if it printed nothing.
    pub startup_max: Duration,
    /// Text deltas are coalesced for this long.
    pub flush: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle: Duration::from_secs(3),
            settle: Duration::from_millis(500),
            startup_max: Duration::from_secs(3),
            flush: Duration::from_millis(200),
        }
    }
}

/// One normalized event.
#[derive(Debug, Clone, PartialEq)]
pub struct Emitted {
    /// Kind + payload.
    pub body: EventBody,
    /// Turn it belongs to (None for output outside a turn).
    pub turn: Option<u64>,
}

/// What one input produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Out {
    /// Events to emit, in order.
    pub events: Vec<Emitted>,
    /// Bytes to write to the terminal, in order.
    pub writes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Question {
    attn_id: String,
    turn: Option<u64>,
    /// The unfinished line it was raised on (trimmed): an identical redraw keeps it open.
    tail: String,
}

/// Completed lines remembered since twr last typed (menu detection).
const SINCE_INPUT_MAX: usize = 8;

/// A line of typed text whose echo is expected.
#[derive(Debug, Clone)]
struct Echo {
    /// The typed line, trimmed.
    text: String,
    /// The unfinished line on screen when it was typed (the program's prompt), trimmed at
    /// the start; empty for the second and later lines of a paste.
    prompt: String,
    /// Completed visible lines seen while this was the next expected echo.
    seen: u8,
}

/// How many completed lines may pass before an expected echo is given up.
const ECHO_WINDOW: u8 = 2;

/// Start of a bracketed paste.
pub const PASTE_START: &str = "\x1b[200~";
/// End of a bracketed paste.
pub const PASTE_END: &str = "\x1b[201~";

/// `text` as it may be typed: tab becomes a space; every other control character (ESC, C0
/// and C1 controls, DEL) is removed except line breaks, which are normalized to `\n`.
/// Trailing line breaks are dropped.
fn sanitize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let clean: String = text
        .chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    clean.trim_end_matches('\n').to_string()
}

/// The lines of `text` as typed (see [`typed_bytes`]): the sanitized lines when they go as a
/// bracketed paste, or the one flattened line otherwise.
pub fn typed_lines(text: &str, paste: bool) -> Vec<String> {
    let clean = sanitize(text);
    if paste {
        clean.split('\n').map(str::to_string).collect()
    } else {
        vec![clean.replace('\n', " ")]
    }
}

/// The bytes typed for `text`: `text\r` for one line; for several, a bracketed paste of the
/// lines (joined by `\r`, as a terminal pastes them) + one `\r` when `paste` (the program
/// enabled bracketed paste), else the lines flattened into one (joined by spaces) + `\r`.
/// Exactly one submit either way; control characters and escape sequences are removed (see
/// the module doc).
pub fn typed_bytes(text: &str, paste: bool) -> Vec<u8> {
    let lines = typed_lines(text, paste);
    let mut bytes = Vec::new();
    if lines.len() > 1 {
        bytes.extend_from_slice(PASTE_START.as_bytes());
        bytes.extend_from_slice(lines.join("\r").as_bytes());
        bytes.extend_from_slice(PASTE_END.as_bytes());
    } else {
        bytes.extend_from_slice(lines.concat().as_bytes());
    }
    bytes.push(b'\r');
    bytes
}

/// A completed line as compared with an expected echo: trimmed, without paste markers (the
/// ANSI stripper removes `ESC [200~`, but a terminal that echoes control characters shows it
/// as `^[[200~`).
fn echo_form(line: &str) -> String {
    line.replace("^[[200~", "")
        .replace("^[[201~", "")
        .trim()
        .to_string()
}

/// Heuristic session state of one PTY process.
#[derive(Debug)]
pub struct Core {
    timing: Timing,
    started_at: Instant,
    /// The program shows its input prompt (checked after `settle` of quiet or at the end of a
    /// turn) and nothing was printed or typed since: a prompt may be typed.
    prompt_ready: bool,
    /// The out-of-turn readiness check of the current quiet period was done.
    settle_handled: bool,
    /// Completed visible lines since twr last typed (the last [`SINCE_INPUT_MAX`]).
    since_input: VecDeque<String>,
    last_output: Option<Instant>,
    idle_handled: bool,
    decoder: Utf8Decoder,
    stripper: AnsiStripper,
    /// Whether the program enabled bracketed paste (decides how several lines are typed).
    paste_mode: PasteMode,
    lines: LineBuffer,
    pending: Vec<String>,
    flush_at: Option<Instant>,
    turn: Option<u64>,
    queue: VecDeque<(String, u64)>,
    fresh: bool,
    question: Option<Question>,
    segment: String,
    echo: VecDeque<Echo>,
    attn_seq: u64,
    stopping: bool,
    recent: String,
}

/// `{"pty": {"degraded": true, ...extra}}`.
pub fn ext(extra: Map<String, Value>) -> Option<JsonMap> {
    let mut pty = Map::new();
    pty.insert("degraded".into(), json!(true));
    pty.extend(extra);
    let mut m = JsonMap::new();
    m.insert(EXT.into(), Value::Object(pty));
    Some(m)
}

fn heuristic(pairs: &[(&str, Value)]) -> Option<JsonMap> {
    let mut m = Map::new();
    m.insert("heuristic".into(), json!(true));
    for (k, v) in pairs {
        m.insert((*k).into(), v.clone());
    }
    ext(m)
}

/// The text typed for an answer.
pub fn answer_text(answer: &Answer) -> String {
    let msg = answer
        .message
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match answer.decision.trim() {
        "allow" => msg.unwrap_or("y").to_string(),
        "deny" | "decline" => msg.unwrap_or("n").to_string(),
        "" => msg.unwrap_or("").to_string(),
        other => other.to_string(),
    }
}

fn keep_last(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = s.len() - max;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
}

impl Core {
    /// A session started at `now`.
    pub fn new(timing: Timing, now: Instant) -> Self {
        Core {
            timing,
            started_at: now,
            prompt_ready: false,
            settle_handled: false,
            since_input: VecDeque::new(),
            last_output: None,
            idle_handled: false,
            decoder: Utf8Decoder::default(),
            stripper: AnsiStripper::default(),
            paste_mode: PasteMode::default(),
            lines: LineBuffer::default(),
            pending: Vec::new(),
            flush_at: None,
            turn: None,
            queue: VecDeque::new(),
            fresh: false,
            question: None,
            segment: String::new(),
            echo: VecDeque::new(),
            attn_seq: 0,
            stopping: false,
            recent: String::new(),
        }
    }

    /// The open turn.
    pub fn turn(&self) -> Option<u64> {
        self.turn
    }

    /// The open question's attention id.
    pub fn open_question(&self) -> Option<&str> {
        self.question.as_ref().map(|q| q.attn_id.as_str())
    }

    /// The last ~[`RECENT_MAX`] chars of cleaned output.
    pub fn recent_output(&self) -> &str {
        &self.recent
    }

    /// Whether a prompt would be typed now (the program shows its input prompt, see the
    /// module doc).
    pub fn prompt_ready(&self) -> bool {
        self.prompt_ready
    }

    /// Prompts waiting to be typed.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// When the out-of-turn readiness check is due: `settle` after the last output, or
    /// `startup_max` after the start when there was none.
    fn settle_at(&self) -> Instant {
        match self.last_output {
            Some(t) => t + self.timing.settle,
            None => self.started_at + self.timing.startup_max,
        }
    }

    /// When the idle decision is due: `idle` after the last output, or `startup_max` after
    /// the start when there was none.
    fn idle_at(&self) -> Instant {
        match self.last_output {
            Some(t) => t + self.timing.idle,
            None => self.started_at + self.timing.startup_max,
        }
    }

    fn settle_pending(&self) -> bool {
        !self.settle_handled && self.turn.is_none() && !self.stopping
    }

    /// When [`Core::tick`] has something to do next.
    pub fn next_deadline(&self) -> Option<Instant> {
        let mut d: Vec<Instant> = Vec::new();
        d.extend(self.flush_at);
        if self.settle_pending() {
            d.push(self.settle_at());
        }
        if !self.idle_handled {
            d.push(self.idle_at());
        }
        d.into_iter().min()
    }

    /// Terminal output arrived.
    pub fn on_output(&mut self, bytes: &[u8], now: Instant) -> Out {
        let mut out = Out::default();
        if bytes.is_empty() {
            return out;
        }
        self.last_output = Some(now);
        self.idle_handled = false;
        self.settle_handled = false;
        let text = self.decoder.decode(bytes);
        self.paste_mode.feed(&text);
        let clean = self.stripper.strip(&text);
        let mut visible = false;
        let mut new_line = false;
        for line in self.lines.feed(&clean) {
            if self.is_echo(&line) {
                continue;
            }
            if is_noise(&line) {
                continue;
            }
            visible = true;
            new_line = true;
            self.since_input.push_back(line.clone());
            if self.since_input.len() > SINCE_INPUT_MAX {
                self.since_input.pop_front();
            }
            self.pending.push(line);
        }
        let tail = self.lines.tail();
        let tail_shown = !is_noise(&tail);
        if tail_shown {
            visible = true;
        }
        if visible {
            // Whatever the screen shows now has not been checked yet.
            self.prompt_ready = false;
            self.fresh = true;
            // A completed line, or a different unfinished line: the program moved on. The
            // same prompt drawn again (`\r` + identical text) is not an answer to anything.
            let moved_on = new_line
                || (tail_shown
                    && self
                        .question
                        .as_ref()
                        .is_some_and(|q| q.tail != tail.trim()));
            if let Some(q) = self.question.take_if(|_| moved_on) {
                // The program moved on without an answer: the question is gone.
                tracing::info!(attn_id = %q.attn_id, "pty: output resumed; withdrawing the question");
                out.events.push(Emitted {
                    body: EventBody::AttentionResolved(AttentionResolved {
                        attn_id: q.attn_id,
                        by: ResolvedBy::Timeout,
                        answer: None,
                        ext: heuristic(&[("cancelled_by", json!("output_resumed"))]),
                        ..Default::default()
                    }),
                    turn: q.turn,
                });
            }
        }
        if !self.pending.is_empty() && self.flush_at.is_none() {
            self.flush_at = Some(now + self.timing.flush);
        }
        out
    }

    /// Queues turn `turn` with `text`: typed once no turn or question is open and the program
    /// shows its input prompt (see the module doc). When the program has been quiet for
    /// `idle` without showing one, the prompt is held behind a question at once.
    pub fn send(&mut self, text: &str, turn: u64, now: Instant) -> Out {
        let mut out = Out::default();
        self.queue.push_back((text.to_string(), turn));
        self.pump(&mut out);
        if self.idle_handled && self.turn.is_none() {
            self.hold(&mut out);
        }
        let _ = now;
        out
    }

    /// Types the next queued prompt if the gate is open. Returns whether it did.
    fn pump(&mut self, out: &mut Out) -> bool {
        if !self.prompt_ready || self.turn.is_some() || self.question.is_some() || self.stopping {
            return false;
        }
        let Some((text, turn)) = self.queue.pop_front() else {
            return false;
        };
        // Output from before the turn stays outside it.
        self.flush_lines(out);
        self.segment.clear();
        self.turn = Some(turn);
        self.fresh = false;
        self.type_text(&text, out);
        true
    }

    /// The since-input lines as a slice-friendly vector.
    fn recent_lines(&self) -> Vec<String> {
        self.since_input.iter().cloned().collect()
    }

    /// Out of a turn, after `settle` of quiet: the gate opens when the program shows its input
    /// prompt and no menu.
    fn on_settle(&mut self, out: &mut Out) {
        if self.question.is_some() {
            return;
        }
        let tail = self.lines.tail();
        let lines = self.recent_lines();
        if classify_tail(&tail) == Tail::InputPrompt && !is_menu(&lines, &tail) {
            self.prompt_ready = true;
            self.pump(out);
        }
    }

    /// Out of a turn, with the gate closed after `idle` of quiet (or a prompt queued after
    /// that): raises a question for what the program shows when a prompt is waiting, or when
    /// it looks like a question or a menu. The queue is held until the program shows its input
    /// prompt; the user's answer is typed as given.
    fn hold(&mut self, out: &mut Out) {
        if self.question.is_some() || self.prompt_ready || self.stopping || self.turn.is_some() {
            return;
        }
        let tail = self.lines.tail();
        let lines = self.recent_lines();
        let class = classify_tail(&tail);
        let menu = is_menu(&lines, &tail);
        let asks = matches!(class, Tail::Question { .. }) || menu_hint(&lines, &tail);
        if self.queue.is_empty() && !asks {
            return;
        }
        let options = match &class {
            Tail::Question { options } if !menu => options.clone(),
            _ => Vec::new(),
        };
        let shown = one_line(&tail, 400);
        let summary = if menu {
            let last = lines.last().map(|l| one_line(l, 200)).unwrap_or_default();
            match (shown.is_empty(), last.is_empty()) {
                (false, _) => format!("The program shows a menu: {shown}"),
                (true, false) => format!("The program shows a menu: {last}"),
                (true, true) => "The program shows a menu".to_string(),
            }
        } else if shown.is_empty() {
            "The program shows no input prompt twr recognizes; your answer is typed into it"
                .to_string()
        } else if class == Tail::Other || class == Tail::InputPrompt {
            format!("The program shows no input prompt twr recognizes: {shown}")
        } else {
            shown
        };
        self.attn_seq += 1;
        let attn_id = format!("pty-q{}", self.attn_seq);
        let turn = self.queue.front().map(|(_, t)| *t);
        let kind = if menu {
            "menu"
        } else if options.is_empty() {
            "open"
        } else {
            "yes_no"
        };
        tracing::info!(%attn_id, queued = self.queue.len(), "pty: no input prompt; holding the queue behind a question");
        out.events.push(Emitted {
            body: EventBody::AttentionRaised(AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Question,
                summary,
                options,
                call_id: None,
                ext: heuristic(&[
                    ("tail", json!(tail)),
                    ("kind", json!(kind)),
                    ("held", json!(true)),
                    ("queued", json!(self.queue.len())),
                ]),
                ..Default::default()
            }),
            turn,
        });
        self.question = Some(Question {
            attn_id,
            turn,
            tail: tail.trim().to_string(),
        });
    }

    /// Whether completed `line` is the echo of typed text (see the module doc). Consumes the
    /// expectation it matches, and gives up one after [`ECHO_WINDOW`] other lines.
    fn is_echo(&mut self, line: &str) -> bool {
        let l = echo_form(line);
        if l.is_empty() || is_noise(&l) {
            return false;
        }
        let Some(exp) = self.echo.front_mut() else {
            return false;
        };
        let with_prompt =
            !exp.prompt.is_empty() && l.ends_with(&format!("{}{}", exp.prompt, exp.text));
        if l == exp.text || with_prompt {
            self.echo.pop_front();
            return true;
        }
        exp.seen += 1;
        if exp.seen >= ECHO_WINDOW {
            // Not echoed where it would have been (e.g. a password prompt): stop expecting it.
            self.echo.clear();
        }
        false
    }

    fn type_text(&mut self, text: &str, out: &mut Out) {
        let prompt = self.lines.tail().trim_start().to_string();
        let paste = self.paste_mode.enabled();
        let mut first = true;
        for l in typed_lines(text, paste) {
            let t = l.trim();
            if t.is_empty() {
                continue;
            }
            self.echo.push_back(Echo {
                text: t.to_string(),
                prompt: if first { prompt.clone() } else { String::new() },
                seen: 0,
            });
            first = false;
        }
        // Whatever was on screen has been answered: the gate is checked again from here.
        self.prompt_ready = false;
        self.since_input.clear();
        out.writes.push(typed_bytes(text, paste));
    }

    /// Answers the open question `attn_id` at `now`. None if it is not the open question.
    pub fn respond(&mut self, attn_id: &str, answer: &Answer, now: Instant) -> Option<Out> {
        if self.question.as_ref().map(|q| q.attn_id.as_str()) != Some(attn_id) {
            return None;
        }
        self.question = None;
        let mut out = Out::default();
        self.fresh = false;
        self.type_text(&answer_text(answer), &mut out);
        // What the program shows next is checked from now on (even if it prints nothing).
        self.last_output = Some(now);
        self.settle_handled = false;
        self.idle_handled = false;
        Some(out)
    }

    /// Stops heuristics: queued prompts are dropped and an exit no longer ends the turn.
    pub fn stop(&mut self) {
        self.stopping = true;
        self.queue.clear();
    }

    /// Handles due timers.
    pub fn tick(&mut self, now: Instant) -> Out {
        let mut out = Out::default();
        if self.flush_at.is_some_and(|t| t <= now) {
            self.flush_lines(&mut out);
        }
        if self.settle_pending() && self.settle_at() <= now {
            self.settle_handled = true;
            self.on_settle(&mut out);
        }
        if !self.idle_handled && self.idle_at() <= now {
            self.idle_handled = true;
            self.on_idle(&mut out);
        }
        out
    }

    fn on_idle(&mut self, out: &mut Out) {
        self.flush_lines(out);
        // An echo that has not shown up by now will not (e.g. a password prompt).
        self.echo.clear();
        let tail = self.lines.tail();
        let shown = self.lines.take_tail();
        if !is_noise(&shown) {
            self.emit_text(shown, out);
        }
        let Some(turn) = self.turn else {
            self.hold(out);
            return;
        };
        if !self.fresh || self.question.is_some() {
            return;
        }
        let lines = self.recent_lines();
        let class = match classify_tail(&tail) {
            // A menu or dialog waits for a choice, whatever its last line looks like.
            _ if menu_hint(&lines, &tail) => Tail::Question {
                options: Vec::new(),
            },
            c => c,
        };
        match class {
            Tail::Question { options } => {
                self.flush_final(out);
                self.attn_seq += 1;
                let attn_id = format!("pty-q{}", self.attn_seq);
                let summary = match one_line(&tail, 400) {
                    s if s.is_empty() => "The program is waiting for input".to_string(),
                    s => s,
                };
                let kind = if options.is_empty() { "open" } else { "yes_no" };
                out.events.push(Emitted {
                    body: EventBody::AttentionRaised(AttentionRaised {
                        attn_id: attn_id.clone(),
                        reason: AttentionReason::Question,
                        summary,
                        options,
                        call_id: None,
                        ext: heuristic(&[("tail", json!(tail)), ("kind", json!(kind))]),
                        ..Default::default()
                    }),
                    turn: Some(turn),
                });
                self.question = Some(Question {
                    attn_id,
                    turn: Some(turn),
                    tail: tail.trim().to_string(),
                });
            }
            t => {
                let end = if t == Tail::InputPrompt {
                    // The program's own prompt is not part of its answer.
                    let prompt = tail.trim();
                    let kept = self.segment.trim_end();
                    if let Some(rest) = kept.strip_suffix(prompt) {
                        let n = rest.len();
                        self.segment.truncate(n);
                    }
                    "input_prompt"
                } else {
                    "idle"
                };
                self.end_turn(TurnResult::Completed, &[("end", json!(end))], out);
                // The next prompt only goes into the program's own input prompt.
                if t == Tail::InputPrompt && !is_menu(&lines, &tail) {
                    self.prompt_ready = true;
                    self.pump(out);
                } else {
                    self.hold(out);
                }
            }
        }
    }

    /// The process exited with `code`: flushes the output and, unless stopping, ends the
    /// open turn.
    pub fn on_exit(&mut self, code: Option<i32>) -> Out {
        let mut out = Out::default();
        self.flush_lines(&mut out);
        let shown = self.lines.take_tail();
        if !is_noise(&shown) {
            self.emit_text(shown, &mut out);
        }
        if self.turn.is_some() && !self.stopping {
            let result = if code == Some(0) {
                TurnResult::Completed
            } else {
                TurnResult::Error
            };
            self.end_turn(
                result,
                &[("end", json!("exit")), ("exit_code", json!(code))],
                &mut out,
            );
        }
        out
    }

    fn end_turn(&mut self, result: TurnResult, extra: &[(&str, Value)], out: &mut Out) {
        self.flush_final(out);
        let turn = self.turn.take();
        self.question = None;
        out.events.push(Emitted {
            body: EventBody::TurnEnded(TurnEnded {
                result,
                ext: heuristic(extra),
                ..Default::default()
            }),
            turn,
        });
    }

    fn flush_lines(&mut self, out: &mut Out) {
        self.flush_at = None;
        if self.pending.is_empty() {
            return;
        }
        let mut text = std::mem::take(&mut self.pending).join("\n");
        text.push('\n');
        self.emit_text(text, out);
    }

    fn emit_text(&mut self, text: String, out: &mut Out) {
        if text.is_empty() {
            return;
        }
        self.recent.push_str(&text);
        keep_last(&mut self.recent, RECENT_MAX);
        if self.turn.is_some() {
            self.segment.push_str(&text);
            keep_last(&mut self.segment, FINAL_MAX);
        }
        out.events.push(Emitted {
            body: EventBody::AgentText(AgentText {
                text,
                delta: true,
                ext: ext(Map::new()),
                ..Default::default()
            }),
            turn: self.turn,
        });
    }

    fn flush_final(&mut self, out: &mut Out) {
        self.flush_lines(out);
        let text = std::mem::take(&mut self.segment);
        let text = text.trim_end();
        if text.trim().is_empty() {
            return;
        }
        out.events.push(Emitted {
            body: EventBody::AgentText(AgentText {
                text: text.to_string(),
                is_final: true,
                ext: heuristic(&[]),
                ..Default::default()
            }),
            turn: self.turn,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Timing {
        Timing {
            idle: Duration::from_millis(1000),
            settle: Duration::from_millis(100),
            startup_max: Duration::from_millis(2000),
            flush: Duration::from_millis(50),
        }
    }

    fn kinds(out: &Out) -> Vec<String> {
        out.events
            .iter()
            .map(|e| match &e.body {
                EventBody::AgentText(t) if t.is_final => format!("final@{:?}", e.turn),
                EventBody::AgentText(_) => format!("delta@{:?}", e.turn),
                b => format!("{}@{:?}", b.kind(), e.turn),
            })
            .collect()
    }

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    fn texts(out: &Out) -> String {
        out.events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AgentText(t) if t.delta => Some(t.text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn question_answer_and_turn_end() {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        // Waits for the program to settle before typing.
        assert!(c.send("task", 1, t0).writes.is_empty());
        c.on_output(b"> ", ms(t0, 10));
        let out = c.tick(ms(t0, 110));
        assert_eq!(out.writes, [b"task\r".to_vec()]);
        assert_eq!(c.turn(), Some(1));
        // Echo, output, then a yes/no prompt.
        c.on_output(b"> task\r\n", ms(t0, 120));
        c.on_output(
            b"\x1b[1mrunning task\x1b[0m\r\nContinue? [y/N] ",
            ms(t0, 130),
        );
        let out = c.tick(ms(t0, 200));
        assert_eq!(texts(&out), "running task\n");
        let out = c.tick(ms(t0, 1130));
        assert_eq!(
            kinds(&out),
            ["delta@Some(1)", "final@Some(1)", "attention.raised@Some(1)"]
        );
        assert_eq!(texts(&out), "Continue? [y/N] ");
        let a = match &out.events[2].body {
            EventBody::AttentionRaised(a) => a.clone(),
            _ => unreachable!(),
        };
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.options, ["y", "n"]);
        assert_eq!(a.summary, "Continue? [y/N]");
        assert_eq!(a.ext.as_ref().unwrap()["pty"]["degraded"], true);
        assert!(c.respond("nope", &Answer::allow(), ms(t0, 1150)).is_none());
        let out = c
            .respond(&a.attn_id, &Answer::allow(), ms(t0, 1150))
            .unwrap();
        assert_eq!(out.writes, [b"y\r".to_vec()]);
        // Echo of the answer completes the prompt line: nothing new is shown for it.
        c.on_output(b"y\r\ngot y\r\n> ", ms(t0, 1200));
        let out = c.tick(ms(t0, 2200));
        assert_eq!(texts(&out), "got y\n> ");
        assert_eq!(
            kinds(&out),
            [
                "delta@Some(1)",
                "delta@Some(1)",
                "final@Some(1)",
                "turn.ended@Some(1)"
            ]
        );
        match &out.events[3].body {
            EventBody::TurnEnded(e) => {
                assert_eq!(e.result, TurnResult::Completed);
                let p = &e.ext.as_ref().unwrap()["pty"];
                assert_eq!(p["heuristic"], true);
                assert_eq!(p["end"], "input_prompt");
            }
            _ => unreachable!(),
        }
        assert_eq!(c.turn(), None);
    }

    #[test]
    fn idle_without_output_keeps_the_turn_open() {
        let (mut c, t0, out) = started("go");
        assert_eq!(out.writes, [b"go\r".to_vec()]);
        c.on_output(b"go\r\n", ms(t0, 2010));
        let out = c.tick(ms(t0, 5000));
        assert!(out.events.is_empty(), "{out:?}");
        assert_eq!(c.turn(), Some(1));
        assert_eq!(c.next_deadline(), None);
    }

    fn raised(out: &Out) -> AttentionRaised {
        out.events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::AttentionRaised(a) => Some(a.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no attention.raised in {out:?}"))
    }

    /// A CLI that starts with a consent prompt never gets the task typed into
    /// it; the prompt is held behind a question until the program shows its input prompt.
    #[test]
    fn a_startup_consent_prompt_holds_the_task_behind_a_question() {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        assert!(c.send("task", 1, t0).writes.is_empty());
        let out = c.on_output(b"Trust this directory? [y/N] ", ms(t0, 10));
        assert!(out.writes.is_empty());
        // Settled: not an input prompt, so nothing is typed.
        let out = c.tick(ms(t0, 110));
        assert!(out.writes.is_empty(), "{out:?}");
        assert!(!c.prompt_ready());
        // Idle: the consent prompt becomes a question for the user.
        let out = c.tick(ms(t0, 1010));
        assert!(out.writes.is_empty(), "{out:?}");
        let a = raised(&out);
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.options, ["y", "n"]);
        assert_eq!(a.summary, "Trust this directory? [y/N]");
        let p = &a.ext.as_ref().unwrap()["pty"];
        assert_eq!(p["held"], true);
        assert_eq!(p["queued"], 1);
        assert_eq!(kinds(&out).last().unwrap(), "attention.raised@Some(1)");
        assert_eq!(c.turn(), None, "no turn runs while the question is open");
        // Nothing is typed however long it waits, or when more is sent.
        assert!(c.tick(ms(t0, 9000)).writes.is_empty());
        assert!(c.send("more", 2, ms(t0, 9000)).writes.is_empty());
        assert_eq!(c.queued(), 2);
        // The user's answer is typed as given; the task follows the program's input prompt.
        let out = c
            .respond(&a.attn_id, &Answer::allow(), ms(t0, 9100))
            .unwrap();
        assert_eq!(out.writes, [b"y\r".to_vec()]);
        c.on_output(b"y\r\nWelcome\r\n> ", ms(t0, 9150));
        let out = c.tick(ms(t0, 9250));
        assert_eq!(out.writes, [b"task\r".to_vec()]);
        assert_eq!(c.turn(), Some(1));
    }

    #[test]
    fn a_startup_menu_is_never_typed_into() {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        c.send("task", 1, t0);
        c.on_output(
            "Do you trust the files in this folder?\r\n\u{276f} 1. Yes, proceed\r\n  2. No, exit\r\n\r\nEnter to confirm \u{b7} Esc to exit".as_bytes(),
            ms(t0, 10),
        );
        assert!(c.tick(ms(t0, 110)).writes.is_empty());
        let out = c.tick(ms(t0, 1010));
        assert!(out.writes.is_empty());
        let a = raised(&out);
        assert!(
            a.summary.starts_with("The program shows a menu"),
            "{}",
            a.summary
        );
        assert!(a.options.is_empty());
        assert_eq!(a.ext.as_ref().unwrap()["pty"]["kind"], "menu");
        // Even a prompt-like last line under a menu hint is not an input prompt.
        let mut c = Core::new(t(), t0);
        c.send("task", 1, t0);
        c.on_output(
            "Pick:\r\n\u{276f} 1. Yes\r\n  2. No\r\n> ".as_bytes(),
            ms(t0, 10),
        );
        assert!(c.tick(ms(t0, 110)).writes.is_empty());
        assert!(c.tick(ms(t0, 1010)).writes.is_empty());
        assert!(c.open_question().is_some());
    }

    #[test]
    fn no_output_at_all_holds_a_waiting_prompt() {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        // Nothing waits: nothing to ask.
        let out = c.tick(ms(t0, 2000));
        assert!(out.events.is_empty() && out.writes.is_empty(), "{out:?}");
        // A prompt sent to a silent program is held behind a question at once.
        let out = c.send("go", 1, ms(t0, 2001));
        assert!(out.writes.is_empty());
        let a = raised(&out);
        assert!(a.summary.contains("no input prompt"), "{}", a.summary);
        assert!(a.options.is_empty());
        // The same when the program was silent from the start and a prompt was queued.
        let mut c = Core::new(t(), t0);
        c.send("go", 1, t0);
        let out = c.tick(ms(t0, 2000));
        assert!(out.writes.is_empty());
        raised(&out);
    }

    #[test]
    fn between_turns_the_next_prompt_waits_for_an_input_prompt() {
        // Turn 1 ends idle on a line that is not an input prompt: turn 2 is held.
        let (mut c, t0, _) = started("one");
        c.send("two", 2, ms(t0, 120));
        c.on_output(b"one\r\nDownloading model", ms(t0, 130));
        let out = c.tick(ms(t0, 1130));
        assert!(out.writes.is_empty(), "{out:?}");
        assert_eq!(
            kinds(&out),
            [
                "delta@Some(1)",
                "final@Some(1)",
                "turn.ended@Some(1)",
                "attention.raised@Some(2)"
            ]
        );
        assert_eq!(c.turn(), None);
        // The program shows its prompt: the question is withdrawn and turn 2 is typed.
        let out = c.on_output(b"\r\ndone\r\n> ", ms(t0, 1500));
        assert_eq!(kinds(&out), ["attention.resolved@Some(2)"]);
        let out = c.tick(ms(t0, 1600));
        assert_eq!(out.writes, [b"two\r".to_vec()]);
        assert_eq!(c.turn(), Some(2));

        // A numbered list right before the prompt may be a menu: held too.
        let (mut c, t0, _) = started("one");
        c.send("two", 2, ms(t0, 120));
        c.on_output(b"one\r\nChoose:\r\n1. keep\r\n2. drop\r\n> ", ms(t0, 130));
        let out = c.tick(ms(t0, 1130));
        assert!(out.writes.is_empty(), "{out:?}");
        assert_eq!(kinds(&out).last().unwrap(), "attention.raised@Some(2)");
        // The answer is typed; the next plain prompt lets turn 2 through.
        let id = c.open_question().unwrap().to_string();
        let out = c.respond(&id, &Answer::new("1"), ms(t0, 1200)).unwrap();
        assert_eq!(out.writes, [b"1\r".to_vec()]);
        c.on_output(b"1\r\nkept\r\n> ", ms(t0, 1250));
        let out = c.tick(ms(t0, 1350));
        assert_eq!(out.writes, [b"two\r".to_vec()]);
    }

    #[test]
    fn a_menu_during_a_turn_is_a_question() {
        let (mut c, t0, _) = started("go");
        c.on_output(
            "go\r\nRun rm -rf build?\r\n\u{276f} 1. Yes\r\n  2. No\r\nEsc to cancel".as_bytes(),
            ms(t0, 120),
        );
        let out = c.tick(ms(t0, 1120));
        assert_eq!(kinds(&out).last().unwrap(), "attention.raised@Some(1)");
        assert_eq!(c.turn(), Some(1));
    }

    #[test]
    fn new_output_withdraws_an_unanswered_question() {
        let (mut c, t0, _) = started("go");
        c.on_output(b"go\r\nName? ", ms(t0, 2010));
        let out = c.tick(ms(t0, 3010));
        assert_eq!(kinds(&out).last().unwrap(), "attention.raised@Some(1)");
        let id = c.open_question().unwrap().to_string();
        let out = c.on_output(b"\r\ntimed out, using default\r\n", ms(t0, 3100));
        assert_eq!(kinds(&out), ["attention.resolved@Some(1)"]);
        match &out.events[0].body {
            EventBody::AttentionResolved(r) => {
                assert_eq!(r.attn_id, id);
                assert_eq!(r.by, ResolvedBy::Timeout);
            }
            _ => unreachable!(),
        }
        assert!(c.respond(&id, &Answer::new("bob"), ms(t0, 3150)).is_none());
        let out = c.tick(ms(t0, 4200));
        assert_eq!(kinds(&out).last().unwrap(), "turn.ended@Some(1)");
    }

    #[test]
    fn exit_ends_the_open_turn_unless_stopped() {
        let (mut c, t0, _) = started("go");
        c.on_output(b"partial", ms(t0, 2010));
        let out = c.on_exit(Some(2));
        assert_eq!(
            kinds(&out),
            ["delta@Some(1)", "final@Some(1)", "turn.ended@Some(1)"]
        );
        match &out.events[2].body {
            EventBody::TurnEnded(e) => assert_eq!(e.result, TurnResult::Error),
            _ => unreachable!(),
        }
        let (mut c, t0, _) = started("go");
        c.stop();
        assert!(!kinds(&c.on_exit(None))
            .iter()
            .any(|k| k.starts_with("turn.ended")));
        assert!(c.send("x", 2, ms(t0, 2100)).writes.is_empty());
    }

    /// A session with the program's `> ` prompt shown and turn 1 typed.
    fn started(text: &str) -> (Core, Instant, Out) {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        c.send(text, 1, t0);
        c.on_output(b"> ", ms(t0, 10));
        let out = c.tick(ms(t0, 110));
        (c, t0, out)
    }

    #[test]
    fn one_submit_per_prompt() {
        for paste in [false, true] {
            assert_eq!(typed_bytes("hi", paste), b"hi\r");
            assert_eq!(
                typed_bytes("hi\n", paste),
                b"hi\r",
                "a trailing newline is not a second line"
            );
        }
        assert_eq!(
            typed_bytes("fix a\nthen b\r\nthen c", true),
            b"\x1b[200~fix a\rthen b\rthen c\x1b[201~\r"
        );
        // Without bracketed paste every `\r` would be a submit: the lines are flattened.
        assert_eq!(
            typed_bytes("fix a\nthen b\r\nthen c", false),
            b"fix a then b then c\r"
        );
        // A program that enabled bracketed paste gets the paste.
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        c.send("line one\nline two", 1, t0);
        c.on_output(b"\x1b[?2004h> ", ms(t0, 10));
        let out = c.tick(ms(t0, 110));
        assert_eq!(
            out.writes,
            [b"\x1b[200~line one\rline two\x1b[201~\r".to_vec()]
        );
        // One that did not (or turned it off again) gets one line.
        let (_, _, out) = started("line one\nline two");
        assert_eq!(out.writes, [b"line one line two\r".to_vec()]);
        let mut c = Core::new(t(), t0);
        c.send("line one\nline two", 1, t0);
        c.on_output(b"\x1b[?2004h\x1b[?2004l> ", ms(t0, 10));
        assert_eq!(
            c.tick(ms(t0, 110)).writes,
            [b"line one line two\r".to_vec()]
        );
        // Answers are typed the same way.
        let (mut c, t0, _) = started("go");
        c.on_output(b"go\r\nName? ", ms(t0, 120));
        c.tick(ms(t0, 1120));
        let id = c.open_question().unwrap().to_string();
        let out = c.respond(&id, &Answer::new("a\nb"), ms(t0, 1130)).unwrap();
        assert_eq!(out.writes, [b"a b\r".to_vec()]);
    }

    /// Typed text cannot leave the submission: no paste terminator, escape sequence or
    /// control character goes through, with or without bracketed paste.
    #[test]
    fn typed_text_carries_no_control_sequences() {
        let evil = "task\x1b[201~\ry\x1b[200~\x03\x04\x7f\u{9b}2J\tend\x00";
        for paste in [false, true] {
            let bytes = typed_bytes(evil, paste);
            let inner = if paste {
                let b = bytes
                    .strip_prefix(PASTE_START.as_bytes())
                    .expect("several lines: a paste");
                b.strip_suffix(format!("{PASTE_END}\r").as_bytes())
                    .expect("one paste end, then one submit")
                    .to_vec()
            } else {
                bytes.strip_suffix(b"\r").unwrap().to_vec()
            };
            let inner = String::from_utf8(inner).unwrap();
            assert!(
                !inner.chars().any(|c| c.is_control() && c != '\r'),
                "{inner:?}"
            );
            let submits = bytes.iter().filter(|b| **b == b'\r').count();
            let expected_submits = if paste { 2 } else { 1 }; // one separator inside the paste
            assert_eq!(submits, expected_submits, "{bytes:?}");
            if paste {
                assert_eq!(inner, "task[201~\ry[200~2J end");
            } else {
                assert_eq!(inner, "task[201~ y[200~2J end");
            }
        }
    }

    #[test]
    fn echo_of_a_paste_is_dropped() {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        c.send("line one\nline two", 1, t0);
        c.on_output(b"\x1b[?2004h> ", ms(t0, 10));
        c.tick(ms(t0, 110));
        // A line-mode terminal echoes the paste markers as `^[[200~` / `^[[201~`.
        c.on_output(
            b"> ^[[200~line one\r\nline two^[[201~\r\nok\r\n",
            ms(t0, 120),
        );
        let out = c.tick(ms(t0, 200));
        assert_eq!(texts(&out), "ok\n");

        // Flattened (no bracketed paste): the echo is the one typed line.
        let (mut c, t0, _) = started("line one\nline two");
        c.on_output(b"> line one line two\r\nok\r\n", ms(t0, 120));
        let out = c.tick(ms(t0, 200));
        assert_eq!(texts(&out), "ok\n");
    }

    #[test]
    fn echo_is_only_the_typed_line_right_after_it_was_typed() {
        // "Successfully" ends with "y" but is not the echo of the answer "y".
        let (mut c, t0, _) = started("go");
        c.on_output(b"go\r\nApply? [y/N] ", ms(t0, 120));
        c.tick(ms(t0, 1120));
        let id = c.open_question().unwrap().to_string();
        c.respond(&id, &Answer::allow(), ms(t0, 1130)).unwrap();
        c.on_output(b"\r\nSuccessfully\r\n", ms(t0, 1200));
        let out = c.tick(ms(t0, 1300));
        assert_eq!(texts(&out), "Successfully\n");

        // The echo counts only within the next two lines; later, it is output.
        let (mut c, t0) = started_bare("ls");
        c.on_output(b"one\r\ntwo\r\nls\r\n", ms(t0, 2120));
        let out = c.tick(ms(t0, 2200));
        assert_eq!(texts(&out), "one\ntwo\nls\n");
        let (mut c, t0) = started_bare("ls");
        c.on_output(b"one\r\nls\r\ntwo\r\n", ms(t0, 2120));
        let out = c.tick(ms(t0, 2200));
        assert_eq!(texts(&out), "one\ntwo\n", "second line: still the echo");

        // With the prompt that was on screen when it was typed: dropped.
        let (mut c, t0, _) = started("ls");
        c.on_output(b"ls\r\nfile\r\n", ms(t0, 120));
        let out = c.tick(ms(t0, 200));
        assert_eq!(texts(&out), "file\n");

        // A line merely ending with the typed text is output (the old rule dropped it).
        let (mut c, t0) = started_bare("ls");
        c.on_output(b"$ ls\r\n", ms(t0, 2120));
        let out = c.tick(ms(t0, 2200));
        assert_eq!(texts(&out), "$ ls\n");
    }

    /// A session whose `> ` prompt was already shown as output (idle passed) with turn 1
    /// typed: the lines that follow start on a clean line.
    fn started_bare(text: &str) -> (Core, Instant) {
        let t0 = Instant::now();
        let mut c = Core::new(t(), t0);
        c.on_output(b"> ", ms(t0, 10));
        c.tick(ms(t0, 1010));
        let out = c.send(text, 1, ms(t0, 2000));
        assert_eq!(out.writes.len(), 1);
        (c, t0)
    }

    #[test]
    fn a_redrawn_prompt_keeps_the_question() {
        let (mut c, t0, _) = started("go");
        c.on_output(b"go\r\nContinue? [y/N] ", ms(t0, 120));
        let out = c.tick(ms(t0, 1120));
        assert_eq!(kinds(&out).last().unwrap(), "attention.raised@Some(1)");
        let id = c.open_question().unwrap().to_string();
        // The same prompt drawn again, in one chunk or split: still the same question.
        let out = c.on_output(b"\rContinue? [y/N] ", ms(t0, 1500));
        assert!(out.events.is_empty(), "{out:?}");
        let out = c.on_output(b"\r\x1b[K", ms(t0, 1600));
        assert!(out.events.is_empty(), "{out:?}");
        let out = c.on_output(b"Continue? [y/N] ", ms(t0, 1610));
        assert!(out.events.is_empty(), "{out:?}");
        let out = c.tick(ms(t0, 3000));
        assert!(
            !kinds(&out).iter().any(|k| k.starts_with("attention")),
            "not raised again: {out:?}"
        );
        assert_eq!(c.open_question(), Some(id.as_str()));
        // A different unfinished line withdraws it.
        let out = c.on_output(b"\rContinue? [y/N] (5s) ", ms(t0, 3100));
        assert_eq!(kinds(&out), ["attention.resolved@Some(1)"]);
        assert_eq!(c.open_question(), None);

        // So does a completed line.
        let (mut c, t0, _) = started("go");
        c.on_output(b"go\r\nName? ", ms(t0, 120));
        c.tick(ms(t0, 1120));
        let out = c.on_output(b"\r\n", ms(t0, 1200));
        assert!(out.events.is_empty(), "a blank line is not output");
        let out = c.on_output(b"using default\r\n", ms(t0, 1300));
        assert_eq!(kinds(&out), ["attention.resolved@Some(1)"]);
    }

    #[test]
    fn answers() {
        assert_eq!(answer_text(&Answer::allow()), "y");
        assert_eq!(answer_text(&Answer::deny()), "n");
        assert_eq!(
            answer_text(&Answer {
                decision: "allow".into(),
                message: Some("yes please".into()),
                ..Default::default()
            }),
            "yes please"
        );
        assert_eq!(answer_text(&Answer::new("bob")), "bob");
        assert_eq!(answer_text(&Answer::new("")), "");
    }
}

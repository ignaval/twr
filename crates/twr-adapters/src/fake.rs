//! Fake adapter. Tier: headless (structured events, no real process). For tests and demos.
//!
//! Not offered by [`crate::registry`] / [`crate::find`] unless `TWR_ENABLE_FAKE=1`
//! ([`crate::ENABLE_FAKE_ENV`]): `!write` writes files, so a production hub must not run it
//! for anyone who can name a runtime. Tests add it explicitly (`crate::registry_with(true)`
//! or their own adapter lookup).
//!
//! Deterministic: a given prompt always yields the same event sequence (ids derive from the
//! session id and turn number). A prompt is processed line by line; lines starting with a
//! directive do something special, other lines are echoed as `agent.text` deltas (one per
//! word) followed by a final `agent.text` with the whole block, and the turn ends `completed`.
//!
//! | directive              | effect                                                                     |
//! |------------------------|----------------------------------------------------------------------------|
//! | `!perm <tool> [input]` | risky `agent.tool_call` + `attention.raised{permission}`, waits for respond; allow -> `tool_result{ok:true}`, anything else -> `ok:false` |
//! | `!ask <q>\|a\|b`       | `attention.raised{question, options:[a,b]}`, waits; then `agent.text` `"answer: <decision>"` |
//! | `!tool <tool> [input]` | non-risky `agent.tool_call` + ok `agent.tool_result`                       |
//! | `!think <text>`        | `agent.thinking` (only when `SpawnConfig::thinking`)                       |
//! | `!long <bytes>`        | a final `agent.text` of that many `x` bytes (payload-cap tests)            |
//! | `!sleep <ms>`          | pause (commands keep being accepted)                                       |
//! | `!error [msg]`         | end the turn `error` (`ext.fake.message`), skip the rest                   |
//! | `!transient [n]`       | end the turn `error` flagged transient (`ext.fake.transient`), skip the rest; with `n > 1` the session's next `n - 1` turns fail the same way whatever their prompt |
//! | `!giveup`              | final text "I couldn't finish", end `completed`, skip the rest             |
//! | `!crash`               | exit with code 1 (no `turn.ended`)                                         |
//! | `!deaf`                | the session ignores the first interrupt of this turn, whether it comes before the turn began or during it (as a runtime that has not begun a turn may); emits nothing |
//! | `!write <file> [text]` | writes `text` plus a newline to `<file>` under the session's `cwd` (relative path without `..`; parent directories created), as a non-risky `Write` `agent.tool_call` + `agent.tool_result` (`ok:false` when it cannot) |
//! | `!compact [manual] [ms]` | `context.compacting`, a pause of `ms` (commands keep being accepted), then `context.compacted{pre_tokens: 2000, post_tokens: 500}`, trigger `auto` (or `manual`); the turn's end then reports a context of 500 tokens |
//! | `!context <tokens> [window]` | the turn's end reports a context of `tokens` (window default [`CONTEXT_WINDOW`]) |
//! | `!cache <ttl_s>\|- [ago_s]` | the turn's end reports a prompt cache ([`PromptCache`]) written `ago_s` seconds ago (default 0) that lives `ttl_s` seconds (`-`: unknown) |
//!
//! [`AdapterCmd::Compact`] runs `!compact manual` as the turn, when the adapter was made
//! [`FakeAdapter::with_compaction`] (it then [`Adapter::compacts`]); otherwise the turn ends
//! `error`.
//!
//! Sessions: the first event is `thread.started{session_id}` (a ULID) with
//! `ext.fake.transcript_len`, the number of turns the session has finished, and
//! `ext.fake.model`, the model it "runs" (`SpawnConfig::model`, else [`DEFAULT_MODEL`]), and
//! `ext.fake.mode`, the permission mode it was started with (`SpawnConfig::mode`; the fake
//! applies none, the hub's mode-change tests read it), and `ext.fake.effort`, the reasoning
//! effort it was started with (`SpawnConfig::effort`, null without one; for the hub's
//! model-change tests), and `ext.fake.codex_auto` (`SpawnConfig::codex_auto`).
//! Resuming a session id restores that count from a process-global map ([`transcript_len`]);
//! unknown ids start at zero. `send` while a turn runs is queued and runs after it. `stop` exits with code 0 and no
//! `turn.ended` (the hub closes the open turn), like a killed runtime; it also discards queued
//! prompts that have not started. `respond` for an attn_id the session is not waiting on
//! (unknown or already answered) is ignored.
//!
//! [`FakeAdapter::scripted`] replays exact [`Step`]s instead of interpreting prompts.
//!
//! Attach (`twr attach`): the interactive CLI is `sh -c 'eval "${TWR_FAKE_ATTACH:-exit 0}"'
//! twr-fake-attach <session_id>` in the thread's directory, so a test sets `TWR_FAKE_ATTACH`
//! in the attaching CLI's environment to script what the "user" does (`$1` is the session id).

use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::mpsc::error::TryRecvError;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer, AttachApplies,
    SpawnConfig, Tier,
};
use twr_core::event::*;

/// Adapter name (`source = adapter:fake`).
pub const NAME: &str = "fake";

/// The model a fake session reports when none was requested (`ext.fake.model`).
pub const DEFAULT_MODEL: &str = "fake-1";

/// Final text emitted by `!giveup`.
pub const GIVE_UP_TEXT: &str = "I couldn't finish";

/// The context window a fake session reports (`!context`, `!compact`).
pub const CONTEXT_WINDOW: u64 = 100_000;

/// The turn a compaction runs as ([`AdapterCmd::Compact`]).
const COMPACT_PROMPT: &str = "!compact manual";

static SESSIONS: LazyLock<Mutex<HashMap<String, u64>>> = LazyLock::new(Default::default);

/// Transient failures still owed per session (`!transient n`), process-global so that they
/// survive a resume.
static TRANSIENT_LEFT: LazyLock<Mutex<HashMap<String, u32>>> = LazyLock::new(Default::default);

fn transient_left() -> std::sync::MutexGuard<'static, HashMap<String, u32>> {
    TRANSIENT_LEFT.lock().unwrap_or_else(|e| e.into_inner())
}

/// `turn.ended` `ext` of a transient failure.
fn transient_ext(msg: &str) -> Map<String, Value> {
    let mut ext = Map::new();
    ext.insert(NAME.into(), json!({ "message": msg, "transient": true }));
    ext
}

/// Number of turns finished in a fake session (process-global), if the session exists.
pub fn transcript_len(session_id: &str) -> Option<u64> {
    SESSIONS.lock().ok()?.get(session_id).copied()
}

fn sessions() -> std::sync::MutexGuard<'static, HashMap<String, u64>> {
    SESSIONS.lock().unwrap_or_else(|e| e.into_inner())
}

/// One step of a scripted fake session.
// Scripts are small test fixtures; boxing `Emit` would only make them noisier to write.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Emit a body tagged with the current turn.
    Emit(EventBody),
    /// Session-scoped or autonomous event without a turn stamp.
    EmitSession(EventBody),
    /// Pause.
    Sleep(u64),
    /// Wait for `respond` on this attn_id.
    AwaitRespond(String),
    /// Wait for the next `send` (becomes the current turn).
    AwaitSend,
    /// Exit the process.
    Exit {
        /// Exit code.
        code: Option<i32>,
        /// Stderr tail.
        stderr_tail: String,
    },
}

/// The fake runtime adapter.
#[derive(Debug, Clone, Default)]
pub struct FakeAdapter {
    script: Option<Arc<Vec<Step>>>,
    /// [`AdapterCmd::Compact`] runs a compaction ([`FakeAdapter::with_compaction`]).
    compaction: bool,
}

impl FakeAdapter {
    /// Prompt-directive mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripted mode: every spawned session plays `steps` exactly (no `thread.started` or
    /// `turn.ended` is added), then waits for `stop`.
    pub fn scripted(steps: Vec<Step>) -> Self {
        FakeAdapter {
            script: Some(Arc::new(steps)),
            compaction: false,
        }
    }

    /// The same adapter, compacting a session's context on [`AdapterCmd::Compact`] (as
    /// `!compact manual`); without it, the fake runtime is one that cannot.
    pub fn with_compaction(mut self) -> Self {
        self.compaction = true;
        self
    }
}

impl Adapter for FakeAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Headless,
            description: "Deterministic fake runtime driven by prompt directives (tests)",
        }
    }

    /// No MCP: the fake runtime cannot host the orchestrator (`twr orchestrator use fake` is
    /// refused; a test may still configure it as `[orchestrator] runtime`).
    fn hosts_orchestrator(&self) -> bool {
        false
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (handle, end) = AdapterHandle::pair();
        let first_turn = cfg.prompt_turn.unwrap_or(1);
        let session_id = cfg
            .resume_session
            .clone()
            .unwrap_or_else(|| ulid::Ulid::generate().to_string());
        let mut s = Session {
            end,
            session_id,
            thinking: cfg.thinking,
            cwd: cfg.cwd.clone(),
            model: cfg
                .model
                .clone()
                .filter(|m| !m.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_MODEL.into()),
            mode: cfg.mode.clone(),
            effort: cfg.effort.clone().filter(|e| !e.trim().is_empty()),
            codex_auto: cfg.codex_auto.as_str(),
            queue: VecDeque::new(),
            compaction: self.compaction,
            interruptible: self.script.is_none(),
            deaf: false,
            deaf_used: None,
        };
        if let Some(p) = prompt {
            s.queue.push_back((p, first_turn));
        }
        match &self.script {
            Some(script) => {
                let script = Arc::clone(script);
                tokio::spawn(async move { s.run_script(&script).await });
            }
            None => {
                let resumed = cfg.resume_session.is_some();
                tokio::spawn(async move { s.run(resumed).await });
            }
        }
        Ok(handle)
    }

    /// Only an interpreting session stops a turn on `interrupt`; a scripted one replays its
    /// steps to the end.
    fn interrupts_turns(&self) -> bool {
        self.script.is_none()
    }

    fn compacts(&self) -> bool {
        self.compaction && self.script.is_none()
    }

    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("sh");
        cmd.args([
            "-c",
            "eval \"${TWR_FAKE_ATTACH:-exit 0}\"",
            "twr-fake-attach",
            session_id,
        ]);
        crate::process::apply_env_std(&mut cmd, crate::process::attach_env(cfg), &cfg.env_remove);
        if !cfg.cwd.as_os_str().is_empty() {
            cmd.current_dir(&cfg.cwd);
        }
        Some(cmd)
    }

    /// Both: the fake runtime keeps nothing of a session (`crate::resume_keeps`), so its
    /// stand-in interactive CLI runs in whatever the thread says, as an attach that hands the
    /// settings over would.
    fn attach_applies(&self, _cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies {
            model: true,
            effort: true,
        }
    }
}

/// Why a session loop must stop early.
enum Halt {
    Interrupt,
    /// `stop` received or the command channel closed.
    Stop,
    /// `Exited` already sent (crash).
    Exited,
    /// The hub dropped the event receiver.
    Closed,
}

type Flow<T = ()> = Result<T, Halt>;

struct Session {
    end: AdapterEnd,
    session_id: String,
    thinking: bool,
    /// Where `!write` writes (`SpawnConfig::cwd`).
    cwd: PathBuf,
    /// Reported in `thread.started` `ext.fake.model`.
    model: String,
    /// Reported in `thread.started` `ext.fake.mode` (the fake applies no mode).
    mode: String,
    /// Reported in `thread.started` `ext.fake.effort` (null: none asked for).
    effort: Option<String>,
    /// Reported in `thread.started` `ext.fake.codex_auto` (`SpawnConfig::codex_auto`).
    codex_auto: &'static str,
    queue: VecDeque<(String, u64)>,
    /// [`FakeAdapter::with_compaction`].
    compaction: bool,
    interruptible: bool,
    /// `!deaf`: the next interrupt is ignored.
    deaf: bool,
    /// The turn whose `!deaf` ignored its interrupt already.
    deaf_used: Option<u64>,
}

impl Session {
    fn emit(&self, body: EventBody, turn: Option<u64>) -> Flow {
        if self.end.emit(body, turn) {
            Ok(())
        } else {
            Err(Halt::Closed)
        }
    }

    /// Handles a command that arrived while busy. Returns the answer if it matches `waiting_for`.
    fn on_busy_cmd(
        &mut self,
        cmd: Option<AdapterCmd>,
        waiting_for: Option<&str>,
    ) -> Flow<Option<Answer>> {
        match cmd {
            Some(AdapterCmd::Send { text, turn }) => {
                self.queue.push_back((text, turn));
                Ok(None)
            }
            Some(AdapterCmd::Compact { turn }) => {
                let prompt = match self.compaction {
                    true => COMPACT_PROMPT,
                    false => "!error the fake runtime cannot compact its context",
                };
                self.queue.push_back((prompt.to_string(), turn));
                Ok(None)
            }
            Some(AdapterCmd::Respond { attn_id, answer }) => {
                if waiting_for == Some(attn_id.as_str()) {
                    Ok(Some(answer))
                } else {
                    tracing::debug!(%attn_id, "fake: ignoring respond for unknown attention");
                    Ok(None)
                }
            }
            Some(AdapterCmd::Interrupt) if self.interruptible && self.deaf => {
                self.deaf = false;
                Ok(None)
            }
            Some(AdapterCmd::Interrupt) if self.interruptible => Err(Halt::Interrupt),
            Some(AdapterCmd::Interrupt) => {
                tracing::warn!("fake: scripted turn interruption is unsupported; session retained");
                Ok(None)
            }
            Some(AdapterCmd::Stop) | None => Err(Halt::Stop),
        }
    }

    async fn await_respond(&mut self, attn_id: &str) -> Flow<Answer> {
        loop {
            let cmd = self.end.cmds.recv().await;
            if let Some(answer) = self.on_busy_cmd(cmd, Some(attn_id))? {
                return Ok(answer);
            }
        }
    }

    async fn sleep(&mut self, ms: u64) -> Flow {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(ms);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Ok(()),
                cmd = self.end.cmds.recv() => { self.on_busy_cmd(cmd, None)?; }
            }
        }
    }

    /// Waits for the next prompt when idle. Commands that arrived while the previous turn ran
    /// are handled before the queue is popped, so a `stop` sent after a `send` preempts the
    /// queued prompt instead of running it first.
    async fn next_prompt(&mut self) -> Flow<(String, u64)> {
        loop {
            loop {
                match self.end.cmds.try_recv() {
                    Ok(cmd) => {
                        self.on_busy_cmd(Some(cmd), None)?;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Err(Halt::Stop),
                }
            }
            if let Some(p) = self.queue.pop_front() {
                return Ok(p);
            }
            let cmd = self.end.cmds.recv().await;
            self.on_busy_cmd(cmd, None)?;
        }
    }

    fn finish(&self, halt: Halt) {
        match halt {
            Halt::Stop => {
                self.end.exited(Some(0), "");
            }
            Halt::Interrupt | Halt::Exited | Halt::Closed => {}
        }
    }

    // --- directive mode ----------------------------------------------------------------------

    async fn run(mut self, resumed: bool) {
        let len = *sessions().entry(self.session_id.clone()).or_insert(0);
        let mut ext = Map::new();
        ext.insert(
            NAME.into(),
            json!({"transcript_len": len, "resumed": resumed, "model": self.model,
                   "mode": self.mode, "effort": self.effort,
                   "codex_auto": self.codex_auto}),
        );
        let started = EventBody::ThreadStarted(SessionPayload {
            session_id: Some(self.session_id.clone()),
            ext: Some(ext),
            ..Default::default()
        });
        if let Err(h) = self.emit(started, None) {
            return self.finish(h);
        }
        loop {
            let flow = match self.next_prompt().await {
                Ok((prompt, turn)) => match self.run_turn(&prompt, turn).await {
                    Err(Halt::Interrupt) => self.emit(
                        EventBody::TurnEnded(TurnEnded {
                            result: TurnResult::Interrupted,
                            ..Default::default()
                        }),
                        Some(turn),
                    ),
                    result => result,
                },
                // An interrupt that comes before its turn began (the hub sent the prompt, then
                // the interrupt): the hub's open turn is the oldest prompt not started yet, and
                // it ends before it begins. Idle, there is nothing to interrupt.
                // A `!deaf` prompt ignores that first interrupt and runs.
                Err(Halt::Interrupt)
                    if self.queue.front().is_some_and(|(p, turn)| {
                        p.trim_start().starts_with("!deaf") && self.deaf_used != Some(*turn)
                    }) =>
                {
                    self.deaf_used = self.queue.front().map(|(_, turn)| *turn);
                    Ok(())
                }
                Err(Halt::Interrupt) => match self.queue.pop_front() {
                    Some((_, turn)) => self.emit(
                        EventBody::TurnEnded(TurnEnded {
                            result: TurnResult::Interrupted,
                            ..Default::default()
                        }),
                        Some(turn),
                    ),
                    None => continue,
                },
                Err(h) => Err(h),
            };
            if let Err(h) = flow {
                return self.finish(h);
            }
        }
    }

    async fn run_turn(&mut self, prompt: &str, turn: u64) -> Flow {
        let t = Some(turn);
        self.deaf = false;
        let mut text_block: Vec<&str> = Vec::new();
        let mut words_out = 0u64;
        let mut slept = 0u64;
        let mut n = 0u64;
        let mut result = TurnResult::Completed;
        let mut end_ext: Option<Map<String, Value>> = None;
        let mut context: Option<ContextSize> = None;
        let mut prompt_cache: Option<PromptCache> = None;
        let owed = {
            let mut left = transient_left();
            match left.get_mut(&self.session_id) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    true
                }
                _ => false,
            }
        };
        let lines: Vec<&str> = if owed {
            result = TurnResult::Error;
            end_ext = Some(transient_ext("fake transient error (again)"));
            Vec::new()
        } else {
            prompt.lines().collect()
        };

        for line in lines {
            let trimmed = line.trim();
            let (directive, rest) = match trimmed.strip_prefix('!') {
                Some(d) => {
                    let (name, rest) = d.split_once(char::is_whitespace).unwrap_or((d, ""));
                    (name, rest.trim())
                }
                None => ("", trimmed),
            };
            let known = matches!(
                directive,
                "perm"
                    | "ask"
                    | "tool"
                    | "think"
                    | "long"
                    | "sleep"
                    | "error"
                    | "transient"
                    | "giveup"
                    | "crash"
                    | "write"
                    | "deaf"
                    | "compact"
                    | "context"
                    | "cache"
            );
            if !known {
                if !trimmed.is_empty() {
                    text_block.push(trimmed);
                }
                continue;
            }
            words_out += self.flush_text(&mut text_block, t)?;
            n += 1;
            match directive {
                "deaf" => {
                    if self.deaf_used != Some(turn) {
                        self.deaf = true;
                        self.deaf_used = Some(turn);
                    }
                }
                "perm" => {
                    let (tool, input) = split_word(rest);
                    let tool = if tool.is_empty() { "Bash" } else { tool };
                    let call_id = format!("t{turn}-c{n}");
                    let attn_id = format!("{}-t{turn}-a{n}", self.session_id);
                    self.emit(tool_call(&call_id, tool, input, true), t)?;
                    self.emit(
                        EventBody::AttentionRaised(AttentionRaised {
                            attn_id: attn_id.clone(),
                            reason: AttentionReason::Permission,
                            summary: format!("Allow {tool}?"),
                            options: vec!["allow".into(), "deny".into()],
                            call_id: Some(call_id.clone()),
                            ..Default::default()
                        }),
                        t,
                    )?;
                    let answer = self.await_respond(&attn_id).await?;
                    let ok = answer.is_allow();
                    self.emit(
                        tool_result(&call_id, ok, if ok { "ok" } else { "denied" }),
                        t,
                    )?;
                }
                "ask" => {
                    let mut parts = rest.split('|').map(str::trim);
                    let question = parts.next().unwrap_or_default().to_string();
                    let options: Vec<String> = parts
                        .filter(|o| !o.is_empty())
                        .map(str::to_string)
                        .collect();
                    let attn_id = format!("{}-t{turn}-a{n}", self.session_id);
                    self.emit(
                        EventBody::AttentionRaised(AttentionRaised {
                            attn_id: attn_id.clone(),
                            reason: AttentionReason::Question,
                            summary: question,
                            options,
                            ..Default::default()
                        }),
                        t,
                    )?;
                    let answer = self.await_respond(&attn_id).await?;
                    let reply = format!("answer: {}", answer.decision);
                    words_out += self.echo(&reply, t)?;
                }
                "tool" => {
                    let (tool, input) = split_word(rest);
                    let tool = if tool.is_empty() { "Read" } else { tool };
                    let call_id = format!("t{turn}-c{n}");
                    self.emit(tool_call(&call_id, tool, input, false), t)?;
                    self.emit(tool_result(&call_id, true, "ok"), t)?;
                }
                "think" => {
                    if self.thinking {
                        self.emit(
                            EventBody::AgentThinking(AgentThinking {
                                text: rest.to_string(),
                                ..Default::default()
                            }),
                            t,
                        )?;
                    }
                }
                "long" => {
                    let len: usize = rest.parse().unwrap_or(8192);
                    self.emit(final_text("x".repeat(len)), t)?;
                    words_out += 1;
                }
                "sleep" => {
                    let ms: u64 = rest.parse().unwrap_or(0);
                    self.sleep(ms).await?;
                    slept += ms;
                }
                "compact" => {
                    let words: Vec<&str> = rest.split_whitespace().collect();
                    let trigger = if words.contains(&"manual") {
                        "manual"
                    } else {
                        "auto"
                    };
                    let ms: u64 = words.iter().find_map(|w| w.parse().ok()).unwrap_or(0);
                    self.emit(
                        EventBody::ContextCompacting(ContextCompacting {
                            trigger: Some(trigger.into()),
                            ..Default::default()
                        }),
                        t,
                    )?;
                    self.sleep(ms).await?;
                    slept += ms;
                    self.emit(
                        EventBody::ContextCompacted(ContextCompacted {
                            trigger: trigger.into(),
                            pre_tokens: Some(2000),
                            post_tokens: Some(500),
                            duration_ms: Some(ms),
                            ..Default::default()
                        }),
                        t,
                    )?;
                    context = Some(ContextSize {
                        tokens: 500,
                        window: Some(CONTEXT_WINDOW),
                    });
                }
                "context" => {
                    let mut words = rest.split_whitespace().filter_map(|w| w.parse().ok());
                    let tokens = words.next().unwrap_or(0);
                    let window = words.next().unwrap_or(CONTEXT_WINDOW);
                    context = Some(ContextSize {
                        tokens,
                        window: Some(window),
                    });
                }
                "cache" => {
                    let (ttl, ago) = split_word(rest);
                    let ago: i64 = ago.parse().unwrap_or(0);
                    prompt_cache = Some(PromptCache {
                        called_at: chrono::Utc::now() - chrono::Duration::seconds(ago),
                        ttl_s: ttl.parse().ok(),
                    });
                }
                "error" => {
                    result = TurnResult::Error;
                    let msg = if rest.is_empty() { "fake error" } else { rest };
                    let mut ext = Map::new();
                    ext.insert(NAME.into(), json!({ "message": msg }));
                    end_ext = Some(ext);
                    break;
                }
                "transient" => {
                    result = TurnResult::Error;
                    let n: u32 = rest.parse().unwrap_or(1).max(1);
                    if n > 1 {
                        transient_left().insert(self.session_id.clone(), n - 1);
                    }
                    end_ext = Some(transient_ext("fake transient error"));
                    break;
                }
                "giveup" => {
                    words_out += self.echo(GIVE_UP_TEXT, t)?;
                    break;
                }
                "crash" => {
                    self.end.exited(Some(1), "fake: crash requested");
                    return Err(Halt::Exited);
                }
                "write" => {
                    let (file, text) = split_word(rest);
                    let call_id = format!("t{turn}-c{n}");
                    self.emit(tool_call(&call_id, "Write", file, false), t)?;
                    let out = match write_in(&self.cwd, file, text) {
                        Ok(()) => tool_result(&call_id, true, "written"),
                        Err(e) => tool_result(&call_id, false, &e),
                    };
                    self.emit(out, t)?;
                }
                _ => unreachable!("directive list above"),
            }
        }
        words_out += self.flush_text(&mut text_block, t)?;

        let finished = {
            let mut map = sessions();
            let c = map.entry(self.session_id.clone()).or_insert(0);
            *c += 1;
            *c
        };
        tracing::trace!(session = %self.session_id, finished, "fake turn done");
        self.emit(
            EventBody::TurnEnded(TurnEnded {
                result,
                session_id: Some(self.session_id.clone()),
                usage: Some(Usage {
                    input: prompt.split_whitespace().count() as u64,
                    output: words_out,
                    cost: Some(0.0),
                }),
                duration_ms: Some(slept),
                context,
                prompt_cache,
                ext: end_ext,
                ..Default::default()
            }),
            t,
        )
    }

    /// Echoes the accumulated text lines as one message. Returns words emitted.
    fn flush_text(&self, block: &mut Vec<&str>, turn: Option<u64>) -> Flow<u64> {
        if block.is_empty() {
            return Ok(0);
        }
        let text = block.join("\n");
        block.clear();
        self.echo(&text, turn)
    }

    /// Emits one delta per word (whitespace kept) and then the final text.
    fn echo(&self, text: &str, turn: Option<u64>) -> Flow<u64> {
        let mut words = 0;
        for chunk in text.split_inclusive(char::is_whitespace) {
            words += 1;
            self.emit(
                EventBody::AgentText(AgentText {
                    text: chunk.to_string(),
                    delta: true,
                    ..Default::default()
                }),
                turn,
            )?;
        }
        self.emit(final_text(text.to_string()), turn)?;
        Ok(words)
    }

    // --- scripted mode -----------------------------------------------------------------------

    async fn run_script(mut self, script: &[Step]) {
        let mut turn = self.queue.pop_front().map(|(_, t)| t);
        for step in script {
            let flow = match step {
                Step::Emit(body) => self.emit(body.clone(), turn),
                Step::EmitSession(body) => self.emit(body.clone(), None),
                Step::Sleep(ms) => self.sleep(*ms).await,
                Step::AwaitRespond(id) => self.await_respond(id).await.map(|_| ()),
                Step::AwaitSend => self.next_prompt().await.map(|(_, t)| turn = Some(t)),
                Step::Exit { code, stderr_tail } => {
                    self.end.exited(*code, stderr_tail.clone());
                    Err(Halt::Exited)
                }
            };
            if let Err(h) = flow {
                return self.finish(h);
            }
        }
        loop {
            match self.end.cmds.recv().await {
                Some(AdapterCmd::Stop) | None => return self.finish(Halt::Stop),
                Some(_) => {}
            }
        }
    }
}

/// `!write`: writes `text` and a newline to `file` (relative, no `..`) under `cwd`.
fn write_in(cwd: &Path, file: &str, text: &str) -> Result<(), String> {
    let rel = Path::new(file);
    if file.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!(
            "refusing to write {file:?}: not a plain relative path"
        ));
    }
    if cwd.as_os_str().is_empty() {
        return Err("the session has no working directory".into());
    }
    let path = cwd.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&path, format!("{text}\n")).map_err(|e| format!("{}: {e}", path.display()))
}

fn split_word(s: &str) -> (&str, &str) {
    match s.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (s, ""),
    }
}

fn final_text(text: String) -> EventBody {
    EventBody::AgentText(AgentText {
        text,
        is_final: true,
        ..Default::default()
    })
}

fn tool_call(call_id: &str, tool: &str, input: &str, risky: bool) -> EventBody {
    EventBody::AgentToolCall(AgentToolCall {
        call_id: call_id.into(),
        tool: tool.into(),
        input_summary: input.into(),
        risky,
        ..Default::default()
    })
}

fn tool_result(call_id: &str, ok: bool, summary: &str) -> EventBody {
    EventBody::AgentToolResult(AgentToolResult {
        call_id: call_id.into(),
        ok,
        output_summary: summary.into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tokio::time::timeout;
    use twr_core::adapter::AdapterEvent;

    type Rx = UnboundedReceiver<AdapterEvent>;

    const WAIT: Duration = Duration::from_secs(5);

    fn spawn(prompt: Option<&str>) -> (AdapterHandle, Rx) {
        spawn_with(FakeAdapter::new(), SpawnConfig::default(), prompt)
    }

    fn spawn_with(a: FakeAdapter, cfg: SpawnConfig, prompt: Option<&str>) -> (AdapterHandle, Rx) {
        let mut h = a.spawn(cfg, prompt.map(str::to_string)).unwrap();
        let rx = h.take_events().unwrap();
        (h, rx)
    }

    async fn next(rx: &mut Rx) -> AdapterEvent {
        timeout(WAIT, rx.recv())
            .await
            .expect("timed out waiting for fake event")
            .expect("fake event channel closed")
    }

    /// Next `Emit`'s body and turn.
    async fn body(rx: &mut Rx) -> (EventBody, Option<u64>) {
        match next(rx).await {
            AdapterEvent::Emit { body, turn, .. } => (body, turn),
            other => panic!("expected Emit, got {other:?}"),
        }
    }

    /// Collects bodies up to and including `turn.ended`.
    async fn until_turn_end(rx: &mut Rx) -> Vec<(EventBody, Option<u64>)> {
        let mut out = Vec::new();
        loop {
            let (b, t) = body(rx).await;
            let done = matches!(b, EventBody::TurnEnded(_));
            out.push((b, t));
            if done {
                return out;
            }
        }
    }

    async fn started(rx: &mut Rx) -> SessionPayload {
        match body(rx).await {
            (EventBody::ThreadStarted(p), None) => p,
            other => panic!("expected thread.started, got {other:?}"),
        }
    }

    fn kinds(evs: &[(EventBody, Option<u64>)]) -> Vec<&str> {
        evs.iter().map(|(b, _)| b.kind()).collect()
    }

    fn ended(evs: &[(EventBody, Option<u64>)]) -> &TurnEnded {
        match evs.last() {
            Some((EventBody::TurnEnded(e), _)) => e,
            other => panic!("expected turn.ended last, got {other:?}"),
        }
    }

    fn final_texts(evs: &[(EventBody, Option<u64>)]) -> Vec<String> {
        evs.iter()
            .filter_map(|(b, _)| match b {
                EventBody::AgentText(t) if t.is_final => Some(t.text.clone()),
                _ => None,
            })
            .collect()
    }

    fn raised(evs: &[(EventBody, Option<u64>)]) -> AttentionRaised {
        evs.iter()
            .find_map(|(b, _)| match b {
                EventBody::AttentionRaised(a) => Some(a.clone()),
                _ => None,
            })
            .expect("attention.raised")
    }

    /// Reads events until an `attention.raised` (inclusive).
    async fn until_raised(rx: &mut Rx) -> Vec<(EventBody, Option<u64>)> {
        let mut out = Vec::new();
        loop {
            let (b, t) = body(rx).await;
            let done = matches!(b, EventBody::AttentionRaised(_));
            out.push((b, t));
            if done {
                return out;
            }
        }
    }

    #[test]
    fn attach_command_runs_the_scripted_shell() {
        let cfg = SpawnConfig {
            cwd: "/tmp".into(),
            ..Default::default()
        };
        let cmd = FakeAdapter::new().attach_command(&cfg, "s-1").unwrap();
        assert_eq!(cmd.get_program(), "sh");
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args[2], "twr-fake-attach");
        assert_eq!(args[3], "s-1");
        assert_eq!(cmd.get_current_dir(), Some(std::path::Path::new("/tmp")));
        // No configured variables: nothing is set, only twr's secrets are removed.
        assert!(cmd
            .get_envs()
            .all(|(k, v)| v.is_none() && crate::process::is_scrubbed(&k.to_string_lossy())));
    }

    #[tokio::test]
    async fn info_and_registry() {
        let info = FakeAdapter::new().info();
        assert_eq!(info.name, "fake");
        assert_eq!(info.tier, Tier::Headless);
        assert!(crate::registry_with(true)
            .iter()
            .any(|a| a.info().name == NAME));
    }

    #[tokio::test]
    async fn plain_text_echoes_deltas_then_final() {
        let (_h, mut rx) = spawn(Some("hello big world"));
        let s = started(&mut rx).await;
        assert!(s.session_id.is_some());
        assert_eq!(s.ext.as_ref().unwrap()["fake"]["transcript_len"], 0);
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(
            kinds(&evs),
            [
                "agent.text",
                "agent.text",
                "agent.text",
                "agent.text",
                "turn.ended"
            ]
        );
        let deltas: Vec<_> = evs
            .iter()
            .filter_map(|(b, _)| match b {
                EventBody::AgentText(t) if t.delta => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, ["hello ", "big ", "world"]);
        assert_eq!(final_texts(&evs), ["hello big world"]);
        assert!(evs.iter().all(|(_, t)| *t == Some(1)));
        let e = ended(&evs);
        assert_eq!(e.result, TurnResult::Completed);
        assert_eq!(e.session_id, s.session_id);
        assert_eq!(e.usage.as_ref().unwrap().input, 3);
        assert_eq!(e.usage.as_ref().unwrap().output, 3);
    }

    #[tokio::test]
    async fn prompt_turn_and_send_turn_numbers_are_echoed() {
        let cfg = SpawnConfig {
            prompt_turn: Some(7),
            ..Default::default()
        };
        let (h, mut rx) = spawn_with(FakeAdapter::new(), cfg, Some("one"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert!(evs.iter().all(|(_, t)| *t == Some(7)));
        h.send("two", 8).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert!(evs.iter().all(|(_, t)| *t == Some(8)));
        assert_eq!(final_texts(&evs), ["two"]);
    }

    #[tokio::test]
    async fn perm_allow_and_deny() {
        let (h, mut rx) = spawn(Some("!perm Bash rm -rf build\n!perm Write notes.md\ndone"));
        started(&mut rx).await;
        let evs = until_raised(&mut rx).await;
        assert_eq!(kinds(&evs), ["agent.tool_call", "attention.raised"]);
        let EventBody::AgentToolCall(call) = &evs[0].0 else {
            panic!()
        };
        assert_eq!(call.tool, "Bash");
        assert_eq!(call.input_summary, "rm -rf build");
        assert!(call.risky);
        let a = raised(&evs);
        assert_eq!(a.reason, AttentionReason::Permission);
        assert_eq!(a.options, ["allow", "deny"]);
        assert_eq!(a.call_id.as_deref(), Some(call.call_id.as_str()));

        // A respond for another id is ignored.
        h.respond("nope", Answer::allow()).unwrap();
        h.respond(&a.attn_id, Answer::allow()).unwrap();
        match body(&mut rx).await.0 {
            EventBody::AgentToolResult(r) => {
                assert!(r.ok);
                assert_eq!(r.call_id, call.call_id);
            }
            other => panic!("{other:?}"),
        }

        let evs = until_raised(&mut rx).await;
        let a2 = raised(&evs);
        assert_ne!(a2.attn_id, a.attn_id);
        h.respond(&a2.attn_id, Answer::deny()).unwrap();
        let evs = until_turn_end(&mut rx).await;
        match &evs[0].0 {
            EventBody::AgentToolResult(r) => assert!(!r.ok),
            other => panic!("{other:?}"),
        }
        assert_eq!(final_texts(&evs), ["done"]);
        assert_eq!(ended(&evs).result, TurnResult::Completed);
    }

    #[tokio::test]
    async fn ask_question_with_options() {
        let (h, mut rx) = spawn(Some("!ask Which db?|postgres|sqlite"));
        started(&mut rx).await;
        let evs = until_raised(&mut rx).await;
        let a = raised(&evs);
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.summary, "Which db?");
        assert_eq!(a.options, ["postgres", "sqlite"]);
        h.respond(&a.attn_id, Answer::new("sqlite")).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), ["answer: sqlite"]);
    }

    #[tokio::test]
    async fn error_ends_turn_with_error_and_skips_rest() {
        let (_h, mut rx) = spawn(Some("before\n!error rate limited\nafter"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), ["before"]);
        let e = ended(&evs);
        assert_eq!(e.result, TurnResult::Error);
        assert_eq!(e.ext.as_ref().unwrap()["fake"]["message"], "rate limited");
    }

    /// `!cache <ttl_s> [ago_s]`: the turn's end reports a prompt cache written `ago_s` seconds
    /// before it, living `ttl_s` (`-`: unknown); a turn without it reports none.
    #[tokio::test]
    async fn cache_reports_the_prompt_cache() {
        let before = chrono::Utc::now();
        let (h, mut rx) = spawn(Some("!cache 3600 600"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        let after = chrono::Utc::now();
        let ago = |s| chrono::Duration::seconds(s);
        let pc = ended(&evs).prompt_cache.expect("a prompt cache");
        assert_eq!(pc.ttl_s, Some(3600));
        assert!(
            before - ago(600) <= pc.called_at && pc.called_at <= after - ago(600),
            "{pc:?}"
        );
        h.send("!cache -", 2).unwrap();
        let evs = until_turn_end(&mut rx).await;
        let pc = ended(&evs).prompt_cache.expect("a prompt cache");
        assert_eq!(pc.ttl_s, None);
        assert!(pc.called_at <= chrono::Utc::now());
        h.send("plain", 3).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(ended(&evs).prompt_cache, None);
    }

    #[tokio::test]
    async fn transient_errors_are_flagged_and_can_repeat() {
        let (h, mut rx) = spawn(Some("before\n!transient 2\nafter"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), ["before"]);
        let e = ended(&evs);
        assert_eq!(e.result, TurnResult::Error);
        assert_eq!(e.ext.as_ref().unwrap()["fake"]["transient"], true);
        // The next turn fails the same way whatever its prompt; the one after runs.
        h.send("continue", 2).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert!(final_texts(&evs).is_empty());
        assert_eq!(ended(&evs).ext.as_ref().unwrap()["fake"]["transient"], true);
        h.send("continue", 3).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), ["continue"]);
        assert_eq!(ended(&evs).result, TurnResult::Completed);
    }

    #[tokio::test]
    async fn crash_exits_with_code_1() {
        let (_h, mut rx) = spawn(Some("working\n!crash\nnever"));
        started(&mut rx).await;
        let mut texts = Vec::new();
        loop {
            match next(&mut rx).await {
                AdapterEvent::Emit { body, .. } => {
                    assert_ne!(body.kind(), "turn.ended");
                    texts.push(body);
                }
                AdapterEvent::Exited { code, stderr_tail } => {
                    assert_eq!(code, Some(1));
                    assert!(stderr_tail.contains("crash"));
                    break;
                }
            }
        }
        assert!(!texts.is_empty());
        assert!(
            timeout(WAIT, rx.recv()).await.unwrap().is_none(),
            "nothing after Exited"
        );
    }

    #[tokio::test]
    async fn giveup_text() {
        let (_h, mut rx) = spawn(Some("!giveup\nnot echoed"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), [GIVE_UP_TEXT]);
        assert_eq!(ended(&evs).result, TurnResult::Completed);
    }

    #[tokio::test]
    async fn write_creates_files_under_the_cwd_only() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = SpawnConfig {
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        let prompt =
            "!write notes/a.txt hello world\n!write ../escape.txt no\n!write /abs.txt no\ndone";
        let (_h, mut rx) = spawn_with(FakeAdapter::new(), cfg, Some(prompt));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("notes/a.txt")).unwrap(),
            "hello world\n"
        );
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
        let results: Vec<bool> = evs
            .iter()
            .filter_map(|(b, _)| match b {
                EventBody::AgentToolResult(r) => Some(r.ok),
                _ => None,
            })
            .collect();
        assert_eq!(results, [true, false, false]);
        assert_eq!(final_texts(&evs), ["done"]);
        assert_eq!(ended(&evs).result, TurnResult::Completed);
        // Without a working directory nothing is written.
        let (_h, mut rx) = spawn(Some("!write x.txt"));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert!(evs
            .iter()
            .any(|(b, _)| matches!(b, EventBody::AgentToolResult(r) if !r.ok)));
    }

    #[tokio::test]
    async fn sleep_accepts_sends_and_queues_them() {
        let (h, mut rx) = spawn(Some("!sleep 100\nfirst"));
        started(&mut rx).await;
        // Sent while turn 1 sleeps: queued, runs after turn 1 ends.
        h.send("second", 2).unwrap();
        h.send("third", 3).unwrap();
        let t1 = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&t1), ["first"]);
        assert_eq!(ended(&t1).duration_ms, Some(100));
        let t2 = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&t2), ["second"]);
        assert!(t2.iter().all(|(_, t)| *t == Some(2)));
        let t3 = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&t3), ["third"]);
    }

    #[tokio::test]
    async fn send_while_waiting_for_permission_is_queued() {
        let (h, mut rx) = spawn(Some("!perm Bash ls"));
        started(&mut rx).await;
        let a = raised(&until_raised(&mut rx).await);
        h.send("next please", 2).unwrap();
        h.respond(&a.attn_id, Answer::allow()).unwrap();
        let t1 = until_turn_end(&mut rx).await;
        assert!(t1.iter().all(|(_, t)| *t == Some(1)));
        let t2 = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&t2), ["next please"]);
    }

    #[tokio::test]
    async fn stop_while_idle_and_while_waiting() {
        let (h, mut rx) = spawn(None);
        started(&mut rx).await;
        h.stop().unwrap();
        assert_eq!(
            next(&mut rx).await,
            AdapterEvent::Exited {
                code: Some(0),
                stderr_tail: String::new()
            }
        );

        let (h, mut rx) = spawn(Some("!perm Bash ls"));
        started(&mut rx).await;
        until_raised(&mut rx).await;
        h.stop().unwrap();
        assert!(matches!(
            next(&mut rx).await,
            AdapterEvent::Exited { code: Some(0), .. }
        ));
        // The adapter is gone now.
        assert!(timeout(WAIT, rx.recv()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stop_interrupts_sleep() {
        let (h, mut rx) = spawn(Some("!sleep 60000"));
        started(&mut rx).await;
        h.stop().unwrap();
        assert!(matches!(
            next(&mut rx).await,
            AdapterEvent::Exited { code: Some(0), .. }
        ));
    }

    /// An interrupt ends the running turn and keeps the session; one sent right after the
    /// prompt, before its turn began, ends that turn before it does anything; idle, it is
    /// nothing.
    #[tokio::test]
    async fn interrupt_ends_the_turn_and_keeps_the_session() {
        let (h, mut rx) = spawn(Some("!sleep 60000\n!tool Bash ls"));
        started(&mut rx).await;
        h.interrupt().unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(kinds(&evs), vec!["turn.ended"]);
        assert_eq!(ended(&evs).result, TurnResult::Interrupted);
        h.interrupt().unwrap();
        h.send("!tool Bash ls\n!sleep 60000", 2).unwrap();
        h.interrupt().unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(evs.last().unwrap().1, Some(2));
        assert_eq!(ended(&evs).result, TurnResult::Interrupted);
        h.send("still here", 3).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(ended(&evs).result, TurnResult::Completed);
        assert!(FakeAdapter::new().interrupts_turns());
    }

    /// `!deaf`: the first interrupt of the turn is ignored, before the turn began or during
    /// it; the next one ends it.
    #[tokio::test]
    async fn deaf_ignores_the_first_interrupt_of_its_turn() {
        let (h, mut rx) = spawn(Some("!deaf\nhello\n!sleep 60000\n!tool Bash ls"));
        h.interrupt().unwrap();
        started(&mut rx).await;
        let (b, _) = body(&mut rx).await;
        assert_eq!(b.kind(), "agent.text", "the first interrupt was ignored");
        h.interrupt().unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(ended(&evs).result, TurnResult::Interrupted);
        assert!(
            !kinds(&evs).contains(&"agent.tool_call"),
            "{:?}",
            kinds(&evs)
        );
        h.send("!deaf\n!sleep 60000\n!tool Bash ls", 2).unwrap();
        h.interrupt().unwrap();
        h.interrupt().unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(ended(&evs).result, TurnResult::Interrupted);
        assert_eq!(kinds(&evs), vec!["turn.ended"]);
    }

    #[tokio::test]
    async fn dropping_handle_stops_session() {
        let (h, mut rx) = spawn(None);
        started(&mut rx).await;
        drop(h);
        assert!(matches!(next(&mut rx).await, AdapterEvent::Exited { .. }));
    }

    #[tokio::test]
    async fn resume_restores_transcript_length() {
        let (h, mut rx) = spawn(Some("one"));
        let sid = started(&mut rx).await.session_id.unwrap();
        until_turn_end(&mut rx).await;
        h.send("two", 2).unwrap();
        until_turn_end(&mut rx).await;
        h.stop().unwrap();
        next(&mut rx).await;
        assert_eq!(transcript_len(&sid), Some(2));

        let mut h2 = FakeAdapter::new()
            .resume(SpawnConfig::default(), sid.clone())
            .unwrap();
        let mut rx2 = h2.take_events().unwrap();
        let s = started(&mut rx2).await;
        assert_eq!(s.session_id.as_deref(), Some(sid.as_str()));
        let ext = &s.ext.as_ref().unwrap()["fake"];
        assert_eq!(ext["transcript_len"], 2);
        assert_eq!(ext["resumed"], true);
        assert_eq!(ext["model"], DEFAULT_MODEL);
        h2.send("three", 3).unwrap();
        let evs = until_turn_end(&mut rx2).await;
        assert_eq!(ended(&evs).session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(transcript_len(&sid), Some(3));
    }

    #[tokio::test]
    async fn resume_unknown_session_starts_empty() {
        let mut h = FakeAdapter::new()
            .resume(SpawnConfig::default(), "no-such-session".into())
            .unwrap();
        let mut rx = h.take_events().unwrap();
        let s = started(&mut rx).await;
        assert_eq!(s.ext.as_ref().unwrap()["fake"]["transcript_len"], 0);
    }

    /// A session reports the requested model as the one it runs (`ext.fake.model`).
    #[tokio::test]
    async fn the_requested_model_is_reported() {
        let cfg = SpawnConfig {
            model: Some("fake-big".into()),
            ..Default::default()
        };
        let (_h, mut rx) = spawn_with(FakeAdapter::new(), cfg, None);
        let s = started(&mut rx).await;
        assert_eq!(s.ext.as_ref().unwrap()["fake"]["model"], "fake-big");
    }

    #[tokio::test]
    async fn think_respects_thinking_flag_and_tool_and_long() {
        let prompt = "!think pondering\n!tool Read src/main.rs\n!long 5000";
        let (_h, mut rx) = spawn(Some(prompt));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(
            kinds(&evs),
            [
                "agent.tool_call",
                "agent.tool_result",
                "agent.text",
                "turn.ended"
            ]
        );
        let EventBody::AgentToolCall(c) = &evs[0].0 else {
            panic!()
        };
        assert!(!c.risky);
        assert_eq!(c.input_summary, "src/main.rs");
        assert_eq!(final_texts(&evs)[0].len(), 5000);

        let cfg = SpawnConfig {
            thinking: true,
            ..Default::default()
        };
        let (_h, mut rx) = spawn_with(FakeAdapter::new(), cfg, Some(prompt));
        started(&mut rx).await;
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(kinds(&evs)[0], "agent.thinking");
    }

    #[tokio::test]
    async fn deterministic_for_same_prompt() {
        let prompt = "hi there\n!tool Grep foo\n!giveup";
        let strip = |evs: Vec<(EventBody, Option<u64>)>| -> Vec<Value> {
            evs.into_iter()
                .map(|(b, _)| {
                    let mut v = b.data().unwrap();
                    v.as_object_mut().unwrap().remove("session_id");
                    v
                })
                .collect()
        };
        let (_h1, mut rx1) = spawn(Some(prompt));
        started(&mut rx1).await;
        let a = strip(until_turn_end(&mut rx1).await);
        let (_h2, mut rx2) = spawn(Some(prompt));
        started(&mut rx2).await;
        let b = strip(until_turn_end(&mut rx2).await);
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn scripted_mode_plays_steps_exactly() {
        let script = vec![
            Step::Emit(EventBody::ThreadStarted(SessionPayload {
                session_id: Some("scripted".into()),
                ..Default::default()
            })),
            Step::AwaitSend,
            Step::Emit(final_text("hi".into())),
            Step::Emit(EventBody::AttentionRaised(AttentionRaised {
                attn_id: "x1".into(),
                ..Default::default()
            })),
            Step::AwaitRespond("x1".into()),
            Step::Sleep(10),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::Exit {
                code: Some(3),
                stderr_tail: "bye".into(),
            },
        ];
        let (h, mut rx) = spawn_with(FakeAdapter::scripted(script), SpawnConfig::default(), None);
        let (b, t) = body(&mut rx).await;
        assert_eq!((b.kind(), t), ("thread.started", None));
        h.send("go", 4).unwrap();
        assert_eq!(body(&mut rx).await.1, Some(4));
        assert_eq!(body(&mut rx).await.0.kind(), "attention.raised");
        h.respond("x1", Answer::allow()).unwrap();
        let (b, t) = body(&mut rx).await;
        assert_eq!((b.kind(), t), ("turn.ended", Some(4)));
        assert_eq!(
            next(&mut rx).await,
            AdapterEvent::Exited {
                code: Some(3),
                stderr_tail: "bye".into()
            }
        );
    }

    #[tokio::test]
    async fn scripted_mode_waits_for_stop_after_script() {
        let script = vec![Step::Emit(final_text("only".into()))];
        let (h, mut rx) = spawn_with(
            FakeAdapter::scripted(script),
            SpawnConfig::default(),
            Some("prompt"),
        );
        assert_eq!(body(&mut rx).await.1, Some(1));
        h.send("ignored", 2).unwrap();
        h.stop().unwrap();
        assert!(matches!(
            next(&mut rx).await,
            AdapterEvent::Exited { code: Some(0), .. }
        ));
    }

    /// All events until `Exited` (inclusive).
    async fn until_exit(rx: &mut Rx) -> Vec<AdapterEvent> {
        let mut out = Vec::new();
        loop {
            let ev = next(rx).await;
            let done = matches!(ev, AdapterEvent::Exited { .. });
            out.push(ev);
            if done {
                return out;
            }
        }
    }

    #[tokio::test]
    async fn stop_preempts_queued_prompt() {
        let (h, mut rx) = spawn(Some("!ask go?|yes\nfinishing"));
        started(&mut rx).await;
        let a = raised(&until_raised(&mut rx).await);
        // Turn 1 is running: queue turn 2, let turn 1 finish, then stop before turn 2 starts.
        h.send("second", 2).unwrap();
        h.respond(&a.attn_id, Answer::new("yes")).unwrap();
        h.stop().unwrap();
        let evs = until_exit(&mut rx).await;
        let turns: Vec<Option<u64>> = evs
            .iter()
            .filter_map(|e| match e {
                AdapterEvent::Emit { turn, .. } => Some(*turn),
                _ => None,
            })
            .collect();
        assert!(!turns.is_empty());
        assert!(
            turns.iter().all(|t| *t == Some(1)),
            "no turn 2 events: {turns:?}"
        );
        assert!(matches!(
            evs.last(),
            Some(AdapterEvent::Exited { code: Some(0), .. })
        ));
        assert!(timeout(WAIT, rx.recv()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stale_respond_is_ignored() {
        let (h, mut rx) = spawn(Some("!ask first?|a"));
        started(&mut rx).await;
        let a1 = raised(&until_raised(&mut rx).await);
        h.respond(&a1.attn_id, Answer::new("a")).unwrap();
        until_turn_end(&mut rx).await;
        // Idle: a repeated answer for the finished item does nothing.
        h.respond(&a1.attn_id, Answer::new("a")).unwrap();
        h.send("!ask second?|b", 2).unwrap();
        let a2 = raised(&until_raised(&mut rx).await);
        assert_ne!(a1.attn_id, a2.attn_id);
        // Waiting on a2: the stale a1 answer does not unblock the turn.
        h.respond(&a1.attn_id, Answer::new("a")).unwrap();
        assert!(
            timeout(Duration::from_millis(100), rx.recv())
                .await
                .is_err(),
            "stale respond must not produce events"
        );
        h.respond(&a2.attn_id, Answer::new("b")).unwrap();
        let evs = until_turn_end(&mut rx).await;
        assert_eq!(final_texts(&evs), ["answer: b"]);
    }

    #[tokio::test]
    async fn dropped_event_receiver_ends_session_and_send_fails() {
        let (h, mut rx) = spawn(Some("!ask go?|yes\nmore text"));
        started(&mut rx).await;
        let a = raised(&until_raised(&mut rx).await);
        // The hub side goes away mid-turn; the next emit fails and the session ends quietly.
        drop(rx);
        h.respond(&a.attn_id, Answer::new("yes")).unwrap();
        let gone = timeout(WAIT, async {
            loop {
                if h.send("anyone?", 2).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(
            gone.is_ok(),
            "session should end once its events are unwanted"
        );
        assert_eq!(h.send("again", 3), Err(twr_core::adapter::AdapterGone));
        assert_eq!(h.stop(), Err(twr_core::adapter::AdapterGone));
    }
}

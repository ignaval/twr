//! A seeded, randomized stress harness for the runner reconnect paths (hub side + real
//! runners): an in-process hub (its own tokio runtime, so it can crash: every task gone at
//! once) with its runner listener, two boxes (`gpu`, `cpu`) each served by a real runner
//! (`twr_runner::run_with`, its own runtime too, so it can crash with its runtimes) through a
//! fault-injecting relay chain, and a recording runtime ([`FuzzAdapter`]) that raises
//! permissions / questions (re-raises and replacements included), emits text, ends turns and
//! crashes, as the prompts script it.
//!
//! Relay chain per box (every connection of the box's runners goes through it):
//!
//! ```text
//! runner -> TCP cutter A -> WebSocket proxy -> TCP cutter B -> hub listener
//! ```
//!
//! Cutter A can cut hub->runner bytes at any byte (the runner's WebSocket reader sees a
//! partial frame), cutter B runner->hub bytes (the hub's reader sees one); A can also refuse
//! connections (box offline windows) and kill the current one. The WebSocket proxy cuts after
//! N messages in either direction (the Nth is lost in flight), withholds the runner's command
//! results for a while (delayed / withheld `cmd_result`s), swallows acks (and then cuts that
//! connection: a live WebSocket loses nothing, acks are only lost with it), and makes *stale*
//! connections: it withholds a few runner messages, closes the runner side (the runner
//! reconnects and fences the old connection) and delivers them to the hub on the fenced
//! connection afterwards.
//!
//! Actions (weighted, drawn from the seed's xorshift PRNG): `thread.new` on a box, prompts
//! (random scripts, each carrying a unique `#P<n>` id), answers to open items (with their
//! current revision; sometimes deliberately with a stale one, which must be refused), stop,
//! park / resume, attach begin / end (ssh-configured boxes), byte cuts both directions,
//! message cuts both directions, stale (fenced) connections, withheld results, swallowed
//! acks, offline windows, runner crash + restart from its spool (now or later), graceful
//! runner restarts, runner spool write failures, hub crash (its runtime dropped) + reopen, a
//! second runner of the same box for a while (fencing ping-pong; a separate data dir), SQLite
//! failure windows on the hub (a trigger refusing inserts of one event kind, or of watermarks,
//! outbox rows or queued prompts), naps. Waits are condition polls with deadlines, never
//! fixed sleeps standing in for "long enough".
//!
//! Every few steps, and at the end, a checkpoint quiesces the system (faults off, runners
//! up, both boxes online, every open runtime item answered, parked threads resumed, attaches
//! ended, queues a crash left behind nudged with a new prompt) and checks the invariants
//! (see `check_all`):
//!
//! 1. per-thread event seqs contiguous from 1, event ids unique, control gseq contiguous;
//! 2. replay == live: the reducers folded from the log equal the live state, and a hub
//!    reopened from the database (crash + open, runners kept away) has the same threads,
//!    views, queue sizes and attention stack;
//! 3. at most one live runtime per thread at any time (checked at every spawn, on the
//!    runtime side), and once quiesced the runtimes alive on the boxes are exactly the ones
//!    the log shows live (same session);
//! 4. every answer a runtime received is one the harness gave, for exactly that item
//!    incarnation (same runtime instance, same id, same content), none twice, none that the
//!    hub accepted for a stale revision;
//! 5. every prompt sent is in exactly one `turn.started` (retries aside) unless a stop
//!    dropped it (per thread: never-started prompts == prompts the stops reported dropped),
//!    none reached a runtime twice, a refused send never runs, every started turn ended
//!    exactly once, and a turn whose runtime-sent `turn.ended` is recorded has every event its
//!    runtime emitted in it, once, in order;
//! 6. runner spools and the hub's `remote_outbox` are empty, and each watermark equals the
//!    runner's last rseq of the thread;
//! 7. no thread stuck: the quiesce completes (no open turn, no queue, no item left) within
//!    its deadline;
//! 8. a stopped thread stays stopped (no later turn reaches a runtime, nothing starts after
//!    its `thread.stopped`) until a send / park / resume / attach.
//!
//! A second runner of a box (the `Impostor` action: its own data dir, so its spool and its
//! command dedup are its own) claims the box's runtimes are gone and may execute the same
//! recorded commands as the real one; nothing a hub does can undo that. The threads of a box
//! while it had one are *tainted*: the global invariants (1, 2) still cover them, the
//! per-thread ones skip them, and the seed summary counts them.
//!
//! Failures carry the seed, the invariant, the details and the tail of the step log; the
//! data directory of a failed seed (hub database, runner spools) is kept and named.
//!
//! Reproducing: the action script (which action, on which box / thread index, with which
//! parameters) is a function of the seed alone, but thread scheduling is not, so a seed
//! replays the same decisions over possibly different interleavings (run a failing seed a few
//! times). The data lives on tmpfs (`/dev/shm`) when there is one (`TWR_FUZZ_TMP` overrides):
//! every hub commit and spool append is synced. Environment:
//!
//! - `TWR_FUZZ_SEED=7` or `TWR_FUZZ_SEED=7,19`: only these seeds;
//! - `TWR_FUZZ_SEEDS=n`: number of seeds (default 20; the long run 500), from
//!   `TWR_FUZZ_BASE` (default 1; the long run 1000);
//! - `TWR_FUZZ_ITERS=n`: steps per seed (default 40; the long run 60);
//! - `TWR_FUZZ_JOBS=n`: seeds run in parallel (default 2);
//! - `TWR_FUZZ_MINUTES=n`: the long run starts no new seed after this (default 30);
//! - `TWR_FUZZ_LOG=warn|info|debug`: tracing to stderr;
//! - `TWR_FUZZ_TRACE=1`: the step log as it happens.
//!
//! The long run: `cargo test -p twr-hub --test runner_fuzz -- --ignored --nocapture`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;
use twr_adapters::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::{BoxConfig, Config};
use twr_core::event::*;
use twr_core::reduce::{AttentionStack, Registry};
use twr_core::runner::{HubMsg, RunnerMsg, Secret};
use twr_hub::listener::{RunnerListener, RunnerServer};
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

const BOXES: [&str; 2] = ["gpu", "cpu"];
const RUNTIME: &str = "fake";
const CLIENT: &str = "fuzz";
/// How long a checkpoint may take to quiesce before the system counts as stuck.
const SETTLE: Duration = Duration::from_secs(60);
const CHECK_EVERY: u64 = 12;
const MAX_THREADS: usize = 6;

fn token(box_name: &str) -> String {
    format!("{box_name}-token-5e1f0c")
}

// ------------------------------------------------------------------------------------------
// PRNG
// ------------------------------------------------------------------------------------------

/// xorshift64* (deterministic per seed, no dependency).
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03);
        if r.0 == 0 {
            r.0 = 1;
        }
        for _ in 0..4 {
            r.next();
        }
        r
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (0 when `n` is 0). Always consumes one draw, so that the stream
    /// does not depend on what `n` turned out to be.
    fn below(&mut self, n: u64) -> u64 {
        let x = self.next();
        if n == 0 {
            0
        } else {
            x % n
        }
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.below(hi - lo + 1)
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    fn weighted<T: Copy>(&mut self, items: &[(T, u64)]) -> T {
        let total: u64 = items.iter().map(|(_, w)| w).sum();
        let mut x = self.below(total);
        for (t, w) in items {
            if x < *w {
                return *t;
            }
            x -= w;
        }
        items[items.len() - 1].0
    }
}

// ------------------------------------------------------------------------------------------
// The recording runtime
// ------------------------------------------------------------------------------------------

/// One runtime instance (a spawn or resume on some runner).
#[derive(Debug, Clone)]
#[allow(dead_code)] // read through Debug in failure reports
struct Inst {
    thread: String,
    runner: u64,
    impostor: bool,
    session: String,
    resumed: bool,
    live: bool,
    spawned_step: u64,
    exited_step: Option<u64>,
}

/// An answer a runtime received (`AdapterCmd::Respond`).
#[derive(Debug, Clone)]
#[allow(dead_code)] // read through Debug in failure reports
struct Delivery {
    inst: u64,
    thread: String,
    adapter_id: String,
    decision: String,
    message: Option<String>,
    /// What the runtime's open item with that id asked when the answer arrived.
    content: Option<String>,
    /// The runtime was waiting for exactly this id (it acted on the answer).
    accepted: bool,
    step: u64,
}

/// A prompt a runtime received (initial or `send`).
#[derive(Debug, Clone)]
struct PromptRx {
    inst: u64,
    thread: String,
    turn: u64,
    pid: Option<u64>,
    step: u64,
}

#[derive(Debug, Default)]
struct Rec {
    step: u64,
    next_inst: u64,
    insts: BTreeMap<u64, Inst>,
    deliveries: Vec<Delivery>,
    prompts: Vec<PromptRx>,
    /// Invariant violations seen as they happened.
    violations: Vec<String>,
    /// Overlaps involving a second runner of a box (an operator error the hub cannot
    /// prevent): reported, not failed.
    dual: Vec<String>,
    /// What the runtimes emitted per (thread, turn), as [`frame_sig`] signatures.
    emitted: HashMap<(String, u64), Vec<String>>,
}

/// A comparable signature of an in-turn runtime event (`None`: not compared, e.g. a
/// withdrawal the hub may drop when the item was answered meanwhile).
fn frame_sig(body: &EventBody) -> Option<String> {
    Some(match body {
        EventBody::AgentText(t) => format!("text:{}:{}", t.delta, t.text),
        EventBody::AgentToolCall(c) => format!("call:{}", c.call_id),
        EventBody::AgentToolResult(r) => format!("result:{}:{}", r.call_id, r.ok),
        EventBody::AttentionRaised(a) => format!("raised:{}", a.summary),
        EventBody::TurnEnded(e) => format!("ended:{:?}", e.result),
        EventBody::AttentionResolved(_) => return None,
        other => other.kind().to_string(),
    })
}

#[derive(Debug, Clone, Default)]
struct Recorder(Arc<Mutex<Rec>>);

impl Recorder {
    fn lock(&self) -> MutexGuard<'_, Rec> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Marks its instance exited however the session ends (an exit, a stop, or its task being
/// dropped with its runner's runtime: a crash).
struct ExitGuard {
    rec: Recorder,
    inst: u64,
}

impl Drop for ExitGuard {
    fn drop(&mut self) {
        let mut r = self.rec.lock();
        let step = r.step;
        if let Some(i) = r.insts.get_mut(&self.inst) {
            i.live = false;
            i.exited_step = Some(step);
        }
    }
}

/// The runtime of every box, as the runners run it. Prompt script (one directive per line,
/// after the `#P<n>` header line):
///
/// | line          | effect |
/// |---------------|--------|
/// | `say N`       | N `agent.text` deltas and a final text |
/// | `tool`        | a non-risky tool call and its result |
/// | `perm`        | a risky tool call + permission item; waits for its answer |
/// | `ask`         | a question (options x, y); waits |
/// | `reraise MS`  | a question; unanswered after MS ms it is raised again, same id and content (a new revision at the hub); waits |
/// | `replace MS`  | a question; unanswered after MS ms it is withdrawn (`superseded`) and raised again as `<id>~1` with other content; waits for that one |
/// | `nap MS`      | pause (commands keep coming) |
/// | `crash`       | exit 1 without ending the turn |
///
/// Attention ids are `a<n>` numbered per instance (a resumed session's new process numbers
/// them anew, like real runtimes), so ids repeat across instances of a thread; every item's
/// content (its summary) names the instance: `i<inst>/<id>`.
#[derive(Clone)]
struct FuzzAdapter {
    rec: Recorder,
    runner: u64,
    impostor: bool,
}

impl Adapter for FuzzAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: RUNTIME,
            tier: Tier::Headless,
            description: "fuzz harness runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (handle, end) = AdapterHandle::pair();
        let resumed = cfg.resume_session.is_some();
        let session = cfg
            .resume_session
            .clone()
            .unwrap_or_else(|| ulid::Ulid::generate().to_string());
        let inst = {
            let mut r = self.rec.lock();
            r.next_inst += 1;
            let inst = r.next_inst;
            let step = r.step;
            let others: Vec<(u64, Inst)> = r
                .insts
                .iter()
                .filter(|(_, i)| i.live && i.thread == cfg.thread)
                .map(|(k, i)| (*k, i.clone()))
                .collect();
            if !others.is_empty() {
                let dual = self.impostor || others.iter().any(|(_, i)| i.impostor);
                let msg = format!(
                    "[step {step}] inv3: runtime instance {inst} (runner {}, session {session}) \
                     started for thread {} while instance(s) {:?} still live",
                    self.runner,
                    cfg.thread,
                    others
                        .iter()
                        .map(|(k, i)| format!("{k}@runner{} session {}", i.runner, i.session))
                        .collect::<Vec<_>>()
                );
                if dual {
                    r.dual.push(msg);
                } else {
                    r.violations.push(msg);
                }
            }
            r.insts.insert(
                inst,
                Inst {
                    thread: cfg.thread.clone(),
                    runner: self.runner,
                    impostor: self.impostor,
                    session: session.clone(),
                    resumed,
                    live: true,
                    spawned_step: step,
                    exited_step: None,
                },
            );
            inst
        };
        let mut s = Sess {
            end,
            rec: self.rec.clone(),
            inst,
            thread: cfg.thread.clone(),
            session,
            queue: VecDeque::new(),
            next_attn: 0,
            open: HashMap::new(),
            _guard: ExitGuard {
                rec: self.rec.clone(),
                inst,
            },
        };
        if let Some(p) = prompt {
            s.queue.push_back((p, cfg.prompt_turn.unwrap_or(1)));
        }
        tokio::spawn(s.run());
        Ok(handle)
    }

    fn attach_command(
        &self,
        cfg: &SpawnConfig,
        _session_id: &str,
    ) -> Option<std::process::Command> {
        let mut c = std::process::Command::new("true");
        if !cfg.cwd.as_os_str().is_empty() {
            c.current_dir(&cfg.cwd);
        }
        Some(c)
    }
}

enum Halt {
    Stop,
    Exited,
    Closed,
}

type Flow<T = ()> = Result<T, Halt>;

struct Sess {
    end: AdapterEnd,
    rec: Recorder,
    inst: u64,
    thread: String,
    session: String,
    queue: VecDeque<(String, u64)>,
    next_attn: u64,
    /// Open items: id -> content.
    open: HashMap<String, String>,
    _guard: ExitGuard,
}

fn parse_pid(prompt: &str) -> Option<u64> {
    prompt
        .lines()
        .next()?
        .trim()
        .strip_prefix("#P")?
        .parse()
        .ok()
}

impl Sess {
    fn emit(&self, body: EventBody, turn: Option<u64>) -> Flow {
        if let (Some(t), Some(sig)) = (turn, frame_sig(&body)) {
            self.rec
                .lock()
                .emitted
                .entry((self.thread.clone(), t))
                .or_default()
                .push(sig);
        }
        if self.end.emit(body, turn) {
            Ok(())
        } else {
            Err(Halt::Closed)
        }
    }

    fn on_cmd(&mut self, cmd: Option<AdapterCmd>, waiting: Option<&str>) -> Flow<Option<Answer>> {
        match cmd {
            Some(AdapterCmd::Compact { .. }) => Ok(None),
            Some(AdapterCmd::Send { text, turn }) => {
                self.queue.push_back((text, turn));
                Ok(None)
            }
            Some(AdapterCmd::Respond { attn_id, answer }) => {
                let content = self.open.get(&attn_id).cloned();
                let accepted = content.is_some() && waiting == Some(attn_id.as_str());
                {
                    let mut r = self.rec.lock();
                    let step = r.step;
                    r.deliveries.push(Delivery {
                        inst: self.inst,
                        thread: self.thread.clone(),
                        adapter_id: attn_id.clone(),
                        decision: answer.decision.clone(),
                        message: answer.message.clone(),
                        content,
                        accepted,
                        step,
                    });
                }
                if accepted {
                    self.open.remove(&attn_id);
                    Ok(Some(answer))
                } else {
                    Ok(None)
                }
            }
            Some(AdapterCmd::Stop | AdapterCmd::Interrupt) | None => Err(Halt::Stop),
        }
    }

    async fn next_prompt(&mut self) -> Flow<(String, u64)> {
        loop {
            loop {
                match self.end.cmds.try_recv() {
                    Ok(cmd) => {
                        self.on_cmd(Some(cmd), None)?;
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        return Err(Halt::Stop)
                    }
                }
            }
            if let Some(p) = self.queue.pop_front() {
                return Ok(p);
            }
            let cmd = self.end.cmds.recv().await;
            self.on_cmd(cmd, None)?;
        }
    }

    /// Waits for the answer to `id` (at most `max`).
    async fn wait_answer(&mut self, id: &str, max: Option<Duration>) -> Flow<Option<Answer>> {
        let deadline = max.map(|d| Instant::now() + d);
        loop {
            let cmd = match deadline {
                Some(d) => tokio::select! {
                    _ = tokio::time::sleep_until(d) => return Ok(None),
                    c = self.end.cmds.recv() => c,
                },
                None => self.end.cmds.recv().await,
            };
            if let Some(a) = self.on_cmd(cmd, Some(id))? {
                return Ok(Some(a));
            }
        }
    }

    async fn nap(&mut self, ms: u64) -> Flow {
        let deadline = Instant::now() + Duration::from_millis(ms);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => return Ok(()),
                c = self.end.cmds.recv() => { self.on_cmd(c, None)?; }
            }
        }
    }

    fn new_id(&mut self) -> String {
        self.next_attn += 1;
        format!("a{}", self.next_attn)
    }

    fn raise(
        &mut self,
        id: &str,
        reason: AttentionReason,
        call_id: Option<String>,
        turn: Option<u64>,
    ) -> Flow {
        let content = format!("i{}/{id}", self.inst);
        let options = match reason {
            AttentionReason::Permission => vec!["allow".to_string(), "deny".to_string()],
            _ => vec!["x".to_string(), "y".to_string()],
        };
        self.open.insert(id.to_string(), content.clone());
        self.emit(
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: id.to_string(),
                reason,
                summary: content,
                options,
                call_id,
                ..Default::default()
            }),
            turn,
        )
    }

    async fn run(mut self) {
        let mut ext = Map::new();
        ext.insert("fuzz".into(), json!({"inst": self.inst}));
        let started = EventBody::ThreadStarted(SessionPayload {
            session_id: Some(self.session.clone()),
            ext: Some(ext),
            ..Default::default()
        });
        if let Err(h) = self.emit(started, None) {
            return self.finish(h);
        }
        loop {
            let flow = match self.next_prompt().await {
                Ok((p, turn)) => self.turn(&p, turn).await,
                Err(h) => Err(h),
            };
            if let Err(h) = flow {
                return self.finish(h);
            }
        }
    }

    fn finish(&self, h: Halt) {
        if let Halt::Stop = h {
            self.end.exited(Some(0), "");
        }
    }

    async fn turn(&mut self, prompt: &str, turn: u64) -> Flow {
        let t = Some(turn);
        {
            let mut r = self.rec.lock();
            let step = r.step;
            r.prompts.push(PromptRx {
                inst: self.inst,
                thread: self.thread.clone(),
                turn,
                pid: parse_pid(prompt),
                step,
            });
        }
        for line in prompt.lines().skip(1) {
            let mut words = line.split_whitespace();
            let d = words.next().unwrap_or("");
            let arg: u64 = words.next().and_then(|w| w.parse().ok()).unwrap_or(0);
            match d {
                "say" => {
                    let mut all = String::new();
                    for i in 0..arg.max(1) {
                        let w = format!("w{i} ");
                        all.push_str(&w);
                        self.emit(
                            EventBody::AgentText(AgentText {
                                text: w,
                                delta: true,
                                ..Default::default()
                            }),
                            t,
                        )?;
                    }
                    self.emit(
                        EventBody::AgentText(AgentText {
                            text: all.trim().to_string(),
                            is_final: true,
                            ..Default::default()
                        }),
                        t,
                    )?;
                }
                "tool" => {
                    let call = format!("c{}-{}", turn, self.next_attn);
                    self.emit(tool_call(&call, false), t)?;
                    self.emit(tool_result(&call, true), t)?;
                }
                "perm" => {
                    let id = self.new_id();
                    let call = format!("call-{id}");
                    self.emit(tool_call(&call, true), t)?;
                    self.raise(&id, AttentionReason::Permission, Some(call.clone()), t)?;
                    let a = self.wait_answer(&id, None).await?.unwrap_or_default();
                    self.emit(tool_result(&call, a.is_allow()), t)?;
                }
                "ask" => {
                    let id = self.new_id();
                    self.raise(&id, AttentionReason::Question, None, t)?;
                    let a = self.wait_answer(&id, None).await?.unwrap_or_default();
                    self.emit(final_text(format!("answer: {}", a.decision)), t)?;
                }
                "reraise" => {
                    let id = self.new_id();
                    self.raise(&id, AttentionReason::Question, None, t)?;
                    let a = match self
                        .wait_answer(&id, Some(Duration::from_millis(arg)))
                        .await?
                    {
                        Some(a) => a,
                        None => {
                            // The same item again: same id, same content.
                            self.raise(&id, AttentionReason::Question, None, t)?;
                            self.wait_answer(&id, None).await?.unwrap_or_default()
                        }
                    };
                    self.emit(final_text(format!("answer: {}", a.decision)), t)?;
                }
                "replace" => {
                    let id = self.new_id();
                    self.raise(&id, AttentionReason::Question, None, t)?;
                    let a = match self
                        .wait_answer(&id, Some(Duration::from_millis(arg)))
                        .await?
                    {
                        Some(a) => a,
                        None => {
                            // Withdrawn (superseded) and raised anew under another id.
                            self.open.remove(&id);
                            let mut ext = Map::new();
                            ext.insert(RUNTIME.into(), json!({"superseded": true}));
                            self.emit(
                                EventBody::AttentionResolved(AttentionResolved {
                                    attn_id: id.clone(),
                                    by: ResolvedBy::Timeout,
                                    answer: None,
                                    ext: Some(ext),
                                    ..Default::default()
                                }),
                                t,
                            )?;
                            let id2 = format!("{id}~1");
                            self.raise(&id2, AttentionReason::Question, None, t)?;
                            self.wait_answer(&id2, None).await?.unwrap_or_default()
                        }
                    };
                    self.emit(final_text(format!("answer: {}", a.decision)), t)?;
                }
                "nap" => self.nap(arg).await?,
                "crash" => {
                    self.end.exited(Some(1), "fuzz: crash requested");
                    return Err(Halt::Exited);
                }
                _ => {}
            }
        }
        self.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                session_id: Some(self.session.clone()),
                ..Default::default()
            }),
            t,
        )
    }
}

fn tool_call(call: &str, risky: bool) -> EventBody {
    EventBody::AgentToolCall(AgentToolCall {
        call_id: call.to_string(),
        tool: if risky { "Bash" } else { "Read" }.into(),
        input_summary: "fuzz".into(),
        risky,
        ..Default::default()
    })
}

fn tool_result(call: &str, ok: bool) -> EventBody {
    EventBody::AgentToolResult(AgentToolResult {
        call_id: call.to_string(),
        ok,
        output_summary: if ok { "ok" } else { "denied" }.into(),
        ..Default::default()
    })
}

fn final_text(text: String) -> EventBody {
    EventBody::AgentText(AgentText {
        text,
        is_final: true,
        ..Default::default()
    })
}

// ------------------------------------------------------------------------------------------
// Fault injection: a TCP cutter and a WebSocket proxy
// ------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CutState {
    /// `host:port` connections are relayed to.
    target: String,
    /// Cut after this many more bytes in this direction (`true`: client -> target); the
    /// bytes up to there are forwarded, the rest is lost.
    arm: Option<(bool, u64)>,
    /// Refuse new connections until then.
    blocked_until: Option<Instant>,
    /// Bumped to kill the current connections.
    gen: u64,
    conns: u64,
    cuts: u64,
}

/// A TCP relay that can cut a connection in the middle of a frame.
#[derive(Clone)]
struct Cutter {
    addr: String,
    state: Arc<Mutex<CutState>>,
    task: Arc<tokio::task::JoinHandle<()>>,
}

impl Cutter {
    async fn start(target: &str) -> Cutter {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(CutState {
            target: target.into(),
            ..Default::default()
        }));
        let st = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let st = Arc::clone(&st);
                tokio::spawn(async move { Cutter::relay(st, tcp).await });
            }
        });
        Cutter {
            addr,
            state,
            task: Arc::new(task),
        }
    }

    fn st(&self) -> MutexGuard<'_, CutState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn kill_current(&self) {
        self.st().gen += 1;
    }

    /// How many of `n` bytes in direction `up` go through, and whether to cut then.
    fn take(st: &Mutex<CutState>, up: bool, n: usize) -> (usize, bool) {
        let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
        match s.arm {
            Some((d, rem)) if d == up => {
                if rem as usize <= n {
                    s.arm = None;
                    s.cuts += 1;
                    (rem as usize, true)
                } else {
                    s.arm = Some((d, rem - n as u64));
                    (n, false)
                }
            }
            _ => (n, false),
        }
    }

    async fn relay(st: Arc<Mutex<CutState>>, down: TcpStream) {
        let (target, blocked, my_gen) = {
            let s = st.lock().unwrap_or_else(|e| e.into_inner());
            (
                s.target.clone(),
                s.blocked_until.is_some_and(|u| Instant::now() < u),
                s.gen,
            )
        };
        if blocked {
            return;
        }
        let Ok(Ok(up)) =
            tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&target)).await
        else {
            return;
        };
        st.lock().unwrap_or_else(|e| e.into_inner()).conns += 1;
        let _ = down.set_nodelay(true);
        let _ = up.set_nodelay(true);
        let (mut dr, mut dw) = down.into_split();
        let (mut ur, mut uw) = up.into_split();
        let mut b1 = vec![0u8; 4096];
        let mut b2 = vec![0u8; 4096];
        let mut tick = tokio::time::interval(Duration::from_millis(20));
        loop {
            tokio::select! {
                r = dr.read(&mut b1) => {
                    let n = match r { Ok(0) | Err(_) => return, Ok(n) => n };
                    let (k, cut) = Cutter::take(&st, true, n);
                    if k > 0 && uw.write_all(&b1[..k]).await.is_err() { return; }
                    if cut { let _ = uw.flush().await; return; }
                }
                r = ur.read(&mut b2) => {
                    let n = match r { Ok(0) | Err(_) => return, Ok(n) => n };
                    let (k, cut) = Cutter::take(&st, false, n);
                    if k > 0 && dw.write_all(&b2[..k]).await.is_err() { return; }
                    if cut { let _ = dw.flush().await; return; }
                }
                _ = tick.tick() => {
                    if st.lock().unwrap_or_else(|e| e.into_inner()).gen != my_gen { return; }
                }
            }
        }
    }
}

impl Drop for Cutter {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}

#[derive(Debug, Default)]
struct WsState {
    upstream: String,
    /// Cut after this many more messages in this direction (`true`: runner -> hub); the
    /// last one is lost.
    cut_msgs: Option<(bool, u64)>,
    /// Withhold the runner's command results until then (released afterwards, on the same
    /// connection if it still lives).
    hold_until: Option<Instant>,
    /// Drop the hub's acks until then; a connection that dropped some is cut then (a live
    /// WebSocket loses nothing: acks are only ever lost with their connection).
    swallow_until: Option<Instant>,
    /// The next connection withholds up to this many runner messages, closes the runner
    /// side, and delivers them to the hub once the runner has reconnected (fenced frames).
    stale: Option<usize>,
    gen: u64,
    conns: u64,
    cuts: u64,
    stale_done: u64,
}

#[derive(Clone)]
struct WsProxy {
    addr: String,
    state: Arc<Mutex<WsState>>,
    task: Arc<tokio::task::JoinHandle<()>>,
}

enum UpKind {
    Frame,
    Result,
    Other,
}

fn up_kind(m: &Message) -> UpKind {
    if let Message::Text(t) = m {
        match serde_json::from_str::<RunnerMsg>(t.as_str()) {
            Ok(RunnerMsg::Event { .. } | RunnerMsg::Exited { .. }) => return UpKind::Frame,
            Ok(RunnerMsg::CmdResult(_)) => return UpKind::Result,
            _ => {}
        }
    }
    UpKind::Other
}

fn is_ack(m: &Message) -> bool {
    matches!(m, Message::Text(t) if matches!(serde_json::from_str::<HubMsg>(t.as_str()), Ok(HubMsg::Ack { .. })))
}

impl WsProxy {
    async fn start(upstream: &str) -> WsProxy {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let state = Arc::new(Mutex::new(WsState {
            upstream: upstream.into(),
            ..Default::default()
        }));
        let st = Arc::clone(&state);
        let task = tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let st = Arc::clone(&st);
                tokio::spawn(async move { WsProxy::relay(st, tcp).await });
            }
        });
        WsProxy {
            addr,
            state,
            task: Arc::new(task),
        }
    }

    fn st(&self) -> MutexGuard<'_, WsState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn count(st: &Mutex<WsState>, up: bool) -> bool {
        let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((d, n)) = s.cut_msgs.as_mut() {
            if *d == up {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    s.cut_msgs = None;
                    s.cuts += 1;
                    return true;
                }
            }
        }
        false
    }

    async fn relay(st: Arc<Mutex<WsState>>, tcp: TcpStream) {
        let lock = |st: &Mutex<WsState>| -> (String, u64) {
            let s = st.lock().unwrap_or_else(|e| e.into_inner());
            (s.upstream.clone(), s.gen)
        };
        let Ok(Ok(down)) =
            tokio::time::timeout(Duration::from_secs(3), tokio_tungstenite::accept_async(tcp))
                .await
        else {
            return;
        };
        let (upstream, my_gen) = lock(&st);
        let Ok(Ok((up, _))) = tokio::time::timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(upstream.as_str()),
        )
        .await
        else {
            return;
        };
        let my_conn = {
            let mut s = st.lock().unwrap_or_else(|e| e.into_inner());
            s.conns += 1;
            s.conns
        };
        let (mut dsink, mut dstream) = down.split();
        let (mut usink, mut ustream) = up.split();
        let mut held: Vec<Message> = Vec::new();
        let mut swallowed = false;
        // (limit, armed at, withheld)
        let mut stale: Option<(usize, Instant, Vec<Message>)> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(15));
        let withheld = loop {
            if stale.is_none() {
                if let Some(k) = st.lock().unwrap_or_else(|e| e.into_inner()).stale.take() {
                    stale = Some((k.max(1), Instant::now(), Vec::new()));
                }
            }
            tokio::select! {
                m = dstream.next() => {
                    let Some(Ok(m)) = m else { return };
                    if let Some((k, _, buf)) = stale.as_mut() {
                        buf.push(m);
                        if buf.len() >= *k {
                            break stale.take().unwrap().2;
                        }
                        continue;
                    }
                    if WsProxy::count(&st, true) { return; }
                    if matches!(up_kind(&m), UpKind::Result)
                        && st.lock().unwrap_or_else(|e| e.into_inner()).hold_until.is_some_and(|u| Instant::now() < u)
                    {
                        held.push(m);
                        continue;
                    }
                    if usink.send(m).await.is_err() { return; }
                }
                m = ustream.next() => {
                    let Some(Ok(m)) = m else { return };
                    if WsProxy::count(&st, false) { return; }
                    if is_ack(&m)
                        && st.lock().unwrap_or_else(|e| e.into_inner()).swallow_until.is_some_and(|u| Instant::now() < u)
                    {
                        swallowed = true;
                        continue;
                    }
                    if dsink.send(m).await.is_err() { return; }
                }
                _ = tick.tick() => {
                    let (gen, hold, swallowing) = {
                        let s = st.lock().unwrap_or_else(|e| e.into_inner());
                        let now = Instant::now();
                        (
                            s.gen,
                            s.hold_until.is_some_and(|u| now < u),
                            s.swallow_until.is_some_and(|u| now < u),
                        )
                    };
                    if gen != my_gen || (swallowed && !swallowing) { return; }
                    if !hold && !held.is_empty() {
                        for m in held.drain(..) {
                            if usink.send(m).await.is_err() { return; }
                        }
                    }
                    if let Some((_, at, buf)) = &stale {
                        if at.elapsed() > Duration::from_millis(400) && !buf.is_empty() {
                            break stale.take().unwrap().2;
                        }
                        if at.elapsed() > Duration::from_secs(3) {
                            break Vec::new();
                        }
                    }
                }
            }
        };
        // The runner side goes; the hub side stays silent until the runner is back.
        let _ = dsink.close().await;
        drop(dsink);
        drop(dstream);
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline
            && st.lock().unwrap_or_else(|e| e.into_inner()).conns <= my_conn
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        for m in withheld {
            if usink.send(m).await.is_err() {
                break;
            }
        }
        let _ = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(Ok(_)) = ustream.next().await {}
        })
        .await;
        st.lock().unwrap_or_else(|e| e.into_inner()).stale_done += 1;
    }
}

impl Drop for WsProxy {
    fn drop(&mut self) {
        if Arc::strong_count(&self.task) == 1 {
            self.task.abort();
        }
    }
}

// ------------------------------------------------------------------------------------------
// Hub and runners in their own runtimes
// ------------------------------------------------------------------------------------------

/// Shuts a runtime down from outside any async context (waiting for its blocking tasks, so
/// that nothing it started writes after its successor opened the same files).
async fn kill_runtime(rt: tokio::runtime::Runtime) {
    let (tx, rx) = oneshot::channel();
    std::thread::spawn(move || {
        rt.shutdown_timeout(Duration::from_secs(10));
        let _ = tx.send(());
    });
    let _ = rx.await;
}

fn hub_config() -> Config {
    let mut c = Config {
        default_runtime: RUNTIME.into(),
        ..Default::default()
    };
    c.hub.listen = Some("127.0.0.1:0".into());
    for b in BOXES {
        c.boxes.insert(
            b.into(),
            BoxConfig {
                token: Some(token(b)),
                ssh: Some(format!("me@{b}.lan")),
                ..Default::default()
            },
        );
    }
    c
}

fn with_fake() -> twr_hub::host::AdapterLookup {
    Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            RUNTIME => Some(Arc::new(FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    })
}

struct TestHub {
    rt: Option<tokio::runtime::Runtime>,
    hub: Option<Hub>,
    server: Option<RunnerServer>,
    url: String,
}

impl TestHub {
    async fn start(dir: &Path) -> anyhow::Result<TestHub> {
        let mut opts = HubOptions::new(dir.join("hub").join("twr.db"), hub_config());
        opts.adapters = with_fake();
        opts.stop_timeout = Duration::from_secs(5);
        opts.git_tick = Some(Duration::ZERO);
        opts.stall_after = Some(Duration::ZERO);
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let r = rt
            .spawn(async move {
                let l = RunnerListener::bind(&opts.config.hub)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("no listener"))?;
                let url = l.url();
                let hub = Hub::open(opts).await?;
                let server = l.serve(hub.clone());
                anyhow::Ok((hub, server, url))
            })
            .await;
        let (hub, server, url) = match r {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                kill_runtime(rt).await;
                return Err(e);
            }
            Err(e) => {
                kill_runtime(rt).await;
                return Err(e.into());
            }
        };
        Ok(TestHub {
            rt: Some(rt),
            hub: Some(hub),
            server: Some(server),
            url,
        })
    }

    fn hub(&self) -> &Hub {
        self.hub.as_ref().unwrap()
    }

    fn addr(&self) -> String {
        self.url.trim_start_matches("ws://").to_string()
    }

    /// Runs `f` inside the hub's runtime.
    fn sync<T>(&self, f: impl FnOnce(&Hub) -> T) -> T {
        let _g = self.rt.as_ref().unwrap().enter();
        f(self.hub())
    }

    /// Runs the future `f` makes in the hub's runtime; `None` when it did not finish in
    /// `max` (it keeps running) or the hub died under it.
    async fn run<T, F>(&self, max: Duration, f: impl FnOnce(Hub) -> F) -> Option<T>
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let fut = f(self.hub().clone());
        let h = self.rt.as_ref().unwrap().spawn(fut);
        match tokio::time::timeout(max, h).await {
            Ok(Ok(v)) => Some(v),
            _ => None,
        }
    }

    /// Every task of the hub gone at once; nothing graceful.
    async fn crash(mut self) {
        self.server.take();
        self.hub.take();
        if let Some(rt) = self.rt.take() {
            kill_runtime(rt).await;
        }
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.server.take();
        self.hub.take();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

/// A runner process: its own runtime (its runtimes live there too).
struct RunnerProc {
    id: u64,
    rt: Option<tokio::runtime::Runtime>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
    lbox: Option<LocalBox>,
}

impl RunnerProc {
    fn start(
        box_name: &str,
        url: &str,
        dir: &Path,
        rec: &Recorder,
        id: u64,
        impostor: bool,
    ) -> anyhow::Result<RunnerProc> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let adapter = FuzzAdapter {
            rec: rec.clone(),
            runner: id,
            impostor,
        };
        let opened = {
            let _g = rt.enter();
            let mut o = LocalBoxOptions::new(dir, box_name);
            o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
                (name == RUNTIME).then(|| Arc::new(adapter.clone()) as Arc<dyn Adapter>)
            });
            o.runtimes = vec![RUNTIME.into()];
            LocalBox::open(o)
        };
        let lbox = match opened {
            Ok(lbox) => lbox,
            Err(e) => {
                // Called from async code too: a runtime may not be dropped there.
                std::thread::spawn(move || drop(rt));
                return Err(e);
            }
        };
        let mut o = RunnerOptions::new(url, Secret::new(token(box_name)), box_name, dir);
        o.backoff_min = Duration::from_millis(20);
        o.backoff_max = Duration::from_millis(200);
        o.refused_retry = Duration::from_millis(200);
        o.ping_interval = Duration::from_millis(500);
        o.idle_timeout = Duration::from_secs(10);
        o.welcome_timeout = Duration::from_secs(5);
        o.shutdown_grace = Duration::from_secs(3);
        let (tx, rx) = oneshot::channel::<()>();
        let task = rt.spawn(twr_runner::run_with(o, lbox.clone(), async move {
            let _ = rx.await;
        }));
        Ok(RunnerProc {
            id,
            rt: Some(rt),
            stop: Some(tx),
            task: Some(task),
            lbox: Some(lbox),
        })
    }

    fn lbox(&self) -> &LocalBox {
        self.lbox.as_ref().unwrap()
    }

    fn finished(&self) -> bool {
        self.task.as_ref().is_none_or(|t| t.is_finished())
    }

    /// The process dies: no graceful stop, its runtimes with it.
    async fn crash(mut self) {
        self.lbox.take();
        self.task.take();
        if let Some(rt) = self.rt.take() {
            kill_runtime(rt).await;
        }
    }

    async fn graceful(mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(t) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(15), t).await;
        }
        self.lbox.take();
        if let Some(rt) = self.rt.take() {
            kill_runtime(rt).await;
        }
    }
}

impl Drop for RunnerProc {
    fn drop(&mut self) {
        self.lbox.take();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

// ------------------------------------------------------------------------------------------
// The harness
// ------------------------------------------------------------------------------------------

#[derive(Debug)]
struct Failure {
    seed: u64,
    invariant: String,
    detail: String,
    log_tail: Vec<String>,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "seed {} FAILED {}: {}",
            self.seed, self.invariant, self.detail
        )?;
        writeln!(f, "  step log (tail):")?;
        for l in &self.log_tail {
            writeln!(f, "    {l}")?;
        }
        Ok(())
    }
}

struct BoxH {
    name: &'static str,
    a: Cutter,
    ws: WsProxy,
    b: Cutter,
    rdir: PathBuf,
    proj: PathBuf,
    runner: Option<RunnerProc>,
    /// A runner starting in the background (after a delayed restart).
    pending: Option<tokio::task::JoinHandle<Option<RunnerProc>>>,
    /// A second runner of the box, crashed in the background when its window ends.
    impostor: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // read through Debug in failure reports
struct ThreadM {
    id: String,
    name: String,
    box_idx: usize,
    /// Step of the last successful stop not followed by a send / park / resume.
    stopped_at: Option<u64>,
    /// Turns the thread had started when that stop returned.
    stop_turns: u64,
    lease: Option<u64>,
    /// Prompts the stops reported dropped.
    dropped: u64,
    /// Prompts a failed stop may have dropped: a `thread.stop` commits its queue drop first
    /// and can fail after it (the error then reports no count).
    maybe_dropped: u64,
    nudges: u64,
}

#[derive(Debug, Clone)]
struct PromptM {
    thread: String,
    ok: bool,
    step: u64,
    err: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // read through Debug in failure reports
struct AnswerM {
    attn_id: String,
    thread: String,
    content: String,
    ok: bool,
    /// The harness named a revision it knew to be stale.
    stale: bool,
    step: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    NewThread,
    Send,
    Respond,
    Stop,
    Park,
    Resume,
    AttachBegin,
    AttachEnd,
    TcpCut,
    MsgCut,
    StaleConn,
    HoldResults,
    SwallowAcks,
    Offline,
    RunnerCrash,
    RunnerRestart,
    SpoolFault,
    HubCrash,
    Impostor,
    Trigger,
    Nap,
}

/// The step log, shared with the background tasks that end fault windows.
#[derive(Clone)]
struct Log {
    seed: u64,
    t0: Instant,
    rec: Recorder,
    lines: Arc<Mutex<Vec<String>>>,
    trace: bool,
}

impl Log {
    fn say(&self, s: impl Into<String>) {
        let step = self.rec.lock().step;
        let line = format!(
            "{:>6}ms #{:<3} {}",
            self.t0.elapsed().as_millis(),
            step,
            s.into()
        );
        if self.trace {
            eprintln!("[seed {}] {line}", self.seed);
        }
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(line);
    }

    fn tail(&self, n: usize) -> Vec<String> {
        let l = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        l[l.len().saturating_sub(n)..].to_vec()
    }
}

struct World {
    seed: u64,
    /// The action script.
    rng: Rng,
    /// Choices whose number depends on the system (answers).
    rng_answers: Rng,
    dir: PathBuf,
    rec: Recorder,
    hub: Option<TestHub>,
    boxes: Vec<BoxH>,
    threads: Vec<ThreadM>,
    prompts: BTreeMap<u64, PromptM>,
    answers: BTreeMap<u64, AnswerM>,
    /// Storage-failure triggers created so far (each dropped when its window ends).
    triggers: Vec<String>,
    trigger_until: Option<Instant>,
    /// Threads of a box while a second runner of it was around (see [`World::tainted`]).
    impostor_threads: BTreeSet<String>,
    log: Log,
    notes: Vec<String>,
    step: u64,
    next_pid: u64,
    next_tok: u64,
    next_runner: u64,
    reopen_checks: u64,
}

type Res<T = ()> = Result<T, Failure>;

fn open_db(dir: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(dir.join("hub/twr.db")).unwrap();
    c.busy_timeout(Duration::from_secs(10)).unwrap();
    c
}

impl World {
    async fn new(seed: u64, dir: PathBuf) -> World {
        let rec = Recorder::default();
        let hub = TestHub::start(&dir).await.expect("the first hub opens");
        let mut boxes = Vec::new();
        for name in BOXES {
            let b = Cutter::start(&hub.addr()).await;
            let ws = WsProxy::start(&b.url()).await;
            let a = Cutter::start(&ws.addr).await;
            let proj = dir.join(format!("proj-{name}"));
            std::fs::create_dir_all(&proj).unwrap();
            boxes.push(BoxH {
                name,
                a,
                ws,
                b,
                rdir: dir.join(format!("runner-{name}")),
                proj,
                runner: None,
                pending: None,
                impostor: None,
            });
        }
        let log = Log {
            seed,
            t0: Instant::now(),
            rec: rec.clone(),
            lines: Arc::default(),
            trace: std::env::var_os("TWR_FUZZ_TRACE").is_some(),
        };
        let mut w = World {
            seed,
            rng: Rng::new(seed),
            rng_answers: Rng::new(seed ^ 0xA5A5_5A5A_F00D_BEEF),
            dir,
            rec,
            hub: Some(hub),
            boxes,
            threads: Vec::new(),
            prompts: BTreeMap::new(),
            answers: BTreeMap::new(),
            triggers: Vec::new(),
            trigger_until: None,
            impostor_threads: BTreeSet::new(),
            log,
            notes: Vec::new(),
            step: 0,
            next_pid: 0,
            next_tok: 0,
            next_runner: 0,
            reopen_checks: 0,
        };
        for i in 0..w.boxes.len() {
            w.start_runner(i);
        }
        w
    }

    fn h(&self) -> &TestHub {
        self.hub.as_ref().unwrap()
    }

    fn say(&self, s: impl Into<String>) {
        self.log.say(s);
    }

    fn fail(&self, invariant: &str, detail: impl Into<String>) -> Failure {
        Failure {
            seed: self.seed,
            invariant: invariant.into(),
            detail: detail.into(),
            log_tail: self.log.tail(80),
        }
    }

    fn db(&self) -> rusqlite::Connection {
        open_db(&self.dir)
    }

    fn start_runner(&mut self, i: usize) {
        self.next_runner += 1;
        let id = self.next_runner;
        let b = &self.boxes[i];
        match RunnerProc::start(b.name, &b.a.url(), &b.rdir, &self.rec, id, false) {
            Ok(r) => {
                self.boxes[i].runner = Some(r);
                let name = self.boxes[i].name;
                self.say(format!("runner {id} of {name} started"));
            }
            Err(e) => {
                let name = self.boxes[i].name;
                self.say(format!("runner of {name} failed to start: {e:#}"));
                self.notes.push(format!(
                    "seed {} step {}: a runner of {name} failed to start: {e:#}",
                    self.seed, self.step
                ));
            }
        }
    }

    /// Starts box `i`'s runner in the background after `ms` (the box stays without one
    /// meanwhile).
    fn start_runner_later(&mut self, i: usize, ms: u64) {
        self.next_runner += 1;
        let id = self.next_runner;
        let b = &self.boxes[i];
        let (name, url, rdir, rec, log) = (
            b.name,
            b.a.url(),
            b.rdir.clone(),
            self.rec.clone(),
            self.log.clone(),
        );
        self.boxes[i].pending = Some(tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let r = tokio::task::spawn_blocking(move || {
                RunnerProc::start(name, &url, &rdir, &rec, id, false)
            })
            .await;
            match r {
                Ok(Ok(r)) => {
                    log.say(format!("runner {id} of {name} started (delayed)"));
                    Some(r)
                }
                Ok(Err(e)) => {
                    log.say(format!("runner {id} of {name} failed to start: {e:#}"));
                    None
                }
                Err(e) => {
                    log.say(format!("runner {id} of {name} failed to start: {e}"));
                    None
                }
            }
        }));
    }

    /// Threads of a box while a second runner of it was connected: two processes claimed the
    /// box (the second one's inventory says it runs none of the box's runtimes, so the hub closes
    /// them as lost; both may execute the same recorded commands, each runner deduplicating
    /// only its own), which no hub can undo. Their per-thread invariants are not checked (the
    /// global ones are), and the seed reports how many there were.
    fn tainted(&self) -> BTreeSet<String> {
        let mut t: BTreeSet<String> = self
            .rec
            .lock()
            .insts
            .values()
            .filter(|i| i.impostor)
            .map(|i| i.thread.clone())
            .collect();
        t.extend(self.impostor_threads.iter().cloned());
        t
    }

    /// The threads whose invariants are checked (not [`World::tainted`]).
    fn checked(&self) -> Vec<ThreadM> {
        let tainted = self.tainted();
        self.threads
            .iter()
            .filter(|t| !tainted.contains(&t.id))
            .cloned()
            .collect()
    }

    fn online(&self, i: usize) -> bool {
        let name = self.boxes[i].name;
        self.h()
            .hub()
            .boxes()
            .iter()
            .any(|(n, on, _)| n == name && *on)
    }

    // --- windows and upkeep ---------------------------------------------------------------

    /// Ends every fault window now (background runner starts and impostors included).
    async fn faults_off(&mut self) {
        for b in &self.boxes {
            {
                let mut s = b.ws.st();
                s.cut_msgs = None;
                s.hold_until = None;
                if s.swallow_until.take().is_some() {
                    // The connection that lost acks goes (the relay notices on its own too).
                    b.a.kill_current();
                }
                s.stale = None;
            }
            b.a.st().arm = None;
            b.b.st().arm = None;
            b.a.st().blocked_until = None;
            if let Some(r) = &b.runner {
                r.lbox().spool().inject_failures(0, 0);
            }
        }
        self.drop_triggers();
        for i in 0..self.boxes.len() {
            if let Some(t) = self.boxes[i].impostor.take() {
                t.abort();
                let _ = t.await;
            }
            if let Some(t) = self.boxes[i].pending.take() {
                if let Ok(Some(r)) = t.await {
                    self.boxes[i].runner = Some(r);
                }
            }
        }
    }

    fn drop_triggers(&mut self) {
        self.trigger_until = None;
        if self.triggers.is_empty() {
            return;
        }
        let db = self.db();
        for t in std::mem::take(&mut self.triggers) {
            db.execute_batch(&format!("DROP TRIGGER IF EXISTS {t};"))
                .unwrap();
        }
    }

    /// Restarts runners that died (spool failures) or whose delayed start is due; reopens a
    /// hub that failed.
    async fn upkeep(&mut self) -> Res {
        for i in 0..self.boxes.len() {
            if self.boxes[i]
                .pending
                .as_ref()
                .is_some_and(|t| t.is_finished())
            {
                let t = self.boxes[i].pending.take().unwrap();
                if let Ok(Some(r)) = t.await {
                    self.boxes[i].runner = Some(r);
                }
            }
            let dead = self.boxes[i].runner.as_ref().is_some_and(|r| r.finished());
            if dead {
                let r = self.boxes[i].runner.take().unwrap();
                let why = r.lbox().fatal_error();
                let id = r.id;
                r.crash().await;
                let name = self.boxes[i].name;
                self.say(format!(
                    "runner {id} of {name} had stopped ({why:?}); restarting"
                ));
                self.start_runner(i);
            } else if self.boxes[i].runner.is_none() && self.boxes[i].pending.is_none() {
                self.start_runner(i);
            }
        }
        if let Some(why) = self.h().hub().fatal_error() {
            self.say(format!("hub failed: {why}; reopening"));
            self.notes.push(format!(
                "seed {} step {}: hub failed fatally (reopened): {why}",
                self.seed, self.step
            ));
            self.reopen_hub().await?;
        }
        Ok(())
    }

    async fn reopen_hub(&mut self) -> Res {
        if let Some(h) = self.hub.take() {
            h.crash().await;
        }
        // Injected storage failures would hit the next hub's startup: off first.
        self.drop_triggers();
        let h = match TestHub::start(&self.dir).await {
            Ok(h) => h,
            Err(e) => return Err(self.fail("hub-reopen", format!("the hub does not open: {e:#}"))),
        };
        for b in &self.boxes {
            b.b.st().target = h.addr();
        }
        self.hub = Some(h);
        self.say("hub reopened");
        Ok(())
    }

    // --- actions -----------------------------------------------------------------------------

    /// The next action: a function of the seed and the step alone (never of what the system
    /// did), so a seed always plays the same script; an action that finds nothing to act on
    /// is a no-op.
    fn pick_action(&mut self) -> Action {
        use Action::*;
        // (Weights out of ~400; a second runner of a box is rare: it taints its box's threads.)
        let table = [
            (NewThread, 20),
            (Send, 88),
            (Respond, 72),
            (Stop, 16),
            (Park, 8),
            (Resume, 8),
            (AttachBegin, 8),
            (AttachEnd, 12),
            (TcpCut, 24),
            (MsgCut, 24),
            (StaleConn, 8),
            (HoldResults, 12),
            (SwallowAcks, 8),
            (Offline, 8),
            (RunnerCrash, 16),
            (RunnerRestart, 4),
            (SpoolFault, 8),
            (HubCrash, 12),
            (Impostor, 1),
            (Trigger, 12),
            (Nap, 36),
        ];
        let a = self.rng.weighted(&table);
        // Threads first.
        if self.step <= 2 {
            NewThread
        } else {
            a
        }
    }

    fn script(&mut self, pid: u64) -> String {
        let mut lines = vec![format!("#P{pid}")];
        let n = self.rng.range(1, 4);
        for k in 0..n {
            let d = self.rng.weighted(&[
                ("say", 30),
                ("perm", 14),
                ("ask", 10),
                ("replace", 8),
                ("reraise", 6),
                ("nap", 12),
                ("tool", 8),
                ("crash", if k + 1 == n { 4 } else { 0 }),
            ]);
            let line = match d {
                "say" => format!("say {}", self.rng.range(1, 8)),
                "replace" => format!("replace {}", self.rng.range(0, 300)),
                "reraise" => format!("reraise {}", self.rng.range(0, 300)),
                "nap" => format!("nap {}", self.rng.range(10, 250)),
                other => other.to_string(),
            };
            lines.push(line);
        }
        lines.join("\n")
    }

    fn pick_thread(&mut self) -> usize {
        self.rng.index(self.threads.len())
    }

    /// Open items raised by our threads' runtimes: (attn_id, thread, revision,
    /// first_revision, reason, summary, options).
    fn open_items(&self) -> Vec<Value> {
        let ids: BTreeSet<&str> = self.threads.iter().map(|t| t.id.as_str()).collect();
        self.h()
            .hub()
            .attention_list(true)
            .into_iter()
            .filter(|i| {
                let t = i["thread"].as_str().unwrap_or_default();
                ids.contains(t)
                    && i["attn_id"]
                        .as_str()
                        .is_some_and(|a| a.starts_with(&format!("{t}:")))
            })
            .collect()
    }

    fn send(&mut self, ti: usize, text: String, why: &str) {
        let pid = parse_pid(&text).unwrap();
        let t = self.threads[ti].clone();
        let r = self
            .h()
            .sync(|h| h.send(&t.id, &text, TurnFrom::User, Source::User(CLIENT.into())));
        let ok = r.is_ok();
        let short = text.replace('\n', " | ");
        match &r {
            Ok(o) => self.say(format!(
                "{why} {} <- [{short}] ok turn={:?} pending={} {}",
                t.name,
                o.turn,
                o.pending,
                o.dispatch_error.as_deref().unwrap_or("")
            )),
            Err(e) => self.say(format!(
                "{why} {} <- [{short}] ERR {}: {}",
                t.name, e.code, e.message
            )),
        }
        if ok {
            self.threads[ti].stopped_at = None;
        }
        self.prompts.insert(
            pid,
            PromptM {
                thread: t.id.clone(),
                ok,
                step: self.step,
                err: r.err().map(|e| format!("{}: {}", e.code, e.message)),
            },
        );
    }

    fn respond(&mut self, item: &Value, stale: bool) {
        let attn = item["attn_id"].as_str().unwrap().to_string();
        let thread = item["thread"].as_str().unwrap().to_string();
        let rev = item["revision"].as_u64().unwrap_or(0);
        let first = item["first_revision"].as_u64().unwrap_or(0);
        let content = item["summary"].as_str().unwrap_or_default().to_string();
        let options: Vec<String> = item["options"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        // Its own stream: how many items there are to answer is up to the system.
        let pick = self.rng_answers.index(options.len());
        let decision = if options.is_empty() {
            "allow".to_string()
        } else {
            options[pick].clone()
        };
        self.next_tok += 1;
        let tok = self.next_tok;
        let revision = if stale {
            if first != rev {
                first
            } else {
                rev.saturating_sub(1)
            }
        } else {
            rev
        };
        let answer = Answer {
            decision: decision.clone(),
            message: Some(format!("k{tok}|{content}")),
            ..Default::default()
        };
        let r = self
            .h()
            .sync(|h| h.respond_at(&attn, answer, Some(revision), &Caller::user(CLIENT)));
        let ok = r.is_ok();
        match &r {
            Ok(_) => self.say(format!(
                "respond k{tok} {attn}@{revision}{} [{content}] {decision}: ok",
                if stale { " (stale)" } else { "" }
            )),
            Err(e) => self.say(format!(
                "respond k{tok} {attn}@{revision}{} [{content}] {decision}: {} {}",
                if stale { " (stale)" } else { "" },
                e.code,
                e.message
            )),
        }
        self.answers.insert(
            tok,
            AnswerM {
                attn_id: attn,
                thread,
                content,
                ok,
                stale,
                step: self.step,
            },
        );
    }

    async fn act(&mut self, a: Action) -> Res {
        match a {
            Action::NewThread => {
                let i = self.rng.index(self.boxes.len());
                if self.threads.len() >= MAX_THREADS {
                    return Ok(());
                }
                let n = self.threads.len();
                let name = format!("t{n}");
                let req = NewThread {
                    project: self.boxes[i].proj.to_string_lossy().into_owned(),
                    runtime: Some(RUNTIME.into()),
                    name: Some(name.clone()),
                    box_name: Some(self.boxes[i].name.into()),
                    ..Default::default()
                };
                let r = self
                    .h()
                    .run(Duration::from_secs(20), |h| async move {
                        h.new_thread(req, CLIENT).await
                    })
                    .await;
                match r {
                    Some(Ok(v)) => {
                        let id = v["id"].as_str().unwrap().to_string();
                        self.say(format!(
                            "thread.new {name} on {} = {id}",
                            self.boxes[i].name
                        ));
                        if self.boxes[i]
                            .impostor
                            .as_ref()
                            .is_some_and(|t| !t.is_finished())
                        {
                            self.impostor_threads.insert(id.clone());
                        }
                        self.threads.push(ThreadM {
                            id,
                            name,
                            box_idx: i,
                            stopped_at: None,
                            stop_turns: 0,
                            lease: None,
                            dropped: 0,
                            maybe_dropped: 0,
                            nudges: 0,
                        });
                    }
                    Some(Err(e)) => self.say(format!(
                        "thread.new {name} on {}: {} {}",
                        self.boxes[i].name, e.code, e.message
                    )),
                    None => self.say(format!("thread.new {name}: no answer")),
                }
            }
            Action::Send => {
                let ti = self.pick_thread();
                self.next_pid += 1;
                let pid = self.next_pid;
                let text = self.script(pid);
                if self.threads.is_empty() {
                    return Ok(());
                }
                self.send(ti, text, "send");
            }
            Action::Respond => {
                let items = self.open_items();
                let k = self.rng.index(items.len());
                let stale = self.rng.chance(12);
                if items.is_empty() {
                    return Ok(());
                }
                let item = items[k].clone();
                self.respond(&item, stale);
            }
            Action::Stop => {
                let ti = self.pick_thread();
                if self.threads.is_empty() {
                    return Ok(());
                }
                let id = self.threads[ti].id.clone();
                let pending_before = self
                    .h()
                    .hub()
                    .get_thread(&id)
                    .map_or(0, |j| j["pending"].as_u64().unwrap_or(0));
                let r = self.h().sync(|h| h.stop_thread(&id, CLIENT));
                match r {
                    Ok(v) => {
                        let dropped = v["dropped_prompts"].as_u64().unwrap_or(0);
                        self.threads[ti].dropped += dropped;
                        self.threads[ti].stopped_at = Some(self.step);
                        self.threads[ti].stop_turns =
                            self.h().hub().thread_view(&id).map_or(0, |v| v.turns);
                        self.say(format!("stop {}: {v}", self.threads[ti].name));
                    }
                    Err(e) => {
                        self.threads[ti].maybe_dropped += pending_before;
                        let after = self
                            .h()
                            .hub()
                            .get_thread(&id)
                            .map_or(0, |j| j["pending"].as_u64().unwrap_or(0));
                        self.say(format!(
                            "stop {}: {} {} (queue {pending_before} -> {after})",
                            self.threads[ti].name, e.code, e.message
                        ));
                        if after < pending_before {
                            self.notes.push(format!(
                                "seed {} step {}: thread.stop answered {} ({}) yet dropped {} queued prompt(s)",
                                self.seed,
                                self.step,
                                e.code,
                                e.message,
                                pending_before - after
                            ));
                        }
                    }
                }
            }
            Action::Park => {
                let ti = self.pick_thread();
                let force = self.rng.chance(50);
                if self.threads.is_empty() {
                    return Ok(());
                }
                let id = self.threads[ti].id.clone();
                let r = self
                    .h()
                    .run(Duration::from_secs(40), move |h| async move {
                        h.park_thread(&id, false, force, &Caller::user(CLIENT))
                            .await
                    })
                    .await;
                let name = self.threads[ti].name.clone();
                match r {
                    Some(Ok(o)) => {
                        self.threads[ti].stopped_at = None;
                        self.say(format!("park {name} force={force}: ok {}", json!(o)));
                    }
                    Some(Err(e)) => self.say(format!(
                        "park {name} force={force}: {} {}",
                        e.code, e.message
                    )),
                    None => self.say(format!("park {name}: no answer in 40s")),
                }
            }
            Action::Resume => {
                let ti = self.pick_thread();
                if self.threads.is_empty() {
                    return Ok(());
                }
                self.resume(ti).await;
            }
            Action::AttachBegin => {
                let ti = self.pick_thread();
                let force = self.rng.chance(50);
                if self.threads.is_empty() {
                    return Ok(());
                }
                let id = self.threads[ti].id.clone();
                let r = self
                    .h()
                    .run(Duration::from_secs(40), move |h| async move {
                        h.attach_begin(&id, force, CLIENT).await
                    })
                    .await;
                let name = self.threads[ti].name.clone();
                match r {
                    Some(Ok(g)) => {
                        self.threads[ti].lease = Some(g.lease);
                        self.threads[ti].stopped_at = None;
                        self.say(format!("attach {name} force={force}: lease {}", g.lease));
                    }
                    Some(Err(e)) => self.say(format!(
                        "attach {name} force={force}: {} {}",
                        e.code, e.message
                    )),
                    None => self.say(format!("attach {name}: no answer in 40s")),
                }
            }
            Action::AttachEnd => {
                let with: Vec<usize> = (0..self.threads.len())
                    .filter(|&i| self.threads[i].lease.is_some())
                    .collect();
                let k = self.rng.index(with.len());
                if with.is_empty() {
                    return Ok(());
                }
                let ti = with[k];
                self.attach_end(ti);
            }
            Action::TcpCut => {
                let i = self.rng.index(self.boxes.len());
                let up = self.rng.chance(50);
                let bytes = self.rng.range(1, 6000);
                // Up: the hub reads the partial frame (cutter B); down: the runner does (A).
                let c = if up {
                    &self.boxes[i].b
                } else {
                    &self.boxes[i].a
                };
                c.st().arm = Some((up, bytes));
                self.say(format!(
                    "{}: cut {} after {bytes} bytes",
                    self.boxes[i].name,
                    if up { "runner->hub" } else { "hub->runner" }
                ));
            }
            Action::MsgCut => {
                let i = self.rng.index(self.boxes.len());
                let up = self.rng.chance(50);
                let n = self.rng.range(1, 20);
                self.boxes[i].ws.st().cut_msgs = Some((up, n));
                self.say(format!(
                    "{}: cut {} at message {n} (lost)",
                    self.boxes[i].name,
                    if up { "runner->hub" } else { "hub->runner" }
                ));
            }
            Action::StaleConn => {
                let i = self.rng.index(self.boxes.len());
                let k = self.rng.range(1, 4) as usize;
                self.boxes[i].ws.st().stale = Some(k);
                // Kill the current connection so the stale one is a fresh one.
                self.boxes[i].a.kill_current();
                self.say(format!(
                    "{}: next connection goes stale withholding {k} messages",
                    self.boxes[i].name
                ));
            }
            Action::HoldResults => {
                let i = self.rng.index(self.boxes.len());
                let ms = self.rng.range(50, 1500);
                self.boxes[i].ws.st().hold_until = Some(Instant::now() + Duration::from_millis(ms));
                self.say(format!(
                    "{}: results withheld for {ms}ms",
                    self.boxes[i].name
                ));
            }
            Action::SwallowAcks => {
                let i = self.rng.index(self.boxes.len());
                let ms = self.rng.range(50, 1500);
                self.boxes[i].ws.st().swallow_until =
                    Some(Instant::now() + Duration::from_millis(ms));
                self.say(format!(
                    "{}: acks swallowed for {ms}ms (then that connection is cut)",
                    self.boxes[i].name
                ));
            }
            Action::Offline => {
                let i = self.rng.index(self.boxes.len());
                let ms = self.rng.range(100, 2500);
                self.boxes[i].a.st().blocked_until =
                    Some(Instant::now() + Duration::from_millis(ms));
                self.boxes[i].a.kill_current();
                self.say(format!("{}: unreachable for {ms}ms", self.boxes[i].name));
            }
            Action::RunnerCrash => {
                let i = self.rng.index(self.boxes.len());
                let later = self.rng.chance(40);
                let ms = self.rng.range(100, 2000);
                if let Some(r) = self.boxes[i].runner.take() {
                    let id = r.id;
                    r.crash().await;
                    let name = self.boxes[i].name;
                    if later {
                        self.say(format!("runner {id} of {name} crashed; back in {ms}ms"));
                        self.start_runner_later(i, ms);
                    } else {
                        self.say(format!("runner {id} of {name} crashed"));
                        self.start_runner(i);
                    }
                }
            }
            Action::RunnerRestart => {
                let i = self.rng.index(self.boxes.len());
                if let Some(r) = self.boxes[i].runner.take() {
                    let id = r.id;
                    r.graceful().await;
                    let name = self.boxes[i].name;
                    self.say(format!("runner {id} of {name} stopped gracefully"));
                    self.start_runner(i);
                }
            }
            Action::SpoolFault => {
                let i = self.rng.index(self.boxes.len());
                let appends = self.rng.range(1, 7) as u32;
                let finishes = self.rng.range(0, 3) as u32;
                if let Some(r) = &self.boxes[i].runner {
                    r.lbox().spool().inject_failures(appends, finishes);
                    let name = self.boxes[i].name;
                    self.say(format!(
                        "{name}: spool fails the next {appends} appends, {finishes} result writes"
                    ));
                }
            }
            Action::HubCrash => {
                self.say("hub crash");
                self.reopen_hub().await?;
            }
            Action::Impostor => {
                let i = self.rng.index(self.boxes.len());
                let ms = self.rng.range(200, 1200);
                if self.boxes[i].impostor.is_none() {
                    self.next_runner += 1;
                    let id = self.next_runner;
                    let b = &self.boxes[i];
                    let dir = self.dir.join(format!("impostor-{}-{id}", b.name));
                    match RunnerProc::start(b.name, &b.a.url(), &dir, &self.rec, id, true) {
                        Ok(r) => {
                            let name = self.boxes[i].name;
                            self.say(format!(
                                "impostor runner {id} of {name} (own data dir) for {ms}ms"
                            ));
                            let log = self.log.clone();
                            for t in &self.threads {
                                if t.box_idx == i {
                                    self.impostor_threads.insert(t.id.clone());
                                }
                            }
                            self.boxes[i].impostor = Some(tokio::spawn(async move {
                                // Crashed when its window ends, or when the task is aborted
                                // (the runner's runtime goes with the dropped future).
                                tokio::time::sleep(Duration::from_millis(ms)).await;
                                r.crash().await;
                                log.say(format!("impostor runner {id} of {name} crashed"));
                            }));
                        }
                        Err(e) => self.say(format!("impostor failed to start: {e:#}")),
                    }
                }
            }
            Action::Trigger => {
                let what = self.rng.weighted(&[
                    ("agent.text", 3),
                    ("turn.ended", 3),
                    ("turn.started", 2),
                    ("attention.raised", 2),
                    ("attention.resolved", 2),
                    ("thread.started", 2),
                    ("thread.crashed", 1),
                    ("report.ready", 1),
                    ("*", 2),
                    ("runner_acks", 2),
                    ("remote_outbox", 1),
                    ("pending_prompts", 1),
                ]);
                let ms = self
                    .rng
                    .weighted(&[(200u64, 3), (900, 3), (2600, 2), (4000, 1)]);
                if self.trigger_until.is_none_or(|u| u <= Instant::now()) {
                    let name = format!("fuzz_inject_{}", self.step);
                    let raise = "BEGIN SELECT RAISE(ABORT, 'fuzz: injected'); END;";
                    let sql = match what {
                        "*" => format!("CREATE TRIGGER {name} BEFORE INSERT ON events {raise}"),
                        "runner_acks" => format!(
                            "CREATE TRIGGER {name} BEFORE UPDATE ON runner_acks {raise} \
                             CREATE TRIGGER {name}_i BEFORE INSERT ON runner_acks {raise}"
                        ),
                        "remote_outbox" | "pending_prompts" => {
                            format!("CREATE TRIGGER {name} BEFORE INSERT ON {what} {raise}")
                        }
                        kind => format!(
                            "CREATE TRIGGER {name} BEFORE INSERT ON events WHEN NEW.kind = '{kind}' {raise}"
                        ),
                    };
                    self.db().execute_batch(&sql).unwrap();
                    self.triggers.push(name.clone());
                    self.triggers.push(format!("{name}_i"));
                    self.trigger_until = Some(Instant::now() + Duration::from_millis(ms));
                    self.say(format!("storage refuses {what} for {ms}ms"));
                    let (dir, log) = (self.dir.clone(), self.log.clone());
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                        let _ = tokio::task::spawn_blocking(move || {
                            open_db(&dir).execute_batch(&format!(
                                "DROP TRIGGER IF EXISTS {name}; DROP TRIGGER IF EXISTS {name}_i;"
                            ))
                        })
                        .await;
                        log.say(format!("storage refuses {what}: window over"));
                    });
                }
            }
            Action::Nap => {
                let ms = self.rng.range(20, 400);
                self.say(format!("nap {ms}ms"));
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
        }
        Ok(())
    }

    async fn resume(&mut self, ti: usize) {
        let id = self.threads[ti].id.clone();
        let r = self
            .h()
            .run(Duration::from_secs(40), move |h| async move {
                h.resume_thread(&id, &Caller::user(CLIENT)).await
            })
            .await;
        let name = self.threads[ti].name.clone();
        match r {
            Some(Ok(o)) => {
                self.threads[ti].stopped_at = None;
                self.say(format!("resume {name}: ok {}", json!(o)));
            }
            Some(Err(e)) => self.say(format!("resume {name}: {} {}", e.code, e.message)),
            None => self.say(format!("resume {name}: no answer in 40s")),
        }
    }

    fn attach_end(&mut self, ti: usize) {
        let Some(lease) = self.threads[ti].lease else {
            return;
        };
        let id = self.threads[ti].id.clone();
        let r = self.h().sync(|h| h.attach_end(&id, CLIENT, lease, None));
        let name = self.threads[ti].name.clone();
        match r {
            Ok(b) => {
                self.threads[ti].lease = None;
                self.say(format!("attach end {name} lease {lease}: {b}"));
            }
            Err(e) => {
                // `conflict` while it is still starting, `internal` when storage refused the
                // detach: the lease still holds, tried again later.
                self.say(format!(
                    "attach end {name} lease {lease}: {} {}",
                    e.code, e.message
                ));
                if e.code == twr_core::proto::codes::NOT_FOUND {
                    self.threads[ti].lease = None;
                }
            }
        }
    }

    // --- quiescence --------------------------------------------------------------------------

    /// Faults off, everything up, every item answered, every queue drained: returns once the
    /// system is at rest (inv7 when it never gets there).
    async fn settle(&mut self) -> Res {
        self.say("settle: faults off");
        self.faults_off().await;
        let deadline = Instant::now() + SETTLE;
        let mut quiet_since: Option<(Instant, u64)> = None;
        let mut idle_pending: HashMap<String, Instant> = HashMap::new();
        let mut warned: BTreeSet<String> = BTreeSet::new();
        loop {
            if Instant::now() > deadline {
                return Err(self.fail("inv7-stuck", self.stuck_report()));
            }
            self.upkeep().await?;
            let online = (0..self.boxes.len()).all(|i| self.online(i));
            if !online {
                quiet_since = None;
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
            // Leases and parks end; items are answered.
            for ti in 0..self.threads.len() {
                if self.threads[ti].lease.is_some() {
                    self.attach_end(ti);
                }
            }
            let items = self.open_items();
            for item in &items {
                self.respond(item, false);
            }
            let mut busy = !items.is_empty();
            let tainted = self.tainted();
            for ti in 0..self.threads.len() {
                let t = self.threads[ti].clone();
                if tainted.contains(&t.id) {
                    continue;
                }
                let j = self.h().hub().get_thread(&t.id).unwrap();
                let view = self.h().hub().thread_view(&t.id).unwrap();
                let pending = j["pending"].as_u64().unwrap_or(0);
                if view.attached.is_some() {
                    busy = true;
                    if t.lease.is_none() && warned.insert(t.id.clone()) {
                        self.say(format!(
                            "settle: {} attached under a lease the harness does not hold",
                            t.name
                        ));
                    }
                }
                if view.parked {
                    busy = true;
                    self.resume(ti).await;
                    continue;
                }
                if view.current_turn.is_some() || pending > 0 {
                    busy = true;
                }
                // A queue nothing will dispatch by itself (after a crash, or held after its
                // runtime failed to start): a new prompt dispatches it.
                if view.current_turn.is_none() && pending > 0 && view.attached.is_none() {
                    let since = *idle_pending
                        .entry(t.id.clone())
                        .or_insert_with(Instant::now);
                    if since.elapsed() > Duration::from_millis(1500) && t.nudges < 6 {
                        idle_pending.remove(&t.id);
                        self.threads[ti].nudges += 1;
                        self.next_pid += 1;
                        let pid = self.next_pid;
                        self.say(format!(
                            "settle: {} idle with {pending} queued (last turn {:?}, crashed {}); nudging",
                            t.name,
                            view.last_turn.as_ref().map(|l| l.result),
                            view.crashed
                        ));
                        self.send(ti, format!("#P{pid}\nsay 1"), "nudge");
                    }
                } else {
                    idle_pending.remove(&t.id);
                }
            }
            for b in &self.boxes {
                if let Some(r) = &b.runner {
                    if r.lbox().spool().len().unwrap_or(1) > 0 {
                        busy = true;
                    }
                }
            }
            if self.outbox_rows(&tainted) > 0 {
                busy = true;
            }
            let head = self
                .h()
                .hub()
                .control_read(0, 0)
                .map(|(_, h)| h)
                .unwrap_or(0);
            if busy {
                quiet_since = None;
            } else {
                match quiet_since {
                    Some((at, h)) if h == head => {
                        if at.elapsed() > Duration::from_millis(400) {
                            self.say(format!("settled at control head {head}"));
                            return Ok(());
                        }
                    }
                    _ => quiet_since = Some((Instant::now(), head)),
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// `remote_outbox` rows (box, cmd_id, thread, op) of threads not in `skip`.
    fn outbox(&self, skip: &BTreeSet<String>) -> Vec<String> {
        let db = self.db();
        let mut st = db
            .prepare("SELECT box, cmd_id, thread, op FROM remote_outbox")
            .unwrap();
        let rows: Vec<(String, String, String, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows.into_iter()
            .filter(|(_, _, t, _)| !skip.contains(t))
            .map(|(b, c, t, o)| format!("{b} {c} {t} {o}"))
            .collect()
    }

    fn outbox_rows(&self, skip: &BTreeSet<String>) -> usize {
        self.outbox(skip).len()
    }

    fn stuck_report(&self) -> String {
        let mut out = String::from("the system did not quiesce within 60s:");
        let h = self.h().hub();
        for (i, b) in self.boxes.iter().enumerate() {
            let spool = b
                .runner
                .as_ref()
                .map(|r| r.lbox().spool().len().unwrap_or(0));
            out += &format!(
                "\n  box {} online={} runner={:?} spool={spool:?}",
                b.name,
                self.online(i),
                b.runner.as_ref().map(|r| r.id)
            );
        }
        for t in &self.threads {
            let j = h.get_thread(&t.id).unwrap();
            let rec = self.rec.lock();
            let live: Vec<String> = rec
                .insts
                .iter()
                .filter(|(_, i)| i.live && i.thread == t.id)
                .map(|(k, i)| format!("inst {k}@runner{} session {}", i.runner, i.session))
                .collect();
            out += &format!(
                "\n  {} {}: status={} turn={} pending={} live={} attention={} runtimes={live:?}",
                t.name,
                t.id,
                j["status"],
                j["current_turn"],
                j["pending"],
                j["live"],
                j["attention"]
            );
        }
        let rows = self.outbox(&BTreeSet::new());
        out += &format!(
            "\n  remote_outbox: {rows:?}\n  tainted: {:?}",
            self.tainted()
        );
        out
    }

    // --- invariants --------------------------------------------------------------------------

    async fn checkpoint(&mut self, last: bool) -> Res {
        self.settle().await?;
        self.rec.lock().step = self.step;
        self.check_all().await?;
        if last || self.rng.chance(70) {
            self.check_reopen().await?;
        }
        Ok(())
    }

    async fn check_all(&mut self) -> Res {
        self.check_log()?;
        self.check_frames()?;
        self.check_fold()?;
        self.check_runtimes()?;
        self.check_answers()?;
        self.check_prompts()?;
        self.check_delivery_state()?;
        self.check_stopped()?;
        Ok(())
    }

    /// inv1: seqs contiguous per thread, ids unique, gseq contiguous.
    fn check_log(&self) -> Res {
        let db = self.db();
        let mut st = db
            .prepare("SELECT thread, seq, id FROM events ORDER BY thread, seq")
            .unwrap();
        let rows: Vec<(String, i64, String)> = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut ids = BTreeSet::new();
        let mut last: HashMap<String, i64> = HashMap::new();
        for (thread, seq, id) in rows {
            let prev = last.insert(thread.clone(), seq).unwrap_or(0);
            if seq != prev + 1 {
                return Err(self.fail(
                    "inv1-seq",
                    format!("thread {thread}: seq {seq} follows {prev}"),
                ));
            }
            if !ids.insert(id.clone()) {
                return Err(self.fail("inv1-id", format!("event id {id} appears twice")));
            }
        }
        let mut st = db
            .prepare("SELECT gseq FROM control ORDER BY gseq")
            .unwrap();
        let gs: Vec<i64> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (i, g) in gs.iter().enumerate() {
            if *g != i as i64 + 1 {
                return Err(self.fail(
                    "inv1-gseq",
                    format!(
                        "control gseq {g} at position {} (expected {})",
                        i + 1,
                        i + 1
                    ),
                ));
            }
        }
        let head = self.h().hub().control_read(0, 0).unwrap().1;
        if head != gs.len() as u64 {
            return Err(self.fail(
                "inv1-gseq",
                format!("live control head {head} but {} control rows", gs.len()),
            ));
        }
        Ok(())
    }

    /// inv5 (frames): a turn whose runtime-sent `turn.ended` is recorded has every event its
    /// runtime emitted in it, once and in order (nothing lost or doubled across reconnects,
    /// replays, fencing and hub restarts).
    fn check_frames(&self) -> Res {
        let checked = self.checked();
        let rec = self.rec.lock();
        for t in &checked {
            let mut logged: BTreeMap<u64, Vec<String>> = BTreeMap::new();
            let mut adapter_end: BTreeSet<u64> = BTreeSet::new();
            for e in self.events(&t.id) {
                let (Some(turn), Source::Adapter(_)) = (e.turn, &e.source) else {
                    continue;
                };
                if e.kind() == "turn.ended" {
                    adapter_end.insert(turn);
                }
                if let Some(sig) = frame_sig(&e.body) {
                    logged.entry(turn).or_default().push(sig);
                }
            }
            for turn in adapter_end {
                let got = logged.remove(&turn).unwrap_or_default();
                let sent = rec
                    .emitted
                    .get(&(t.id.clone(), turn))
                    .cloned()
                    .unwrap_or_default();
                if got != sent {
                    return Err(self.fail(
                        "inv5-frames",
                        format!(
                            "thread {} turn {turn}: the runtime emitted {sent:?} but the log has {got:?}",
                            t.name
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn events(&self, thread: &str) -> Vec<Event> {
        self.h()
            .hub()
            .thread_events(thread, 0, 1_000_000)
            .unwrap()
            .1
    }

    /// inv2 (fold): the reducers folded from the log equal the live state.
    fn check_fold(&self) -> Res {
        let h = self.h().hub();
        let mut all = Vec::new();
        for t in &self.threads {
            all.extend(self.events(&t.id));
        }
        let reg = Registry::from_events(&all);
        for t in &self.threads {
            let live = serde_json::to_value(h.thread_view(&t.id).unwrap()).unwrap();
            let folded = serde_json::to_value(reg.get(&t.id).cloned()).unwrap();
            if live != folded {
                return Err(self.fail(
                    "inv2-fold",
                    format!(
                        "thread {} ({}): live view differs from the log fold: {}",
                        t.name,
                        t.id,
                        json_diff(&live, &folded).join("; ")
                    ),
                ));
            }
        }
        let (ctrl, _) = h.control_read(0, 10_000_000).unwrap();
        let mut stack = AttentionStack::new();
        for (g, ev) in &ctrl {
            stack.apply_at(ev, Some(*g));
        }
        let folded: Vec<Value> = stack
            .ordered(&Default::default())
            .into_iter()
            .map(|i| serde_json::to_value(i).unwrap())
            .collect();
        let live: Vec<Value> = h
            .attention_list(true)
            .into_iter()
            .map(|mut v| {
                v.as_object_mut().unwrap().remove("thread_name");
                v
            })
            .collect();
        if Value::from(folded.clone()) != Value::from(live.clone()) {
            return Err(self.fail(
                "inv2-fold",
                format!(
                    "attention stack differs from the log fold: {}",
                    json_diff(&Value::from(live), &Value::from(folded)).join("; ")
                ),
            ));
        }
        Ok(())
    }

    fn snapshot(&self) -> Value {
        let h = self.h().hub();
        let mut threads = Map::new();
        for t in &self.threads {
            let mut j = h.get_thread(&t.id).unwrap();
            j.as_object_mut().unwrap().remove("live");
            // (When its `work` was read is the clock's, not the log's.)
            if let Some(w) = j.get_mut("work").and_then(Value::as_object_mut) {
                w.remove("as_of");
            }
            threads.insert(t.id.clone(), j);
        }
        let attention: Vec<Value> = h.attention_list(true);
        let head = h.control_read(0, 0).unwrap().1;
        json!({"threads": threads, "attention": attention, "head": head})
    }

    /// inv2 (reopen): a hub reopened from the database (runners kept away) has the state the
    /// live one had.
    async fn check_reopen(&mut self) -> Res {
        // The runners go first (their disconnection is recorded), so that nothing changes
        // between the snapshot and the crash.
        for b in &self.boxes {
            b.a.st().blocked_until = Some(Instant::now() + Duration::from_secs(3600));
            b.a.kill_current();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut stable: Option<(Instant, u64)> = None;
        loop {
            if Instant::now() > deadline {
                return Err(self.fail(
                    "inv7-stuck",
                    "the boxes did not go offline (or the log kept growing) with their runners cut",
                ));
            }
            let offline = (0..self.boxes.len()).all(|i| !self.online(i));
            let head = self.h().hub().control_read(0, 0).unwrap().1;
            match stable {
                Some((at, h)) if offline && h == head => {
                    if at.elapsed() > Duration::from_millis(200) {
                        break;
                    }
                }
                _ => stable = Some((Instant::now(), head)),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let before = self.snapshot();
        if let Some(h) = self.hub.take() {
            h.crash().await;
        }
        let h = match TestHub::start(&self.dir).await {
            Ok(h) => h,
            Err(e) => return Err(self.fail("hub-reopen", format!("the hub does not open: {e:#}"))),
        };
        for b in &self.boxes {
            b.b.st().target = h.addr();
            b.a.kill_current();
        }
        self.hub = Some(h);
        let after = self.snapshot();
        for b in &self.boxes {
            b.a.st().blocked_until = None;
        }
        self.reopen_checks += 1;
        self.say("replay check: hub reopened");
        if before != after {
            return Err(self.fail(
                "inv2-reopen",
                format!(
                    "the reopened hub differs from the live one: {}",
                    json_diff(&before, &after).join("; ")
                ),
            ));
        }
        // Back to rest with the runners.
        self.settle().await
    }

    /// inv3: no overlap seen; the live runtimes are exactly those the log shows live.
    fn check_runtimes(&self) -> Res {
        let checked = self.checked();
        let rec = self.rec.lock();
        if let Some(v) = rec.violations.iter().find(|v| v.contains("inv3")) {
            return Err(self.fail("inv3-overlap", v.clone()));
        }
        for t in &checked {
            let evs = self.events(&t.id);
            let mut live: Option<Option<String>> = None;
            for e in &evs {
                match &e.body {
                    EventBody::ThreadStarted(p) if matches!(e.source, Source::Adapter(_)) => {
                        live = Some(p.session_id.clone())
                    }
                    EventBody::ThreadStopped(_)
                    | EventBody::ThreadCrashed(_)
                    | EventBody::ThreadParked(_)
                    | EventBody::ThreadAttached(_) => live = None,
                    _ => {}
                }
            }
            let insts: Vec<(u64, &Inst)> = rec
                .insts
                .iter()
                .filter(|(_, i)| i.live && i.thread == t.id)
                .map(|(k, i)| (*k, i))
                .collect();
            let ok = match (&live, insts.as_slice()) {
                (None, []) => true,
                (Some(s), [(_, i)]) => s.as_deref() == Some(i.session.as_str()),
                _ => false,
            };
            if !ok {
                return Err(self.fail(
                    "inv3-agreement",
                    format!(
                        "thread {} ({}): the log shows {} but the boxes run {:?}",
                        t.name,
                        t.id,
                        match &live {
                            Some(s) => format!("a live runtime (session {s:?})"),
                            None => "no live runtime".into(),
                        },
                        insts
                            .iter()
                            .map(|(k, i)| format!(
                                "inst {k}@runner{} session {} spawned at step {}",
                                i.runner, i.session, i.spawned_step
                            ))
                            .collect::<Vec<_>>()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// inv4: answers.
    fn check_answers(&self) -> Res {
        let tainted = self.tainted();
        let rec = self.rec.lock();
        let mut seen: HashMap<u64, usize> = HashMap::new();
        for (n, d) in rec.deliveries.iter().enumerate() {
            if tainted.contains(&d.thread) {
                continue;
            }
            let msg = d.message.clone().unwrap_or_default();
            let Some((tok, content)) = msg
                .strip_prefix('k')
                .and_then(|m| m.split_once('|'))
                .and_then(|(t, c)| Some((t.parse::<u64>().ok()?, c.to_string())))
            else {
                return Err(self.fail(
                    "inv4-unknown",
                    format!(
                        "runtime instance {} got an answer the harness never gave: {d:?}",
                        d.inst
                    ),
                ));
            };
            let Some(a) = self.answers.get(&tok) else {
                return Err(self.fail("inv4-unknown", format!("unknown answer token {tok}: {d:?}")));
            };
            if let Some(prev) = seen.insert(tok, n) {
                return Err(self.fail(
                    "inv4-twice",
                    format!(
                        "answer k{tok} ({}) delivered twice: {:?} and {d:?}",
                        a.attn_id, rec.deliveries[prev]
                    ),
                ));
            }
            if !a.ok {
                return Err(self.fail(
                    "inv4-refused-delivered",
                    format!(
                        "answer k{tok} was refused by the hub yet reached a runtime: {a:?} {d:?}"
                    ),
                ));
            }
            // The hub id is `<thread>:<gen>:<adapter id>`; the content names the instance.
            let expect_inst = content
                .strip_prefix('i')
                .and_then(|c| c.split_once('/'))
                .and_then(|(i, _)| i.parse::<u64>().ok());
            let adapter_tail = a
                .attn_id
                .strip_prefix(&format!("{}:", a.thread))
                .and_then(|r| r.split_once(':'))
                .map(|(_, id)| id.to_string());
            if d.thread != a.thread
                || adapter_tail.as_deref() != Some(d.adapter_id.as_str())
                || expect_inst != Some(d.inst)
            {
                return Err(self.fail(
                    "inv4-routing",
                    format!(
                        "answer k{tok} for {} [{}] reached instance {} as {}: {d:?}",
                        a.attn_id, a.content, d.inst, d.adapter_id
                    ),
                ));
            }
            if d.accepted && d.content.as_deref() != Some(content.as_str()) {
                return Err(self.fail(
                    "inv4-incarnation",
                    format!(
                        "answer k{tok} given for [{content}] was taken by the runtime for [{:?}]",
                        d.content
                    ),
                ));
            }
        }
        for (tok, a) in &self.answers {
            if a.stale && a.ok && !tainted.contains(&a.thread) {
                return Err(self.fail(
                    "inv4-stale-accepted",
                    format!("answer k{tok} named a stale revision and was accepted: {a:?}"),
                ));
            }
        }
        Ok(())
    }

    /// inv5: prompts.
    fn check_prompts(&self) -> Res {
        let tainted = self.tainted();
        let checked = self.checked();
        let mut started: HashMap<u64, Vec<(String, u64)>> = HashMap::new();
        for t in &checked {
            let evs = self.events(&t.id);
            let mut open: BTreeSet<u64> = BTreeSet::new();
            for e in &evs {
                match &e.body {
                    EventBody::TurnStarted(p) => {
                        let turn = e.turn.unwrap_or(0);
                        open.insert(turn);
                        let retry = p
                            .ext
                            .as_ref()
                            .and_then(|x| x.get("twr"))
                            .and_then(|x| x.get("retry_of"))
                            .is_some();
                        if let (Some(pid), false) = (parse_pid(&p.prompt), retry) {
                            started.entry(pid).or_default().push((t.id.clone(), turn));
                        }
                    }
                    EventBody::TurnEnded(_) => {
                        let turn = e.turn.unwrap_or(0);
                        if !open.remove(&turn) {
                            return Err(self.fail(
                                "inv5-double-end",
                                format!(
                                    "thread {} ({}): turn.ended (seq {}, source {:?}) for turn {turn}, which is not open (ended before or never started)",
                                    t.name, t.id, e.seq, e.source
                                ),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            if !open.is_empty() {
                return Err(self.fail(
                    "inv5-unended",
                    format!("thread {} ({}): turns {open:?} never ended", t.name, t.id),
                ));
            }
        }
        let mut never: HashMap<String, u64> = HashMap::new();
        for (pid, p) in &self.prompts {
            if tainted.contains(&p.thread) {
                continue;
            }
            let n = started.get(pid).map_or(0, Vec::len);
            if !p.ok && n > 0 {
                return Err(self.fail(
                    "inv5-phantom",
                    format!(
                        "prompt P{pid} was refused ({:?}) yet started: {:?}",
                        p.err, started[pid]
                    ),
                ));
            }
            if n > 1 {
                return Err(self.fail(
                    "inv5-twice",
                    format!("prompt P{pid} started {n} times: {:?}", started[pid]),
                ));
            }
            if let Some(v) = started.get(pid) {
                if v[0].0 != p.thread {
                    return Err(self.fail(
                        "inv5-thread",
                        format!("prompt P{pid} for {} started on {:?}", p.thread, v),
                    ));
                }
            }
            if p.ok && n == 0 {
                *never.entry(p.thread.clone()).or_default() += 1;
            }
        }
        for t in &checked {
            let n = never.get(&t.id).copied().unwrap_or(0);
            if n < t.dropped || n > t.dropped + t.maybe_dropped {
                let lost: Vec<String> = self
                    .prompts
                    .iter()
                    .filter(|(pid, p)| p.ok && p.thread == t.id && !started.contains_key(pid))
                    .map(|(pid, p)| format!("P{pid}@step{}", p.step))
                    .collect();
                return Err(self.fail(
                    "inv5-lost",
                    format!(
                        "thread {} ({}): {n} accepted prompts never started {lost:?}, but stops \
                         dropped {} (failed stops: at most {} more)",
                        t.name, t.id, t.dropped, t.maybe_dropped
                    ),
                ));
            }
        }
        let rec = self.rec.lock();
        let mut got: HashMap<u64, Vec<&PromptRx>> = HashMap::new();
        for p in &rec.prompts {
            if tainted.contains(&p.thread) {
                continue;
            }
            if let Some(pid) = p.pid {
                got.entry(pid).or_default().push(p);
            }
        }
        for (pid, v) in &got {
            if v.len() > 1 {
                return Err(self.fail(
                    "inv5-run-twice",
                    format!("prompt P{pid} reached runtimes {} times: {v:?}", v.len()),
                ));
            }
            let p = v[0];
            let ok = started
                .get(pid)
                .is_some_and(|s| s.iter().any(|(t, turn)| *t == p.thread && *turn == p.turn));
            if !ok {
                return Err(self.fail(
                    "inv5-unrecorded-run",
                    format!(
                        "prompt P{pid} ran as turn {} of {} (instance {}) without that turn.started: {:?}",
                        p.turn,
                        p.thread,
                        p.inst,
                        started.get(pid)
                    ),
                ));
            }
        }
        Ok(())
    }

    /// inv6: spools and outbox drained, watermarks at the runners' last rseq.
    fn check_delivery_state(&self) -> Res {
        let tainted = self.tainted();
        let db = self.db();
        let rows = self.outbox(&tainted);
        if !rows.is_empty() {
            return Err(self.fail("inv6-outbox", format!("remote_outbox rows left: {rows:?}")));
        }
        for b in &self.boxes {
            let Some(r) = &b.runner else { continue };
            let spool = r.lbox().spool();
            let n = spool.len().unwrap();
            if n != 0 {
                return Err(self.fail(
                    "inv6-spool",
                    format!(
                        "{}: {n} frames left in the spool: {:?}",
                        b.name,
                        spool.pending()
                    ),
                ));
            }
            let mut st = db
                .prepare("SELECT thread, rseq FROM runner_acks WHERE box = ?1")
                .unwrap();
            let acks: Vec<(String, i64)> = st
                .query_map([b.name], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            for (thread, w) in acks {
                if tainted.contains(&thread) {
                    continue;
                }
                let next = spool.next_rseq(&thread).unwrap();
                if w as u64 + 1 != next {
                    return Err(self.fail(
                        "inv6-watermark",
                        format!(
                            "{}: thread {thread} watermark {w} but the runner's next rseq is {next}",
                            b.name
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    /// inv8: stopped threads stay stopped.
    fn check_stopped(&self) -> Res {
        let checked = self.checked();
        let rec = self.rec.lock();
        for t in &checked {
            let Some(at) = t.stopped_at else { continue };
            let view = self.h().hub().thread_view(&t.id).unwrap();
            if !view.stopped {
                return Err(self.fail(
                    "inv8-not-stopped",
                    format!(
                        "thread {} ({}) was stopped at step {at} (no send since) but its status is {}",
                        t.name,
                        t.id,
                        view.status()
                    ),
                ));
            }
            // A turn dispatched before the stop may still reach its runtime (its spawn was on
            // its way; the stop follows it and interrupts it). No later turn may.
            let ran: Vec<&PromptRx> = rec
                .prompts
                .iter()
                .filter(|p| p.thread == t.id && p.step >= at && p.turn > t.stop_turns)
                .collect();
            if !ran.is_empty() {
                return Err(self.fail(
                    "inv8-ran",
                    format!(
                        "thread {} stopped at step {at} (after turn {}) ran later turns: {ran:?}",
                        t.name, t.stop_turns
                    ),
                ));
            }
            let evs = self.events(&t.id);
            let last_stop = evs.iter().rposition(|e| e.kind() == "thread.stopped");
            let late = evs.iter().enumerate().find(|(i, e)| {
                last_stop.is_some_and(|s| *i > s)
                    && matches!(e.kind(), "thread.started" | "turn.started")
            });
            if let Some((_, e)) = late {
                return Err(self.fail(
                    "inv8-restarted",
                    format!(
                        "thread {} stopped at step {at}: {} (seq {}) after its thread.stopped",
                        t.name,
                        e.kind(),
                        e.seq
                    ),
                ));
            }
        }
        Ok(())
    }

    // --- the run -----------------------------------------------------------------------------

    async fn run(&mut self, steps: u64) -> Res {
        // Both boxes up before anything happens.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(0..self.boxes.len()).all(|i| self.online(i)) {
            if Instant::now() > deadline {
                return Err(self.fail("setup", "the boxes never came online"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        for s in 1..=steps {
            self.step = s;
            self.rec.lock().step = s;
            self.upkeep().await?;
            let a = self.pick_action();
            self.act(a).await?;
            if s % CHECK_EVERY == 0 {
                self.checkpoint(false).await?;
            }
        }
        self.step = steps + 1;
        self.rec.lock().step = self.step;
        self.checkpoint(true).await?;
        Ok(())
    }

    async fn teardown(mut self) {
        for b in &mut self.boxes {
            if let Some(t) = b.impostor.take() {
                t.abort();
                let _ = t.await;
            }
            if let Some(t) = b.pending.take() {
                if let Ok(Some(r)) = t.await {
                    r.crash().await;
                }
            }
            if let Some(r) = b.runner.take() {
                r.crash().await;
            }
        }
        if let Some(h) = self.hub.take() {
            h.crash().await;
        }
    }
}

/// Paths where two JSON values differ (at most 12).
fn json_diff(a: &Value, b: &Value) -> Vec<String> {
    fn walk(path: &str, a: &Value, b: &Value, out: &mut Vec<String>) {
        if out.len() >= 12 || a == b {
            return;
        }
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
                for k in keys {
                    walk(
                        &format!("{path}.{k}"),
                        x.get(k).unwrap_or(&Value::Null),
                        y.get(k).unwrap_or(&Value::Null),
                        out,
                    );
                }
            }
            (Value::Array(x), Value::Array(y)) if x.len() == y.len() => {
                for (i, (p, q)) in x.iter().zip(y).enumerate() {
                    walk(&format!("{path}[{i}]"), p, q, out);
                }
            }
            _ => {
                let s = |v: &Value| {
                    let s = v.to_string();
                    if s.len() > 300 {
                        format!("{}...", &s[..300])
                    } else {
                        s
                    }
                };
                out.push(format!("{path}: {} != {}", s(a), s(b)));
            }
        }
    }
    let mut out = Vec::new();
    walk("$", a, b, &mut out);
    out
}

// ------------------------------------------------------------------------------------------
// Drivers
// ------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
#[allow(dead_code)] // read through Debug in failure reports
struct SeedOutcome {
    failure: Option<Failure>,
    notes: Vec<String>,
    dual: Vec<String>,
    steps: u64,
    threads: usize,
    prompts: usize,
    answers: usize,
    deliveries: usize,
    instances: u64,
    reopens: u64,
    tainted: usize,
    secs: f64,
}

fn run_seed(seed: u64, steps: u64) -> SeedOutcome {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let out = rt.block_on(async move {
        let dir = scratch_dir();
        let started = std::time::Instant::now();
        let mut w = World::new(seed, dir.path().to_path_buf()).await;
        let r = w.run(steps).await;
        let tainted = w.tainted().len();
        let (dual, deliveries, instances) = {
            let rec = w.rec.lock();
            (rec.dual.clone(), rec.deliveries.len(), rec.next_inst)
        };
        let mut out = SeedOutcome {
            failure: r.err(),
            notes: w.notes.clone(),
            dual,
            steps,
            threads: w.threads.len(),
            prompts: w.prompts.len(),
            answers: w.answers.len(),
            deliveries,
            instances,
            reopens: w.reopen_checks,
            tainted,
            secs: 0.0,
        };
        w.teardown().await;
        out.secs = started.elapsed().as_secs_f64();
        if let Some(f) = out.failure.as_mut() {
            // The data (hub database, runner spools) of a failed seed stays for inspection.
            let kept = dir.keep();
            f.detail += &format!("\n  data kept in {}", kept.display());
        }
        out
    });
    rt.shutdown_timeout(Duration::from_secs(10));
    out
}

/// A temporary directory, on tmpfs when there is one (`TWR_FUZZ_TMP` overrides): every
/// hub commit and runner spool append is synced, and a disk makes that the whole run time.
fn scratch_dir() -> tempfile::TempDir {
    let base = std::env::var_os("TWR_FUZZ_TMP")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from("/dev/shm")).filter(|p| p.is_dir()));
    match base {
        Some(b) => tempfile::Builder::new()
            .prefix("twr-fuzz-")
            .tempdir_in(b)
            .unwrap_or_else(|_| tempfile::tempdir().unwrap()),
        None => tempfile::tempdir().unwrap(),
    }
}

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.trim().parse().ok()
}

fn init_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let level = match std::env::var("TWR_FUZZ_LOG").ok().as_deref() {
            Some("debug") => tracing::Level::DEBUG,
            Some("info") => tracing::Level::INFO,
            Some("warn") => tracing::Level::WARN,
            _ => return,
        };
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(level)
                .with_ansi(false)
                .with_writer(std::io::stderr)
                .finish(),
        );
    });
}

/// Runs `seeds` (in parallel, `TWR_FUZZ_JOBS`), stopping new ones after `budget`; prints a
/// summary and panics with every failure.
fn drive(seeds: Vec<u64>, steps: u64, budget: Option<Duration>) {
    init_logs();
    let jobs = env_u64("TWR_FUZZ_JOBS").unwrap_or(2).max(1) as usize;
    let started = std::time::Instant::now();
    let queue = Arc::new(Mutex::new(VecDeque::from(seeds)));
    let results: Arc<Mutex<Vec<(u64, SeedOutcome)>>> = Arc::default();
    let workers: Vec<_> = (0..jobs)
        .map(|_| {
            let queue = Arc::clone(&queue);
            let results = Arc::clone(&results);
            std::thread::spawn(move || loop {
                if budget.is_some_and(|b| started.elapsed() > b) {
                    return;
                }
                let Some(seed) = queue.lock().unwrap().pop_front() else {
                    return;
                };
                let out = run_seed(seed, steps);
                eprintln!(
                    "fuzz seed {seed}: {} in {:.1}s ({} threads ({} tainted by a second runner), {} prompts, {} answers, {} deliveries, {} runtime instances, {} reopen checks)",
                    if out.failure.is_some() { "FAIL" } else { "ok" },
                    out.secs,
                    out.threads,
                    out.tainted,
                    out.prompts,
                    out.answers,
                    out.deliveries,
                    out.instances,
                    out.reopens
                );
                if let Some(f) = &out.failure {
                    eprintln!("{f}");
                }
                results.lock().unwrap().push((seed, out));
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    let mut results = std::mem::take(&mut *results.lock().unwrap());
    results.sort_by_key(|(s, _)| *s);
    let failed: Vec<&(u64, SeedOutcome)> = results
        .iter()
        .filter(|(_, o)| o.failure.is_some())
        .collect();
    eprintln!(
        "fuzz: {} seeds run in {:.0}s, {} failed",
        results.len(),
        started.elapsed().as_secs_f64(),
        failed.len()
    );
    for (seed, o) in &results {
        for n in &o.notes {
            eprintln!("fuzz note (seed {seed}): {n}");
        }
        for d in &o.dual {
            eprintln!(
                "fuzz dual-runner overlap (seed {seed}, expected with two runners of a box): {d}"
            );
        }
    }
    if !failed.is_empty() {
        let mut msg = String::new();
        for (_, o) in &failed {
            msg += &o.failure.as_ref().unwrap().to_string();
        }
        panic!("{} of {} seeds failed:\n{msg}", failed.len(), results.len());
    }
}

fn seeds_from_env(default_n: u64, default_base: u64) -> Vec<u64> {
    if let Ok(s) = std::env::var("TWR_FUZZ_SEED") {
        return s.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    }
    let n = env_u64("TWR_FUZZ_SEEDS").unwrap_or(default_n);
    let base = env_u64("TWR_FUZZ_BASE").unwrap_or(default_base);
    (base..base + n).collect()
}

#[test]
fn runner_reconnect_fuzz() {
    let seeds = seeds_from_env(20, 1);
    let steps = env_u64("TWR_FUZZ_ITERS").unwrap_or(40);
    drive(seeds, steps, None);
}

#[test]
#[ignore = "long run: 500+ seeds, ~30 minutes"]
fn runner_reconnect_fuzz_long() {
    let seeds = seeds_from_env(500, 1000);
    let steps = env_u64("TWR_FUZZ_ITERS").unwrap_or(60);
    let minutes = env_u64("TWR_FUZZ_MINUTES").unwrap_or(30);
    drive(seeds, steps, Some(Duration::from_secs(minutes * 60)));
}

#[test]
fn the_prng_is_deterministic() {
    let a: Vec<u64> = {
        let mut r = Rng::new(42);
        (0..8).map(|_| r.next()).collect()
    };
    let b: Vec<u64> = {
        let mut r = Rng::new(42);
        (0..8).map(|_| r.next()).collect()
    };
    assert_eq!(a, b);
    let mut r = Rng::new(7);
    for _ in 0..1000 {
        let x = r.range(3, 9);
        assert!((3..=9).contains(&x));
    }
}

// ------------------------------------------------------------------------------------------
// Focused reproductions of what the fuzzer found
// ------------------------------------------------------------------------------------------

type RawWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn raw_send(ws: &mut RawWs, m: &RunnerMsg) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into()))
        .await
        .unwrap();
}

/// The next hub message that is not a ping / pong (`None`: closed).
async fn raw_expect(ws: &mut RawWs) -> Option<HubMsg> {
    loop {
        let m = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("no message from the hub");
        match m {
            Some(Ok(Message::Text(t))) => match serde_json::from_str(t.as_str()).unwrap() {
                HubMsg::Ping | HubMsg::Pong => continue,
                // The hub reconciles a box's trash once it is online: an empty trash.
                HubMsg::Cmd {
                    cmd_id,
                    op: twr_core::runner::Op::TrashList {},
                } => {
                    raw_send(
                        ws,
                        &RunnerMsg::CmdResult(twr_core::runner::CmdResult::new(
                            cmd_id,
                            Ok(Value::Array(Vec::new())),
                        )),
                    )
                    .await
                }
                other => return Some(other),
            },
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

/// The next command (acks skipped).
async fn raw_cmd(ws: &mut RawWs) -> (String, twr_core::runner::Op) {
    loop {
        match raw_expect(ws).await {
            Some(HubMsg::Cmd { cmd_id, op }) => return (cmd_id, op),
            Some(_) => continue,
            None => panic!("closed"),
        }
    }
}

async fn raw_ok(ws: &mut RawWs, cmd_id: String) {
    raw_send(
        ws,
        &RunnerMsg::CmdResult(twr_core::runner::CmdResult::new(cmd_id, Ok(Value::Null))),
    )
    .await;
}

/// A raw runner's handshake as the first fuzzed box, up to its inventory (with `acks_needed`):
/// hello, the hub's challenge (its proof checked), auth, authenticated, inventory. The hub's
/// next message is the welcome.
async fn raw_hello(ws: &mut RawWs, acks_needed: BTreeMap<String, u64>) {
    use twr_core::runner::{auth_proof, check_auth_proof, new_nonce, Prover};
    let (box_name, token) = (BOXES[0], Secret::new(token(BOXES[0])));
    let nonce = new_nonce().unwrap();
    let hello = twr_core::runner::Hello {
        box_name: box_name.into(),
        protocol: twr_core::runner::PROTOCOL_VERSION,
        nonce: nonce.clone(),
    };
    raw_send(ws, &RunnerMsg::Hello(hello)).await;
    let Some(HubMsg::Challenge { nonce: hub, proof }) = raw_expect(ws).await else {
        panic!("no challenge")
    };
    assert!(check_auth_proof(
        &token,
        Prover::Hub,
        box_name,
        &nonce,
        &hub,
        &proof
    ));
    let proof = auth_proof(&token, Prover::Runner, box_name, &nonce, &hub);
    raw_send(ws, &RunnerMsg::Auth { proof }).await;
    assert!(matches!(raw_expect(ws).await, Some(HubMsg::Authenticated)));
    let inv = twr_core::runner::Inventory {
        version: "0.0.0-raw".into(),
        runtimes: vec![RUNTIME.into()],
        worktrees: "/nonexistent/worktrees".into(),
        acks_needed,
        ..Default::default()
    };
    raw_send(ws, &RunnerMsg::Inventory(inv)).await;
}

/// Seed 1025's failure (inv5): the exit of a stopped runtime is being recorded (its closing
/// sequence retried: storage refuses it for a while) when the runner reconnects, its spool
/// still holding that exit. The hub takes the thread's "session" over again at the inventory
/// (`adopt_remote_sessions` sees no live runtime, since the exiting one was already taken
/// out by `begin_exit`, and the log still shows the turn open), the fenced connection's
/// closing sequence goes in, the queue is dispatched (turn 2), and the replayed exit is taken
/// as the adopted session's: a second closing sequence (the same turn ended again, the same
/// item resolved again, a second `thread.stopped`) that also closes turn 2 without any
/// `turn.ended`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_being_recorded_is_not_recorded_again_after_a_reconnect() {
    let dir = scratch_dir();
    let h = TestHub::start(dir.path()).await.unwrap();
    let online = |h: &TestHub| {
        h.hub()
            .boxes()
            .iter()
            .any(|(n, on, _)| n == BOXES[0] && *on)
    };
    let mut ws1 = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap()
        .0;
    raw_hello(&mut ws1, BTreeMap::new()).await;
    assert!(matches!(
        raw_expect(&mut ws1).await,
        Some(HubMsg::Welcome { .. })
    ));
    raw_send(&mut ws1, &RunnerMsg::Synced).await;
    while !online(&h) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let proj = dir.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    let req = NewThread {
        project: proj.to_string_lossy().into_owned(),
        runtime: Some(RUNTIME.into()),
        name: Some("t".into()),
        box_name: Some(BOXES[0].into()),
        ..Default::default()
    };
    // The runner is asked where the project's repository is (nowhere: not a git repo).
    let hub = h.hub().clone();
    let created =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.new_thread(req, CLIENT).await });
    let (cmd, op) = raw_cmd(&mut ws1).await;
    assert_eq!(op.name(), "repo_root");
    raw_ok(&mut ws1, cmd).await;
    let t = created.await.unwrap().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let send = |h: &TestHub, text: &str| {
        h.sync(|hub| {
            hub.send(&t, text, TurnFrom::User, Source::User(CLIENT.into()))
                .unwrap()
        })
    };
    send(&h, "#P1\nask");
    let (cmd, op) = raw_cmd(&mut ws1).await;
    // The hub names the run.
    let twr_core::runner::Op::Spawn { run: Some(r1), .. } = op else {
        panic!("{op:?}")
    };
    raw_ok(&mut ws1, cmd).await;
    let frame = |rseq: u64, turn: Option<u64>, body: EventBody| RunnerMsg::Event {
        thread: t.clone(),
        rseq,
        turn,
        body,
        raw: None,
        run: Some(r1.clone()),
        at_ms: None,
    };
    raw_send(
        &mut ws1,
        &frame(
            1,
            None,
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s1".into()),
                ..Default::default()
            }),
        ),
    )
    .await;
    // The user stops it; a prompt waits for the stop.
    let stopped = h.sync(|hub| hub.stop_thread(&t, CLIENT)).unwrap();
    assert_eq!(stopped["stopping"], json!(true));
    let (cmd, op) = raw_cmd(&mut ws1).await;
    assert_eq!(op.name(), "stop");
    raw_ok(&mut ws1, cmd).await;
    send(&h, "#P2\nsay 1");
    // Storage refuses turn.ended while the exit comes in: its closing sequence is retried.
    let db = open_db(dir.path());
    db.execute_batch(
        "CREATE TRIGGER f1 BEFORE INSERT ON events WHEN NEW.kind = 'turn.ended' \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let exited = RunnerMsg::Exited {
        thread: t.clone(),
        rseq: 2,
        code: Some(0),
        stderr_tail: String::new(),
        run: Some(r1.clone()),
        at_ms: None,
    };
    raw_send(&mut ws1, &exited).await;
    let hub = h.hub().clone();
    let deadline = Instant::now() + Duration::from_secs(10);
    while hub.info()["persist_retries"].as_u64().unwrap_or(0) == 0 {
        assert!(Instant::now() < deadline, "no retry");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The runner reconnects meanwhile, its spool holding the exit (running nothing).
    let mut ws2 = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap()
        .0;
    raw_hello(&mut ws2, BTreeMap::from([(t.clone(), 2)])).await;
    assert!(matches!(
        raw_expect(&mut ws2).await,
        Some(HubMsg::Welcome { .. })
    ));
    raw_send(&mut ws2, &exited).await;
    raw_send(&mut ws2, &RunnerMsg::Synced).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    db.execute_batch("DROP TRIGGER f1").unwrap();
    // Answer what comes (the prompt's session), long enough for everything to go in.
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut ops = Vec::new();
    while Instant::now() < deadline {
        if let Ok(Some(HubMsg::Cmd { cmd_id, op })) =
            tokio::time::timeout(Duration::from_millis(200), raw_expect(&mut ws2)).await
        {
            ops.push(op.name().to_string());
            raw_ok(&mut ws2, cmd_id).await;
        }
    }
    let evs = h.hub().thread_events(&t, 0, 10_000).unwrap().1;
    let kinds: Vec<String> = evs
        .iter()
        .map(|e| {
            format!(
                "{}{}",
                e.kind(),
                e.turn.map(|t| format!("#{t}")).unwrap_or_default()
            )
        })
        .collect();
    let n = |k: &str| kinds.iter().filter(|x| x.as_str() == k).count();
    assert_eq!(n("thread.stopped"), 1, "{kinds:?}");
    assert_eq!(n("turn.ended#1"), 1, "{kinds:?}");
    // Turn 2 (the queued prompt) is either still open or ended by a turn.ended of its own.
    let view = h.hub().thread_view(&t).unwrap();
    assert!(
        view.current_turn == Some(2) || n("turn.ended#2") == 1,
        "turn 2 closed without a turn.ended: {kinds:?}"
    );
    // The hub took no session over from the reconnection (the runner runs none, and the one
    // the log showed was exiting): nothing is asked to stop again, and the queued prompt starts
    // a new session.
    assert!(
        matches!(ops.first().map(String::as_str), Some("spawn" | "resume"))
            && !ops.iter().any(|o| o == "stop"),
        "{ops:?} {kinds:?}"
    );
    h.crash().await;
}

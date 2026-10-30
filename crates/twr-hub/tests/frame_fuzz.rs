//! Invariant fuzz for the runner protocol's frame decoding (`twr_core::runner`, protocol 7):
//! what the hub and a runner do with each WebSocket message the other side (or anyone who
//! reaches the hub's listener) sends.
//!
//! Two layers, as the hub reads them:
//!
//! - The WebSocket layer, configured as the hub configures it ([`pre_auth_ws_config`] until a
//!   connection has authenticated, [`runner_ws_config`] after): byte streams of masked client
//!   frames (text, binary, ping, continuations, close) with lengths around the limits, length
//!   headers that promise more than they bring (up to 2^63), fragmented messages that grow
//!   past the limit, and random or damaged bytes, read until the stream ends or fails.
//! - The message layer: every text message is decoded as [`RunnerMsg`] (the hub's reader) and
//!   [`HubMsg`] (the runner's). Messages come from a generator of valid messages of every
//!   kind (hello, auth, inventory, event with several bodies, exited, synced, cmd_result,
//!   challenge, authenticated, welcome, ack, cmd with several ops, error, ping, pong), then
//!   mutated (a JSON value replaced with a random one, a key dropped, added or renamed, the
//!   type changed, bytes flipped, inserted or dropped, the text cut short), or replaced with
//!   random JSON, deep nesting, huge numbers, long strings and escapes.
//!
//! Invariants:
//!
//! - (a) nothing panics (the decoders, tungstenite under the hub's limits, the handshake's
//!   checks);
//! - (b) allocation is bounded: decoding a message allocates at most [`DECODE_FACTOR`] times
//!   its length (plus a little), and a message the hub decodes before authentication (at most
//!   [`HELLO_MAX_BYTES`]) at most [`PRE_AUTH_DECODE_PEAK`]; the WebSocket layer never yields a
//!   message past its limit, and refuses a frame whose header promises more than the limit
//!   before reading (or allocating for) its payload. Measured with a counting allocator, per
//!   thread;
//! - (c) valid messages round-trip: a generated message decodes back to itself exactly, and
//!   any text that decodes re-encodes to text that decodes to the same message (floats
//!   compared to the last bits: serde_json parses them best-effort);
//! - (d) the pre-authentication handshake's checks: [`valid_nonce`] holds exactly for 64
//!   lowercase hex digits, and [`check_auth_proof`] accepts a proof only when it is the proof
//!   of the same token, side, box and nonces (as bytes: hex case aside), never with an empty
//!   token.
//!
//! `TWR_FUZZ_SEED` (default: fixed) and `TWR_FUZZ_CASES` (default 4000; the ignored long run
//! does 2M) set the run; `TWR_FUZZ_CASE=n` runs case n alone. A failure prints the seed,
//! the case and the input. The long run:
//! `cargo test -p twr-hub --release --test frame_fuzz -- --ignored`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::panic::{catch_unwind, AssertUnwindSafe};

use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::error::{CapacityError, Error as WsError};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocket, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use twr_core::event::{
    AgentText, Answer, AttentionRaised, AttentionReason, EventBody, JsonMap, SessionPayload,
    TurnEnded,
};
use twr_core::runner::{
    auth_proof, check_auth_proof, cmd_errors, hub_errors, valid_nonce, CmdError, CmdResult, Hello,
    HubMsg, Inventory, Op, Prover, RunnerMsg, RunningSession, Secret, WireSpawnConfig,
    MAX_FRAME_BYTES, NONCE_HEX_LEN, PROTOCOL_VERSION,
};
use twr_hub::listener::{pre_auth_ws_config, runner_ws_config, HELLO_MAX_BYTES};

const DEFAULT_SEED: u64 = 0x6672_616d_655f_667a;
const DEFAULT_CASES: u64 = 4_000;
const LONG_CASES: u64 = 2_000_000;

/// Decoding a message allocates at most this many times its length (plus [`DECODE_SLACK`]).
/// The most seen is about 51 (an array of small numbers: serde buffers a tagged message
/// before it reads it, and each element takes tens of bytes).
const DECODE_FACTOR: usize = 128;
const DECODE_SLACK: usize = 64 << 10;
/// Decoding a message of at most [`HELLO_MAX_BYTES`] (all an unauthenticated peer can send)
/// allocates at most this much.
const PRE_AUTH_DECODE_PEAK: usize = 1 << 20;
/// Reading a connection before authentication (its frames at most [`HELLO_MAX_BYTES`])
/// allocates at most this much in the WebSocket layer (its read buffer included).
const PRE_AUTH_READ_PEAK: usize = 1 << 20;

// ---------------------------------------------------------------------------------------------
// Counting allocator (per thread)
// ---------------------------------------------------------------------------------------------

struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = LIVE.try_with(|l| {
        let v = l.get() + delta;
        l.set(v);
        let _ = PEAK.try_with(|p| {
            if v > p.get() {
                p.set(v);
            }
        });
    });
}

// SAFETY: every call is passed on to the system allocator unchanged; only sizes are counted.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            note(l.size() as isize);
        }
        p
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(l) };
        if !p.is_null() {
            note(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        note(-(l.size() as isize));
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new) };
        if !q.is_null() {
            note(new as isize - l.size() as isize);
        }
        q
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// `f`'s result and the most it had allocated (on this thread) at any point.
fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = LIVE.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let r = f();
    let peak = PEAK.with(Cell::get) - base;
    (r, peak.max(0) as usize)
}

// ---------------------------------------------------------------------------------------------
// Random numbers
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    /// The generator of case `case` of a run seeded `seed`: each case replays on its own.
    fn for_case(seed: u64, case: u64) -> Rng {
        let mut r = Rng((seed ^ case.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
        for _ in 0..4 {
            r.next();
        }
        r
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

// ---------------------------------------------------------------------------------------------
// Valid messages
// ---------------------------------------------------------------------------------------------

const ODD: &[&str] = &[
    "\"", "\\", "\n", "\u{0}", "\u{7f}", "\u{202e}", "é", "中", "🙂", "\u{feff}", "{", "}", "[",
    "]", ":", ",", "type", "hello", "event",
];

fn string(rng: &mut Rng) -> String {
    let n = match rng.below(20) {
        0 => 0,
        1 => 1_000 + rng.below(6_000),
        _ => 1 + rng.below(24),
    };
    let mut s = String::new();
    while s.len() < n {
        if rng.chance(85) {
            s.push((b'!' + rng.below(94) as u8) as char);
        } else {
            s.push_str(rng.pick(ODD));
        }
    }
    s
}

fn number(rng: &mut Rng) -> u64 {
    match rng.below(5) {
        0 => 0,
        1 => u64::MAX - rng.below(3) as u64,
        2 => rng.next(),
        _ => rng.below(1000) as u64,
    }
}

/// A random JSON value: integers, floats exactly representable in a few digits, strings,
/// arrays and objects.
fn value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth > 3 { 5 } else { 8 }) {
        0 => Value::Null,
        1 => json!(rng.chance(50)),
        2 => json!(rng.next() as i64 >> rng.below(64)),
        3 => json!((rng.below(4000) as f64 - 2000.0) / 4.0),
        4 => json!(string(rng)),
        5 | 6 => Value::Array((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
        _ => Value::Object(
            (0..rng.below(4))
                .map(|_| (string(rng), value(rng, depth + 1)))
                .collect(),
        ),
    }
}

fn opt<T>(rng: &mut Rng, f: impl FnOnce(&mut Rng) -> T) -> Option<T> {
    rng.chance(50).then(|| f(rng))
}

fn ext(rng: &mut Rng) -> Option<JsonMap> {
    opt(rng, |rng| {
        let mut m = JsonMap::new();
        m.insert(string(rng), value(rng, 1));
        m
    })
}

fn body(rng: &mut Rng) -> EventBody {
    match rng.below(5) {
        0 => EventBody::AgentText(AgentText {
            text: string(rng),
            ..Default::default()
        }),
        1 => EventBody::ThreadStarted(SessionPayload {
            session_id: opt(rng, string),
            ext: ext(rng),
            ..Default::default()
        }),
        2 => EventBody::AttentionRaised(AttentionRaised {
            attn_id: string(rng),
            reason: AttentionReason::Permission,
            summary: string(rng),
            options: vec!["allow".into(), "deny".into()],
            call_id: opt(rng, string),
            ext: ext(rng),
            ..Default::default()
        }),
        3 => EventBody::TurnEnded(TurnEnded {
            session_id: opt(rng, string),
            ext: ext(rng),
            ..Default::default()
        }),
        _ => EventBody::Unknown {
            kind: format!("future.{}", rng.below(100)),
            data: value(rng, 0),
        },
    }
}

fn spawn_config(rng: &mut Rng) -> WireSpawnConfig {
    WireSpawnConfig {
        thread: string(rng),
        cwd: string(rng).into(),
        mode: string(rng),
        model: opt(rng, string),
        effort: opt(rng, string),
        // `null` here is no config on the wire (`skip_serializing_if` aside, `null` decodes as
        // none): the hub never sends it.
        mcp_config: opt(rng, |r| json!({ string(r): value(r, 1) })),
        allowed_tools: vec![string(rng)],
        thinking: rng.chance(50),
        env: vec![(string(rng), string(rng))],
        prompt_turn: opt(rng, number),
        ..Default::default()
    }
}

fn op(rng: &mut Rng) -> Op {
    match rng.below(8) {
        0 => Op::Spawn {
            thread: string(rng),
            runtime: string(rng),
            cfg: spawn_config(rng),
            prompt: opt(rng, string),
            run: opt(rng, string),
        },
        1 => Op::Send {
            thread: string(rng),
            turn: number(rng),
            text: string(rng),
            run: opt(rng, string),
        },
        2 => Op::Respond {
            thread: string(rng),
            attn_id: string(rng),
            answer: Answer {
                decision: string(rng),
                message: opt(rng, string),
                ..Default::default()
            },
            run: opt(rng, string),
        },
        3 => Op::Stop {
            thread: string(rng),
            run: opt(rng, string),
        },
        4 => Op::RepoRoot {
            dir: string(rng).into(),
        },
        5 => Op::WorktreeExists {
            path: string(rng).into(),
        },
        6 => Op::TrashList {},
        _ => Op::DiskUsage {
            worktree: opt(rng, |r| string(r).into()),
            disposable: opt(rng, |r| vec![string(r)]),
        },
    }
}

fn nonce(rng: &mut Rng) -> String {
    (0..NONCE_HEX_LEN)
        .map(|_| *rng.pick(b"0123456789abcdef") as char)
        .collect()
}

fn runner_msg(rng: &mut Rng) -> RunnerMsg {
    match rng.below(10) {
        0 => RunnerMsg::Hello(Hello {
            box_name: string(rng),
            protocol: if rng.chance(80) {
                PROTOCOL_VERSION
            } else {
                rng.next() as u32
            },
            nonce: nonce(rng),
        }),
        1 => RunnerMsg::Auth { proof: nonce(rng) },
        2 => RunnerMsg::Inventory(Inventory {
            version: string(rng),
            epoch_hint: opt(rng, number),
            running: vec![RunningSession {
                thread: string(rng),
                runtime: string(rng),
                session_id: opt(rng, string),
                pid: opt(rng, |r| r.next() as u32),
                run: opt(rng, string),
            }],
            in_flight: vec![string(rng)],
            acks_needed: BTreeMap::from([(string(rng), number(rng))]),
            runtimes: vec![string(rng)],
            worktrees: string(rng).into(),
            build: None,
            agents: vec![twr_core::runner::AgentInfo {
                runtime: string(rng),
                found: true,
                path: opt(rng, string),
                version: opt(rng, string),
                login: twr_core::runner::AgentLogin::LoggedIn,
                method: opt(rng, string),
                note: opt(rng, string),
            }],
            launch: opt(rng, string),
        }),
        3 | 4 => RunnerMsg::Event {
            thread: string(rng),
            rseq: number(rng),
            turn: opt(rng, number),
            body: body(rng),
            raw: opt(rng, string),
            run: opt(rng, string),
            at_ms: opt(rng, number),
        },
        5 => RunnerMsg::Exited {
            thread: string(rng),
            rseq: number(rng),
            code: opt(rng, |r| r.next() as i32),
            stderr_tail: string(rng),
            run: opt(rng, string),
            at_ms: opt(rng, number),
        },
        6 => RunnerMsg::Synced,
        7 => RunnerMsg::CmdResult(CmdResult::new(
            string(rng),
            if rng.chance(50) {
                Ok(value(rng, 0))
            } else {
                Err(CmdError::new(cmd_errors::NOT_RUNNING, string(rng)))
            },
        )),
        8 => RunnerMsg::Ping,
        _ => RunnerMsg::Pong,
    }
}

fn hub_msg(rng: &mut Rng) -> HubMsg {
    match rng.below(9) {
        0 => HubMsg::Challenge {
            nonce: nonce(rng),
            proof: nonce(rng),
        },
        1 => HubMsg::Authenticated,
        2 => HubMsg::Welcome {
            epoch: number(rng),
            watermarks: BTreeMap::from([(string(rng), number(rng))]),
        },
        3 => HubMsg::Ack {
            thread: string(rng),
            rseq: number(rng),
        },
        4 | 5 => HubMsg::Cmd {
            cmd_id: string(rng),
            op: op(rng),
        },
        6 => HubMsg::Error {
            code: hub_errors::PROTOCOL.into(),
            message: string(rng),
        },
        7 => HubMsg::Ping,
        _ => HubMsg::Pong,
    }
}

// ---------------------------------------------------------------------------------------------
// Mutations and junk
// ---------------------------------------------------------------------------------------------

fn pointers(v: &Value, at: String, out: &mut Vec<String>) {
    if out.len() > 300 {
        return;
    }
    out.push(at.clone());
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let k = k.replace('~', "~0").replace('/', "~1");
                pointers(x, format!("{at}/{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                pointers(x, format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

const TAGS: &[&str] = &[
    "hello",
    "auth",
    "inventory",
    "event",
    "exited",
    "synced",
    "cmd_result",
    "ping",
    "pong",
    "challenge",
    "authenticated",
    "welcome",
    "ack",
    "cmd",
    "error",
    "spawn",
    "send",
    "agent.text",
    "HELLO",
    "",
];

fn mutate_value(rng: &mut Rng, v: &mut Value) {
    let mut ps = Vec::new();
    pointers(v, String::new(), &mut ps);
    let p = rng.pick(&ps).clone();
    let Some(node) = v.pointer_mut(&p) else {
        return;
    };
    match rng.below(8) {
        0 | 1 => *node = value(rng, 0),
        2 => {
            if let Value::Object(m) = node {
                if let Some(k) = m.keys().nth(rng.below(m.len())).cloned() {
                    m.remove(&k);
                }
            }
        }
        3 => {
            if let Value::Object(m) = node {
                let k = if rng.chance(50) {
                    rng.pick(&["type", "kind", "data", "op", "proof", "nonce", "box"])
                        .to_string()
                } else {
                    string(rng)
                };
                m.insert(k, value(rng, 1));
            }
        }
        4 => *node = json!(rng.pick(TAGS)),
        5 => {
            *node = match node.take() {
                Value::String(s) => json!([s]),
                Value::Number(n) => json!(n.to_string()),
                Value::Bool(b) => json!(b as u8),
                Value::Null => json!({}),
                other => json!(other.to_string()),
            }
        }
        6 => {
            if let Value::Number(_) = node {
                *node = match rng.below(4) {
                    0 => json!(-1),
                    1 => json!(u64::MAX),
                    2 => json!(1.5),
                    _ => json!(i64::MIN),
                };
            }
        }
        _ => *node = json!(string(rng)),
    }
}

fn mutate_bytes(rng: &mut Rng, text: &str) -> String {
    let mut b = text.as_bytes().to_vec();
    for _ in 0..1 + rng.below(4) {
        let i = rng.below(b.len() + 1);
        match rng.below(6) {
            0 if i < b.len() => b[i] ^= 1 << rng.below(8),
            1 => b.insert(i, rng.next() as u8),
            2 if i < b.len() => {
                b.remove(i);
            }
            3 => b.truncate(i),
            4 => b.insert(i, *rng.pick(b"{}[]\",:\\0e-.")),
            _ => {
                // A slice repeated.
                let lo = rng.below(b.len() + 1);
                let hi = (lo + rng.below(256)).min(b.len());
                let dup = b[lo..hi].to_vec();
                let at = i.min(b.len());
                b.splice(at..at, dup);
            }
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// Text built to strain a decoder: deep nesting, huge numbers, long strings and escapes,
/// many keys, number arrays.
fn junk(rng: &mut Rng, limit: usize) -> String {
    let n = 1 + rng.below(limit.max(2));
    match rng.below(8) {
        0 => format!(
            "{}{}",
            "[".repeat(n.min(5000)),
            "]".repeat(rng.below(n.min(5000) + 1))
        ),
        1 => format!(
            r#"{{"type":"event","thread":"t","rseq":1,"body":{{"kind":"x","data":{}{}}}}}"#,
            "[".repeat(n.min(3000)),
            "]".repeat(n.min(3000))
        ),
        2 => format!(
            r#"{{"type":"hello","box":[{}0],"protocol":6}}"#,
            "0,".repeat(n / 2)
        ),
        3 => format!(r#"{{"type":"auth","proof":"{}"}}"#, "\\u0000".repeat(n / 6)),
        4 => format!(
            r#"{{"type":"ack","thread":"t","rseq":{}}}"#,
            "9".repeat(n.min(400))
        ),
        5 => {
            let keys: Vec<String> = (0..n / 8).map(|i| format!("\"k{i}\":{i}")).collect();
            format!(
                r#"{{"type":"cmd_result","cmd_id":"c","ok":true,"result":{{{}}}}}"#,
                keys.join(",")
            )
        }
        6 => format!(
            r#"{{"type":"event","thread":"t","rseq":1,"body":{{"kind":"agent.text","data":{{"text":"{}"}}}}}}"#,
            "\\ud83d\\ude42".repeat(n / 12)
        ),
        _ => (0..n.min(4096)).map(|_| rng.next() as u8 as char).collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// The message layer
// ---------------------------------------------------------------------------------------------

/// Whether two JSON values are equal, floats to their last few bits.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            let (x, y) = (
                x.as_f64().unwrap_or(f64::NAN),
                y.as_f64().unwrap_or(f64::NAN),
            );
            x == y || (x - y).abs() <= x.abs().max(y.abs()) * 1e-15
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((ka, va), (kb, vb))| ka == kb && same(va, vb))
        }
        _ => a == b,
    }
}

#[derive(Debug, Default)]
struct Stats {
    texts: u64,
    runner_decoded: u64,
    hub_decoded: u64,
    generated: u64,
    pre_auth: u64,
    max_ratio: f64,
    max_pre_auth_peak: usize,
    streams: u64,
    ws_messages: u64,
    ws_refused: u64,
    max_ws_peak: usize,
    proofs: u64,
}

/// Decodes `text` as both sides do: no panic, bounded allocation, and what decodes
/// round-trips.
fn check_text(st: &mut Stats, text: &str) -> Result<(), String> {
    st.texts += 1;
    let (runner, rp) = peak_of(|| serde_json::from_str::<RunnerMsg>(text));
    let (hub, hp) = peak_of(|| serde_json::from_str::<HubMsg>(text));
    let peak = rp.max(hp);
    if peak > DECODE_FACTOR * text.len() + DECODE_SLACK {
        return Err(format!(
            "decoding {} bytes allocated {peak} bytes",
            text.len()
        ));
    }
    if text.len() > 64 {
        st.max_ratio = st.max_ratio.max(peak as f64 / text.len() as f64);
    }
    if text.len() <= HELLO_MAX_BYTES {
        st.pre_auth += 1;
        st.max_pre_auth_peak = st.max_pre_auth_peak.max(rp);
        if rp > PRE_AUTH_DECODE_PEAK {
            return Err(format!(
                "decoding a pre-authentication message of {} bytes allocated {rp} bytes",
                text.len()
            ));
        }
    }
    if let Ok(m) = runner {
        st.runner_decoded += 1;
        round_trip(&m, |s| serde_json::from_str::<RunnerMsg>(s))?;
        if let RunnerMsg::Hello(h) = &m {
            hello_checks(h)?;
        }
    }
    if let Ok(m) = hub {
        st.hub_decoded += 1;
        round_trip(&m, |s| serde_json::from_str::<HubMsg>(s))?;
    }
    Ok(())
}

fn round_trip<T: serde::Serialize + std::fmt::Debug>(
    m: &T,
    decode: impl Fn(&str) -> serde_json::Result<T>,
) -> Result<(), String> {
    let s = serde_json::to_string(m).map_err(|e| format!("{m:?} does not encode: {e}"))?;
    let back = decode(&s).map_err(|e| format!("{s} does not decode: {e}"))?;
    let (a, b) = (
        serde_json::to_value(m).map_err(|e| e.to_string())?,
        serde_json::to_value(&back).map_err(|e| e.to_string())?,
    );
    if !same(&a, &b) {
        return Err(format!("{s} decodes to {back:?}"));
    }
    Ok(())
}

/// What the hub checks of a hello before it answers it, on the decoded values.
fn hello_checks(h: &Hello) -> Result<(), String> {
    let shaped = h.nonce.len() == NONCE_HEX_LEN
        && h.nonce
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    if valid_nonce(&h.nonce) != shaped {
        return Err(format!("valid_nonce({:?}) is {}", h.nonce, !shaped));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The WebSocket layer
// ---------------------------------------------------------------------------------------------

/// An in-memory connection: reads `input`, keeps what is written.
struct Mem {
    input: Vec<u8>,
    at: usize,
    written: usize,
}

impl Read for Mem {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.input.len() - self.at);
        buf[..n].copy_from_slice(&self.input[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

impl Write for Mem {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written += buf.len();
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A masked client frame; `len` is the length its header claims (normally `payload.len()`).
fn frame(out: &mut Vec<u8>, fin: bool, opcode: u8, payload: &[u8], len: u64, key: [u8; 4]) {
    out.push(if fin { 0x80 } else { 0 } | opcode);
    match len {
        0..=125 => out.push(0x80 | len as u8),
        126..=0xffff => {
            out.push(0x80 | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(0x80 | 127);
            out.extend_from_slice(&len.to_be_bytes());
        }
    }
    out.extend_from_slice(&key);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
}

/// A stream of client frames around `limit`, some of them lying or damaged.
fn stream(rng: &mut Rng, limit: usize, texts: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..1 + rng.below(4) {
        let key = (rng.next() as u32).to_be_bytes();
        let text = rng.pick(texts).clone();
        match rng.below(9) {
            0 | 1 => frame(&mut out, true, 1, text.as_bytes(), text.len() as u64, key),
            2 => {
                // Exactly at the limit, or one past it.
                let n = limit + rng.below(2);
                let p = vec![b'x'; n];
                frame(&mut out, true, 1, &p, n as u64, key);
            }
            3 => {
                // A header that promises more than the limit (up to 2^63), without it.
                let len = match rng.below(3) {
                    0 => limit as u64 + 1,
                    1 => u64::MAX >> 1,
                    _ => 1 << 32,
                };
                frame(
                    &mut out,
                    true,
                    rng.pick(&[1, 2, 0]).to_owned(),
                    b"",
                    len,
                    key,
                );
                out.extend((0..rng.below(64)).map(|_| rng.next() as u8));
            }
            4 => {
                // A message in fragments that together pass the limit.
                let piece = 1 + rng.below(limit / 2 + 1);
                let n = limit / piece + 2;
                for i in 0..n {
                    let p = vec![b'y'; piece];
                    let op = if i == 0 { 1 } else { 0 };
                    frame(&mut out, i == n - 1, op, &p, piece as u64, key);
                }
            }
            5 => frame(&mut out, true, 9, b"ping", 4, key),
            6 => frame(&mut out, true, 2, text.as_bytes(), text.len() as u64, key),
            7 => {
                let mut f = Vec::new();
                frame(&mut f, true, 1, text.as_bytes(), text.len() as u64, key);
                let b = mutate_bytes(rng, &String::from_utf8_lossy(&f));
                out.extend_from_slice(b.as_bytes());
            }
            _ => out.extend((0..rng.below(300)).map(|_| rng.next() as u8)),
        }
    }
    out
}

/// Reads `input` as the hub does with `config` (limit `limit`): every text message checked
/// at the message layer. Ok(peak allocation of the WebSocket layer).
fn read_stream(
    st: &mut Stats,
    input: Vec<u8>,
    config: WebSocketConfig,
    limit: usize,
) -> Result<usize, String> {
    st.streams += 1;
    let mut messages = Vec::new();
    let (r, peak) = peak_of(|| {
        let mut ws = WebSocket::from_raw_socket(
            Mem {
                input,
                at: 0,
                written: 0,
            },
            Role::Server,
            Some(config),
        );
        loop {
            match ws.read() {
                Ok(Message::Text(t)) => messages.push(t.as_str().to_string()),
                Ok(Message::Binary(b)) => {
                    if b.len() > limit {
                        return Err(format!("a binary message of {} bytes", b.len()));
                    }
                }
                Ok(_) => {}
                Err(WsError::Capacity(CapacityError::MessageTooLong { .. })) => {
                    return Ok(true);
                }
                Err(_) => return Ok(false),
            }
        }
    });
    if r? {
        st.ws_refused += 1;
    }
    for m in &messages {
        st.ws_messages += 1;
        if m.len() > limit {
            return Err(format!("a text message of {} bytes past {limit}", m.len()));
        }
        check_text(st, m)?;
    }
    Ok(peak)
}

// ---------------------------------------------------------------------------------------------
// The handshake's proofs
// ---------------------------------------------------------------------------------------------

fn check_proofs(rng: &mut Rng, st: &mut Stats) -> Result<(), String> {
    st.proofs += 1;
    let token = Secret::new(string(rng));
    let other = Secret::new(format!("{}x", token.expose()));
    let by = *rng.pick(&[Prover::Hub, Prover::Runner]);
    let box_name = string(rng);
    let (rn, hn) = (nonce(rng), nonce(rng));
    let proof = auth_proof(&token, by, &box_name, &rn, &hn);
    let check = |t: &Secret, by: Prover, b: &str, r: &str, h: &str, p: &str| -> bool {
        check_auth_proof(t, by, b, r, h, p)
    };
    if token.is_empty() {
        if check(&token, by, &box_name, &rn, &hn, &proof) {
            return Err("an empty token proved something".into());
        }
        return Ok(());
    }
    if !check(&token, by, &box_name, &rn, &hn, &proof) {
        return Err("a good proof refused".into());
    }
    if !check(&token, by, &box_name, &rn, &hn, &proof.to_uppercase()) {
        return Err("the same proof in upper case refused".into());
    }
    let flip = if by == Prover::Hub {
        Prover::Runner
    } else {
        Prover::Hub
    };
    let wrong = [
        check(&other, by, &box_name, &rn, &hn, &proof),
        check(&token, flip, &box_name, &rn, &hn, &proof),
        check(&token, by, &format!("{box_name}x"), &rn, &hn, &proof),
        check(&token, by, &box_name, &hn, &rn, &proof),
        // The box name and the nonce cut elsewhere: lengths are signed too.
        check(
            &token,
            by,
            &format!("{box_name}{}", &rn[..1]),
            &rn[1..],
            &hn,
            &proof,
        ),
    ];
    if wrong.iter().any(|&w| w) {
        return Err(format!("a proof accepted for other inputs: {wrong:?}"));
    }
    // Damaged proofs: accepted only when they are still the same bytes.
    for _ in 0..4 {
        let bad = mutate_bytes(rng, &proof);
        let ok = check(&token, by, &box_name, &rn, &hn, &bad);
        let same_bytes = hex_bytes(&bad).is_some_and(|b| Some(b) == hex_bytes(&proof));
        if ok != same_bytes {
            return Err(format!("the damaged proof {bad:?} accepted: {ok}"));
        }
    }
    Ok(())
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `m` as the wire carries it: a command's `null` result is an absent one (`result: null`
/// decodes as none, and [`CmdResult::into_outcome`] reads both as `Ok(null)`).
fn wire_form(m: RunnerMsg) -> RunnerMsg {
    match m {
        RunnerMsg::CmdResult(mut r) => {
            if r.result == Some(Value::Null) {
                r.result = None;
            }
            RunnerMsg::CmdResult(r)
        }
        m => m,
    }
}

// ---------------------------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------------------------

fn one_case(rng: &mut Rng, st: &mut Stats, input: &mut String) -> Result<(), String> {
    match rng.below(20) {
        0..=3 => {
            st.generated += 1;
            let m = runner_msg(rng);
            let s = serde_json::to_string(&m).map_err(|e| e.to_string())?;
            *input = s.clone();
            let back: RunnerMsg = serde_json::from_str(&s).map_err(|e| format!("{e}"))?;
            if back != wire_form(m) {
                return Err(format!("decodes to {back:?}"));
            }
            check_text(st, &s)
        }
        4..=6 => {
            st.generated += 1;
            let m = hub_msg(rng);
            let s = serde_json::to_string(&m).map_err(|e| e.to_string())?;
            *input = s.clone();
            let back: HubMsg = serde_json::from_str(&s).map_err(|e| format!("{e}"))?;
            if back != m {
                return Err(format!("decodes to {back:?}"));
            }
            check_text(st, &s)
        }
        7..=11 => {
            let mut v = if rng.chance(50) {
                serde_json::to_value(runner_msg(rng))
            } else {
                serde_json::to_value(hub_msg(rng))
            }
            .map_err(|e| e.to_string())?;
            for _ in 0..1 + rng.below(3) {
                mutate_value(rng, &mut v);
            }
            *input = v.to_string();
            check_text(st, &input.clone())
        }
        12..=14 => {
            let s = serde_json::to_string(&runner_msg(rng)).map_err(|e| e.to_string())?;
            *input = mutate_bytes(rng, &s);
            check_text(st, &input.clone())
        }
        15 => {
            let limit = if rng.chance(70) {
                HELLO_MAX_BYTES
            } else {
                256 << 10
            };
            *input = junk(rng, limit);
            check_text(st, &input.clone())
        }
        16 => check_proofs(rng, st),
        _ => {
            let texts: Vec<String> = (0..3)
                .map(|_| {
                    let m = runner_msg(rng);
                    let s = serde_json::to_string(&m).unwrap_or_default();
                    if rng.chance(30) {
                        mutate_bytes(rng, &s)
                    } else {
                        s
                    }
                })
                .collect();
            let bytes = stream(rng, HELLO_MAX_BYTES, &texts);
            *input = format!(
                "{} stream bytes, {:?}…",
                bytes.len(),
                &bytes[..bytes.len().min(64)]
            );
            let peak = read_stream(st, bytes, pre_auth_ws_config(), HELLO_MAX_BYTES)?;
            st.max_ws_peak = st.max_ws_peak.max(peak);
            if peak > PRE_AUTH_READ_PEAK {
                return Err(format!(
                    "reading before authentication allocated {peak} bytes"
                ));
            }
            Ok(())
        }
    }
}

fn env_u64(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

fn run_fuzz(default_cases: u64) -> Stats {
    let seed = env_u64("TWR_FUZZ_SEED").unwrap_or(DEFAULT_SEED);
    let cases = env_u64("TWR_FUZZ_CASES").unwrap_or(default_cases);
    let only = env_u64("TWR_FUZZ_CASE");
    eprintln!("frame fuzz: TWR_FUZZ_SEED={seed:#x} TWR_FUZZ_CASES={cases}");
    let started = std::time::Instant::now();
    let mut st = Stats::default();
    for case in 0..cases {
        if only.is_some_and(|o| o != case) {
            continue;
        }
        let mut rng = Rng::for_case(seed, case);
        let mut input = String::new();
        let r = catch_unwind(AssertUnwindSafe(|| one_case(&mut rng, &mut st, &mut input)));
        let what = match r {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => format!("invariant violated: {e}"),
            Err(p) => format!(
                "panicked: {}",
                p.downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| p.downcast_ref::<&str>().copied())
                    .unwrap_or("?")
            ),
        };
        let shown: String = input.chars().take(3000).collect();
        panic!("frame fuzz (TWR_FUZZ_SEED={seed:#x} TWR_FUZZ_CASE={case}): {what}\ninput: {shown}");
    }
    eprintln!(
        "frame fuzz: {st:?} in {:.1}s",
        started.elapsed().as_secs_f64()
    );
    st
}

#[test]
fn frame_invariants() {
    let st = run_fuzz(DEFAULT_CASES);
    if env_u64("TWR_FUZZ_CASE").is_some() || env_u64("TWR_FUZZ_CASES").is_some_and(|n| n < 1000) {
        return;
    }
    // The generator must reach the interesting paths, or the fuzz proves little.
    assert!(st.runner_decoded * 4 > st.texts, "{st:?}");
    assert!(st.hub_decoded * 8 > st.texts, "{st:?}");
    assert!(st.ws_messages > 0 && st.ws_refused > 0, "{st:?}");
    assert!(st.proofs > 0 && st.pre_auth > 0, "{st:?}");
}

#[test]
#[ignore = "long run: cargo test -p twr-hub --release --test frame_fuzz -- --ignored"]
fn frame_invariants_long() {
    run_fuzz(LONG_CASES);
}

/// The limits at their edges, as the hub configures them: a message of exactly the limit is
/// read, one byte more is refused, and so is a header promising more (even 2^63 bytes)
/// before any payload is read or allocated.
#[test]
fn the_websocket_limits_hold_at_their_edges() {
    let mut st = Stats::default();
    let key = [1, 2, 3, 4];
    for (config, limit) in [
        (pre_auth_ws_config(), HELLO_MAX_BYTES),
        (runner_ws_config(), MAX_FRAME_BYTES),
    ] {
        let mut at = Vec::new();
        let payload = vec![b' '; limit];
        frame(&mut at, true, 1, &payload, limit as u64, key);
        let mut ws = WebSocket::from_raw_socket(
            Mem {
                input: at,
                at: 0,
                written: 0,
            },
            Role::Server,
            Some(config),
        );
        match ws.read() {
            Ok(Message::Text(t)) => assert_eq!(t.len(), limit),
            other => panic!("{limit}: {other:?}"),
        }
        for len in [limit as u64 + 1, 1 << 40, u64::MAX >> 1] {
            let mut over = Vec::new();
            frame(&mut over, true, 1, b"", len, key);
            let (r, peak) = peak_of(|| {
                let mut ws = WebSocket::from_raw_socket(
                    Mem {
                        input: over,
                        at: 0,
                        written: 0,
                    },
                    Role::Server,
                    Some(config),
                );
                ws.read()
            });
            assert!(
                matches!(
                    r,
                    Err(WsError::Capacity(CapacityError::MessageTooLong { .. }))
                ),
                "{limit}, {len}: {r:?}"
            );
            assert!(peak < 1 << 20, "{limit}, {len}: {peak} bytes");
        }
    }
    // Fragments past the limit together.
    let mut parts = Vec::new();
    let n = HELLO_MAX_BYTES / 1000 + 1;
    for i in 0..n {
        frame(
            &mut parts,
            i == n - 1,
            if i == 0 { 1 } else { 0 },
            &[b'z'; 1000],
            1000,
            key,
        );
    }
    let peak = read_stream(&mut st, parts, pre_auth_ws_config(), HELLO_MAX_BYTES).unwrap();
    assert_eq!(st.ws_refused, 1, "{st:?}");
    assert!(peak < PRE_AUTH_READ_PEAK, "{peak}");
}

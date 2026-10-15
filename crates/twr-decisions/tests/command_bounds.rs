//! Resource bounds of the short-command reader (`twr_decisions::code::command`):
//! the hub reads the user's whole message under its core lock, so every read must be fast and
//! small whatever the message, the labels or the registry.
//!
//! The tiny grammar bounds the work up front: a message over `MAX_MESSAGE_BYTES` (4 KiB) or
//! `MAX_TOKENS` (48) is no command before anything is parsed, and a target list of more than
//! `MAX_GROUPS` groups or a parse of more than `MAX_CLAUSES` clauses is none either. Labels are
//! never matched by their words (only permissions take an answer, by polarity, and a label is
//! read for polarity only when it is short), so a label of a megabyte costs nothing. What is
//! left is linear in the tokens times the threads (each token is resolved once).
//!
//! A counting global allocator measures the peak of live heap bytes while one message is read
//! (the tests here run one at a time, under a lock, so nothing else allocates meanwhile).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use twr_core::event::AttentionReason;
use twr_core::reduce::ThreadStatus;
use twr_decisions::code::command::{answer_coverage, mode_coverage, Coverage, Scene, Waiting};
use twr_decisions::context::ThreadInfo;

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

/// One measured test at a time (the peak counter is global).
static SERIAL: Mutex<()> = Mutex::new(());

/// The hub's largest message (`twr-hub` `route::MAX_MESSAGE`).
const MAX_MESSAGE: usize = 256 * 1024;

fn thread(id: &str, name: &str) -> ThreadInfo {
    ThreadInfo {
        id: id.into(),
        name: name.into(),
        aliases: Vec::new(),
        former_names: Vec::new(),
        project: None,
        status: ThreadStatus::Idle,
        runtime: "fake".into(),
        is_orchestrator: false,
        stopped: false,
        top_attention: None,
        last_report: None,
    }
}

fn perm(attn: &str, thread: &str) -> Waiting {
    Waiting {
        attn_id: attn.into(),
        thread: thread.into(),
        reason: AttentionReason::Permission,
        options: vec!["allow".into(), "deny".into()],
    }
}

/// `head`, `unit` repeated, and `tail`, up to [`MAX_MESSAGE`] bytes.
fn fill(head: &str, unit: &str, tail: &str) -> String {
    let mut s = head.to_string();
    while s.len() + unit.len() + tail.len() <= MAX_MESSAGE {
        s.push_str(unit);
    }
    s + tail
}

/// A call and the coverage it must get: (item or thread, label or mode, coverage).
type Expect<'a> = (&'a str, &'a str, Coverage);

/// A message, its answer call and its mode call.
type Case<'a> = (String, Expect<'a>, Expect<'a>);

/// Reads each case once per call against `threads` and `waiting`, within `limit` in a release
/// build (as the hub runs; a debug build gets thirty times as long, so a loaded CI runner never
/// trips it) and `peak_cap` bytes of live heap.
fn measure(
    threads: &[ThreadInfo],
    waiting: &[Waiting],
    cases: &[Case],
    peak_cap: usize,
    limit: Duration,
) {
    let limit = if cfg!(debug_assertions) {
        limit * 30
    } else {
        limit
    };
    for (msg, (attn, label, answer), (thread, mode, moded)) in cases {
        let scene = Scene {
            message: msg,
            threads,
            waiting,
        };
        let base = LIVE.load(Ordering::Relaxed);
        PEAK.store(base, Ordering::Relaxed);
        let t = Instant::now();
        let a = answer_coverage(&scene, attn, label);
        let m = mode_coverage(&scene, thread, mode);
        let took = t.elapsed();
        let peak = PEAK.load(Ordering::Relaxed) - base;
        let head: String = msg.chars().take(40).collect();
        assert_eq!((&a, &m), (answer, moded), "{head:?}");
        assert!(took < limit, "{head:?}: {took:?}");
        assert!(peak < peak_cap, "{head:?}: peak {peak} bytes");
        eprintln!("{head:?}: {took:?}, peak {} KiB", peak >> 10);
    }
}

#[test]
fn a_largest_message_is_no_command_at_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let threads = vec![thread("ta", "api"), thread("tb", "web")];
    let waiting = vec![perm("pa", "ta"), perm("pb", "tb")];
    use Coverage::Uncovered;
    let cases: Vec<Case> = [
        fill("allow api", ", api", ""),
        fill("allow api", " and api", ""),
        fill("set api", ", web, api", " to auto mode"),
        fill("", "allow api, the api one, ", ""),
        fill("", "allow api. ", ""),
        fill("", "allow api, set web to auto mode, ", "allow web"),
        fill("allow api", " ", ""),
        fill("", "x", ""),
    ]
    .into_iter()
    .map(|m| (m, ("pa", "allow", Uncovered), ("ta", "auto", Uncovered)))
    .collect();
    for (m, ..) in &cases {
        assert!(m.len() > MAX_MESSAGE - 64);
    }
    measure(
        &threads,
        &waiting,
        &cases,
        64 << 10,
        Duration::from_millis(200),
    );
}

#[test]
fn a_command_at_the_caps_is_read_quickly() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let mut threads = Vec::new();
    let mut waiting = Vec::new();
    for i in 0..16 {
        threads.push(thread(&format!("t{i}"), &format!("w{i}")));
        waiting.push(perm(&format!("p{i}"), &format!("t{i}")));
    }
    let names: Vec<String> = (0..16).map(|i| format!("w{i}")).collect();
    use Coverage::{Direct, Uncovered};
    // Eight clauses of two targets joined by ", and" (46 tokens): every joiner reads both as a
    // target joiner and as a clause joiner.
    let eight: Vec<String> = names
        .chunks(2)
        .map(|p| format!("allow {}, {}", p[0], p[1]))
        .collect();
    let eight = eight.join(", and ");
    // Sixteen groups in one list, then a mode: the list reads every prefix.
    let sixteen = format!("allow {}, and set w15 to auto mode", names.join(", "));
    let nine: Vec<String> = names[..9].iter().map(|n| format!("allow {n}")).collect();
    let cases: Vec<Case> = vec![
        (
            eight.clone(),
            ("p15", "allow", Direct),
            ("t0", "auto", Uncovered),
        ),
        (sixteen, ("p0", "allow", Direct), ("t15", "auto", Direct)),
        // Seventeen groups, or nine clauses: no command.
        (
            format!("allow {}, w0", names.join(", ")),
            ("p0", "allow", Uncovered),
            ("t0", "auto", Uncovered),
        ),
        (
            nine.join(", "),
            ("p0", "allow", Uncovered),
            ("t0", "auto", Uncovered),
        ),
        // 48 tokens of joiners and references that never parse.
        (
            vec!["w1 and"; 24].join(" "),
            ("p1", "allow", Uncovered),
            ("t1", "auto", Uncovered),
        ),
        (
            vec!["set w1 to"; 16].join(" "),
            ("p1", "allow", Uncovered),
            ("t1", "auto", Uncovered),
        ),
    ];
    measure(
        &threads,
        &waiting,
        &cases,
        1 << 20,
        Duration::from_millis(100),
    );
}

#[test]
fn huge_labels_cost_nothing() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let threads = vec![
        thread("ta", "api"),
        thread("tb", "web"),
        thread("td", "docs"),
        thread("te", "extra"),
    ];
    let mb = 1 << 20;
    let huge: Vec<String> = vec![
        "a ".repeat(mb / 2),
        "allow api, ".repeat(mb / 11),
        "x".repeat(mb),
        ", ".repeat(mb / 2) + "SQLite",
        "👍".repeat(mb / 4),
        " ".repeat(mb) + "allow",
    ];
    let mut waiting = vec![perm("pa", "ta")];
    // A question with huge labels, alone on docs: covered, never answered directly.
    waiting.push(Waiting {
        attn_id: "qd".into(),
        thread: "td".into(),
        reason: AttentionReason::Question,
        options: huge.clone(),
    });
    // A permission listing huge labels beside its allow / deny.
    let mut options = huge.clone();
    options.extend(["allow".to_string(), "deny".to_string()]);
    waiting.push(Waiting {
        attn_id: "pb".into(),
        thread: "tb".into(),
        reason: AttentionReason::Permission,
        options,
    });
    // Many items with huge labels on another thread.
    for i in 0..16 {
        waiting.push(Waiting {
            attn_id: format!("q{i}"),
            thread: "te".into(),
            reason: AttentionReason::Question,
            options: huge.iter().map(|h| format!("{h} {i}")).collect(),
        });
    }
    use Coverage::{Conflict, Direct, Uncovered};
    let cases: Vec<Case> = vec![
        (
            "allow api".to_string(),
            ("pa", "allow", Direct),
            ("ta", "auto", Uncovered),
        ),
        (
            "allow docs".to_string(),
            ("qd", &huge[0], Conflict),
            ("td", "auto", Uncovered),
        ),
        (
            "deny web".to_string(),
            ("pb", "deny", Direct),
            ("tb", "auto", Uncovered),
        ),
        (
            "approve all".to_string(),
            ("pa", "allow", Direct),
            ("tb", "auto", Uncovered),
        ),
        (
            "allow extra".to_string(),
            ("q0", &huge[1], Uncovered),
            ("te", "auto", Uncovered),
        ),
        (
            "set web to auto mode".to_string(),
            ("pb", "allow", Uncovered),
            ("tb", "auto", Direct),
        ),
    ];
    measure(
        &threads,
        &waiting,
        &cases,
        1 << 20,
        Duration::from_millis(100),
    );
}

#[test]
fn a_large_registry_is_read_quickly() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // 2,000 threads with 64-character names and four aliases each.
    let mut threads = Vec::new();
    let mut waiting = Vec::new();
    for i in 0..2000 {
        let name = format!("wk-{i:04}-{}", "x".repeat(56));
        let mut t = thread(&format!("t{i}"), &name);
        t.aliases = (0..4)
            .map(|k| format!("alias{k}-{i:04}-long-name"))
            .collect();
        threads.push(t);
        waiting.push(perm(&format!("p{i}"), &format!("t{i}")));
    }
    let last = &threads[1999].name.clone();
    let names: Vec<&str> = threads[..16].iter().map(|t| t.name.as_str()).collect();
    use Coverage::{Direct, Uncovered};
    let cases: Vec<Case> = vec![
        (
            format!("allow {last}"),
            ("p1999", "allow", Direct),
            ("t1999", "auto", Uncovered),
        ),
        (
            format!("allow {}", names.join(", ")),
            ("p15", "allow", Direct),
            ("t0", "auto", Uncovered),
        ),
        (
            format!("set {} to plan mode", names.join(" and ")),
            ("p0", "allow", Uncovered),
            ("t3", "plan", Direct),
        ),
        (
            "approve all".to_string(),
            ("p7", "allow", Direct),
            ("t7", "auto", Uncovered),
        ),
        (
            vec!["nomatch"; 48].join(" "),
            ("p7", "allow", Uncovered),
            ("t7", "auto", Uncovered),
        ),
    ];
    // The names are normalized once per read (about 1.2 MiB for this registry), and each
    // distinct token is resolved once.
    measure(&threads, &waiting, &cases, 4 << 20, Duration::from_secs(1));
}

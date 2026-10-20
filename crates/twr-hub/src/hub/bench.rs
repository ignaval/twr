//! How long a hub takes to read its log back as it starts ([`Core::replay_with`]): what a
//! `twr` that finds no hub waits for before anything shows. Run in release mode:
//!
//! ```sh
//! cargo test --release -p twr-hub --lib -- --ignored --nocapture replay_bench
//! ```
//!
//! By default on a log made up here, 40 threads with about 60 000 events; `TWR_BENCH_DB`
//! names a copy of a real `twr.db` instead (opening it may migrate its schema: never point it
//! at a hub's own).

use std::time::{Duration, Instant};

use twr_core::event::*;
use twr_core::testkit::ThreadScript;

use super::Core;
use crate::store::{AppendBatch, Store};

/// A log of 40 threads, each with 12 turns of text, tool calls and background tasks.
fn make_log(path: &std::path::Path) {
    let mut store = Store::open(path).unwrap();
    for t in 0..40 {
        let mut s = ThreadScript::new(format!("t-{t}"));
        let mut evs = vec![s.created(&format!("worker-{t}")), s.started("s1")];
        for turn in 0..12 {
            evs.push(s.turn_started(&format!("step {turn}")));
            for i in 0..30 {
                evs.push(s.text(&format!("Looking at step {turn}, part {i}: {}", "x".repeat(200))));
            }
            for i in 0..10 {
                evs.push(s.event(EventBody::AgentToolCall(AgentToolCall {
                    call_id: format!("c-{turn}-{i}"),
                    tool: "Bash".into(),
                    input_summary: "cargo test --workspace".into(),
                    ..Default::default()
                })));
                evs.push(s.event(EventBody::AgentToolResult(AgentToolResult {
                    call_id: format!("c-{turn}-{i}"),
                    ok: true,
                    output_summary: "test result: ok. 531 passed".into(),
                    ..Default::default()
                })));
            }
            for beat in 0..54 {
                evs.push(s.event(EventBody::TaskUpdated(TaskUpdated {
                    task_id: format!("task-{turn}-{}", beat / 18),
                    running: beat % 18 != 17,
                    description: "background task".into(),
                    heartbeat: beat % 6 == 5,
                    ..Default::default()
                })));
            }
            evs.push(s.turn_ended(TurnResult::Completed));
            evs.push(s.report(Some(turn + 1), Grade::Success));
        }
        store
            .append_batch(AppendBatch {
                events: evs.iter().map(|e| (e, e.is_control())).collect(),
                ..Default::default()
            })
            .unwrap();
    }
}

fn ms(d: Duration) -> String {
    format!("{:8.1} ms", d.as_secs_f64() * 1000.0)
}

/// The shortest of `rounds` runs of `f`.
fn best(rounds: usize, mut f: impl FnMut() -> Duration) -> Duration {
    (0..rounds).map(|_| f()).min().unwrap_or_default()
}

#[test]
#[ignore = "a benchmark: run in release mode with --ignored --nocapture"]
fn replay_bench() {
    let dir = tempfile::tempdir().unwrap();
    let path = match std::env::var_os("TWR_BENCH_DB") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let p = dir.path().join("twr.db");
            make_log(&p);
            p
        }
    };
    let n = Store::open(&path).unwrap().load_all().unwrap().len();
    eprintln!("{n} events (best of 3)");
    let load = best(3, || {
        let store = Store::open(&path).unwrap();
        let t = Instant::now();
        let events = store.load_all().unwrap();
        let d = t.elapsed();
        drop(events);
        d
    });
    eprintln!("{}  load and parse it on one thread", ms(load));
    let mut dropped = Duration::ZERO;
    let load = best(3, || {
        let store = Store::open(&path).unwrap();
        let t = Instant::now();
        let events = store.load_all_with_gseq().unwrap();
        let d = t.elapsed();
        let t = Instant::now();
        drop(events);
        dropped = t.elapsed();
        d
    });
    eprintln!("{}  load and parse it as a hub start does", ms(load));
    eprintln!("{}  drop what was parsed", ms(dropped));
    let replay = best(3, || {
        let t = Instant::now();
        let (core, _reader) = Core::replay_with(&path, None).unwrap();
        let d = t.elapsed();
        drop(core);
        d
    });
    eprintln!("{}  the whole replay (open, load, fold)", ms(replay));
}

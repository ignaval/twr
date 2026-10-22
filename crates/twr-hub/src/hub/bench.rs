//! How long a hub takes to read its log back as it starts ([`Core::replay_with`]): what a
//! `twr` that finds no hub waits for before anything shows. Run in release mode:
//!
//! ```sh
//! cargo test --release -p twr-hub --lib -- --ignored --nocapture replay_bench
//! ```
//!
//! By default on a log made up here, 40 threads with about 60 000 events; `TWR_BENCH_DB`
//! names a copy of a real `twr.db` instead (opening it may migrate its schema, and the bench
//! writes checkpoints into it: never point it at a hub's own). Then the same start from a
//! checkpoint (`hub::checkpoint`): one covering all but the last [`BEHIND`] events, and one
//! covering the whole log.

use std::time::{Duration, Instant};

use twr_core::event::*;
use twr_core::testkit::ThreadScript;

use super::{checkpoint, Core};
use crate::store::{self, AppendBatch, Store};

/// How far behind the log the older checkpoint is: about what an hour of work adds before the
/// next one is written ([`checkpoint::CHECKPOINT_EVERY`]).
const BEHIND: usize = checkpoint::CHECKPOINT_EVERY as usize;

/// A log of 40 threads, each with 12 turns of text, tool calls and background tasks.
fn make_log(path: &std::path::Path) {
    let mut store = Store::open(path).unwrap();
    for t in 0..40 {
        let mut s = ThreadScript::new(format!("t-{t}"));
        let mut evs = vec![s.created(&format!("worker-{t}")), s.started("s1")];
        for turn in 0..12 {
            evs.push(s.turn_started(&format!("step {turn}")));
            for i in 0..30 {
                evs.push(s.text(&format!(
                    "Looking at step {turn}, part {i}: {}",
                    "x".repeat(200)
                )));
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
        // (Whatever checkpoints the log has: the baseline folds every event.)
        let (core, _reader) = Core::replay_full(&path, None).unwrap();
        let d = t.elapsed();
        assert_eq!(core.replayed.checkpoint, None);
        assert_eq!(core.replayed.events, n);
        drop(core);
        d
    });
    eprintln!("{}  the whole replay (open, load, fold)", ms(replay));
    let notify = twr_core::config::NotifyConfig::default();
    let conn = store::connect(&path).unwrap();
    store::drop_checkpoints(&conn).unwrap();
    let log = store::load_with_gseq(&conn, None).unwrap();
    let behind = log[..log.len().saturating_sub(BEHIND)]
        .iter()
        .rev()
        .find(|(_, g)| g.is_some())
        .map(|(ev, _)| ev.id.clone());
    drop(log);
    for (what, upto) in [("older", behind), ("newest", None)] {
        let t = Instant::now();
        let Some(w) = checkpoint::write_upto(&path, &notify, 1, upto.as_deref()).unwrap() else {
            continue;
        };
        eprintln!(
            "{}  write the {what} checkpoint from nothing: {} events, {} KB",
            ms(t.elapsed()),
            w.events,
            w.bytes / 1024
        );
        let mut folded = 0;
        let start = best(3, || {
            let t = Instant::now();
            let (core, _reader) = Core::replay_with(&path, None).unwrap();
            let d = t.elapsed();
            assert_eq!(core.replayed.checkpoint, Some(w.id));
            folded = core.replayed.events;
            drop(core);
            d
        });
        eprintln!(
            "{}  the start from it (open, load it, fold {folded} events)",
            ms(start)
        );
        let load = best(3, || {
            let t = Instant::now();
            let state = store::checkpoint_state(&conn, w.id).unwrap().unwrap();
            let snap = checkpoint::FoldSnapshot::from_bytes(&state).unwrap();
            let d = t.elapsed();
            drop(snap);
            d
        });
        eprintln!("{}  of which reading its state", ms(load));
        store::drop_checkpoints(&conn).unwrap();
    }
}

/// How long selecting a compacted control backlog takes (`control.subscribe` with `compact`,
/// [`twr_core::compact`]): what the lock holds (the tasks at head), the rows the replay keeps
/// for it, the selection over them, and reading the rows from the store instead (what a hub
/// started from a checkpoint does in the background as it opens, `subscribe::warm_up`). Run in release mode on a copy of a real `twr.db`
/// (`TWR_BENCH_DB`, as [`replay_bench`]; a log made up here otherwise):
///
/// ```sh
/// TWR_BENCH_DB=copy.db cargo test --release -p twr-hub --lib -- --ignored --nocapture compact_bench
/// ```
///
/// It also checks that the rows read, those of the replay and the decoded events select alike.
#[test]
#[ignore = "a benchmark: run in release mode with --ignored --nocapture"]
fn compact_bench() {
    let dir = tempfile::tempdir().unwrap();
    let path = match std::env::var_os("TWR_BENCH_DB") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let p = dir.path().join("twr.db");
            make_log(&p);
            p
        }
    };
    let (mut core, reader) = Core::replay_full(&path, None).unwrap();
    let head = core.gseq;
    let mut at_head = twr_core::compact::Head::default();
    let lock = best(5, || {
        let t = Instant::now();
        at_head = twr_core::compact::Head::of(&core.registry);
        t.elapsed()
    });
    eprintln!(
        "{}  the tasks at head, under the lock ({} tasks)",
        ms(lock),
        at_head.tasks.len()
    );
    let events = crate::store::read_control(&reader, 0, usize::MAX >> 1).unwrap();
    let build = best(5, || {
        let t = Instant::now();
        let rows: Vec<_> = events
            .iter()
            .map(|(g, e)| twr_core::compact::Row::of(*g, e))
            .collect();
        let d = t.elapsed();
        drop(rows);
        d
    });
    eprintln!("{}  the replay's rows ({} items)", ms(build), events.len());
    let replayed = std::mem::take(&mut core.compact_rows);
    let mut kept = Vec::new();
    let mut select_cpu = Duration::MAX;
    let select = best(5, || {
        let (t, cpu) = (Instant::now(), thread_cpu());
        kept = twr_core::compact::select(replayed.rows(), 0, &at_head);
        select_cpu = select_cpu.min(thread_cpu() - cpu);
        t.elapsed()
    });
    eprintln!(
        "{}  select {} of {} items ({} of CPU)",
        ms(select),
        kept.len(),
        events.len(),
        ms(select_cpu).trim()
    );
    let mut rows = Vec::new();
    let mut read_cpu = Duration::MAX;
    let read = best(5, || {
        let (t, cpu) = (Instant::now(), thread_cpu());
        rows.clear();
        let mut last = 0;
        while last < head {
            let batch = crate::store::read_control_rows(&reader, last, head, 4096).unwrap();
            let Some(end) = batch.last() else {
                break;
            };
            last = end.gseq;
            rows.extend(batch);
        }
        read_cpu = read_cpu.min(thread_cpu() - cpu);
        t.elapsed()
    });
    eprintln!(
        "{}  read the {} rows from the store instead, as after a checkpoint ({} of CPU)",
        ms(read),
        rows.len(),
        ms(read_cpu).trim()
    );
    assert_eq!(
        kept,
        twr_core::compact::select(&rows, 0, &at_head),
        "rows read select alike"
    );
    let decoded = twr_core::compact::select_events(events.iter().map(|(g, e)| (*g, e)), 0);
    assert_eq!(kept, decoded, "rows and events select alike");
}

/// The CPU time this thread has used: what a benchmark spent, whatever else the machine runs.
fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid timespec for the call to write.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

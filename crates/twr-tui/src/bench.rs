//! How long the TUI takes to read and apply a replay, and to draw the orchestrator
//! conversation it ends on: the work between connecting to the hub and the first frame, and
//! between Esc out of a dive and the frame back in the conversation. Run in release mode:
//!
//! ```sh
//! cargo test --release -p twr-tui --lib -- --ignored --nocapture replay_bench
//! ```
//!
//! By default it replays a log made up here, about the size of a long-used hub's: 40 threads
//! with 30 000 control items, most of them `task.updated`, and an orchestrator conversation of
//! [`crate::TRANSCRIPT_TAIL`] events. `TWR_BENCH_CONTROL` and `TWR_BENCH_TRANSCRIPT` name files
//! of a real hub's stream lines instead (what `control.subscribe` and `thread.subscribe` push,
//! one JSON line each).

use std::time::{Duration, Instant};

use twr_core::event::*;
use twr_core::proto::{ServerMessage, StreamItem, StreamKind};
use twr_core::testkit::ThreadScript;

use crate::app::tests::{draw, orch_events};
use crate::app::AppState;

/// The lines of a stream, as the hub writes them.
fn lines(items: &[StreamItem]) -> Vec<String> {
    items
        .iter()
        .map(|i| serde_json::to_string(i).unwrap())
        .collect()
}

/// A long-used hub's control stream and its orchestrator's latest events, made up.
fn made_up() -> (Vec<String>, Vec<String>) {
    let mut control = Vec::new();
    let mut gseq = 0;
    let mut push = |control: &mut Vec<StreamItem>, event: Event| {
        gseq += 1;
        control.push(StreamItem {
            stream: StreamKind::Control,
            gseq: Some(gseq),
            event,
        });
    };
    let mut o = ThreadScript::new("t-orch");
    for ev in orch_events(&mut o, "t-orch") {
        push(&mut control, ev);
    }
    for t in 0..40 {
        let mut s = ThreadScript::new(format!("t-{t}"));
        push(&mut control, s.created(&format!("worker-{t}")));
        push(&mut control, s.started("s1"));
        for turn in 0..12 {
            push(&mut control, s.turn_started(&format!("step {turn}")));
            for task in 0..3 {
                for beat in 0..18 {
                    let ev = s.event(EventBody::TaskUpdated(TaskUpdated {
                        task_id: format!("task-{turn}-{task}"),
                        running: beat < 17,
                        description: format!("background task {task}"),
                        heartbeat: beat % 6 == 5,
                        ..Default::default()
                    }));
                    push(&mut control, ev);
                }
            }
            push(&mut control, s.turn_ended(TurnResult::Completed));
            push(&mut control, s.report(Some(turn + 1), Grade::Success));
            push(&mut control, s.git("main", turn % 3));
        }
    }
    let mut transcript = Vec::new();
    let turns = crate::TRANSCRIPT_TAIL / 5;
    for turn in 0..turns {
        transcript.push(o.turn_started(&format!("what about step {turn}?")));
        transcript.push(o.text(&format!(
            "Step {turn} is done. The worker ran the tests, **all green**, and wrote:\n\n\
             - `crates/twr-tui/src/lib.rs`: the loop\n- `docs/tui.md`: the keys\n\n\
             Next I'll ask it to look at the hub's side, unless you want something else first."
        )));
        transcript.push(o.event(EventBody::AgentToolCall(AgentToolCall {
            call_id: format!("call-{turn}"),
            tool: "Bash".into(),
            input_summary: format!("twr status worker-{}", turn % 40),
            ..Default::default()
        })));
        transcript.push(o.event(EventBody::AgentToolResult(AgentToolResult {
            call_id: format!("call-{turn}"),
            ok: true,
            output_summary: "worker: idle, 3 commits ahead of main".into(),
            ..Default::default()
        })));
        transcript.push(o.turn_ended(TurnResult::Completed));
    }
    let transcript: Vec<StreamItem> = transcript
        .into_iter()
        .map(|event| StreamItem {
            stream: StreamKind::Thread,
            gseq: None,
            event,
        })
        .collect();
    (lines(&control), lines(&transcript))
}

/// The lines of `var`'s file, if it names one.
fn from_file(var: &str) -> Option<Vec<String>> {
    let path = std::env::var_os(var)?;
    let text = std::fs::read_to_string(path).unwrap();
    Some(text.lines().map(str::to_string).collect())
}

/// Each line decoded as the client decodes what the hub pushes.
fn decode(lines: &[String]) -> Vec<StreamItem> {
    lines
        .iter()
        .map(|l| match ServerMessage::from_line(l.as_bytes()).unwrap() {
            ServerMessage::Stream(i) => *i,
            ServerMessage::Response(r) => panic!("a response in the stream: {r:?}"),
        })
        .collect()
}

fn ms(d: Duration) -> String {
    format!("{:8.1} ms", d.as_secs_f64() * 1000.0)
}

#[test]
#[ignore = "a benchmark: run in release mode with --ignored --nocapture"]
fn replay_bench() {
    let made = made_up();
    let control = from_file("TWR_BENCH_CONTROL").unwrap_or(made.0);
    let transcript = from_file("TWR_BENCH_TRANSCRIPT").unwrap_or(made.1);
    let bytes = |l: &[String]| l.iter().map(String::len).sum::<usize>() as f64 / 1e6;
    eprintln!(
        "control: {} items, {:.1} MB; transcript: {} events, {:.1} MB",
        control.len(),
        bytes(&control),
        transcript.len(),
        bytes(&transcript)
    );

    let t = Instant::now();
    let control = decode(&control);
    eprintln!("{}  decode the control replay", ms(t.elapsed()));
    let t = Instant::now();
    let capped = control
        .iter()
        .filter(|i| i.event.body.maybe_capped())
        .count();
    eprintln!(
        "{}  look for capped fields ({capped} may have some)",
        ms(t.elapsed())
    );
    let mut st = AppState::new();
    st.begin_catch_up(control.last().and_then(|i| i.gseq).unwrap_or(0));
    let t = Instant::now();
    for item in &control {
        st.apply_event(item);
    }
    eprintln!("{}  apply the control replay", ms(t.elapsed()));
    assert_eq!(st.catch_up, None, "the replay is applied");

    let orch = st.main_thread().expect("an orchestrator");
    let t = Instant::now();
    let transcript = decode(&transcript);
    eprintln!("{}  decode the transcript", ms(t.elapsed()));
    let t = Instant::now();
    for item in transcript.iter().filter(|i| i.event.thread == orch) {
        st.apply_event(item);
    }
    eprintln!("{}  apply the transcript", ms(t.elapsed()));

    // The conversation laid out alone, as the main pane does it each frame.
    let entries = st.transcript.as_ref().unwrap().entries.clone();
    let expanded = std::collections::HashSet::new();
    let collapse = crate::ui::Collapse {
        on: true,
        quiet_ends: true,
        all: false,
        expanded: Some(&expanded),
    };
    let threads = crate::ui::Threads {
        boxes: true,
        state: Some(&st),
    };
    let t = Instant::now();
    for _ in 0..10 {
        crate::ui::transcript_view_spans(&entries, 218, &collapse, &threads);
    }
    eprintln!(
        "{}  lay out the conversation (mean of 10, {} entries)",
        ms(t.elapsed() / 10),
        entries.len()
    );

    let (w, h) = (220, 55);
    let t = Instant::now();
    draw(&mut st, w, h);
    eprintln!(
        "{}  draw the orchestrator conversation, first",
        ms(t.elapsed())
    );
    let t = Instant::now();
    for _ in 0..10 {
        draw(&mut st, w, h);
    }
    eprintln!("{}  draw it again (mean of 10)", ms(t.elapsed() / 10));
}

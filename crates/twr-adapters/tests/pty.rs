//! PTY fallback adapter tests (tier pty, DEGRADED). The programs are small `sh -c` scripts
//! run under a real pseudo-terminal; the timing is shortened so the idle heuristics fire fast.

use std::path::Path;
use std::time::Duration;

use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use twr_adapters::pty::{
    self, parse_command, program_env, split_command_line, PtyAdapter, Timing, COMMAND_ENV,
};
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, Answer, SpawnConfig, Tier};
use twr_core::event::*;

const WAIT: Duration = Duration::from_secs(20);

fn fast() -> Timing {
    Timing {
        idle: Duration::from_millis(400),
        settle: Duration::from_millis(100),
        startup_max: Duration::from_millis(1000),
        flush: Duration::from_millis(50),
    }
}

type Rx = UnboundedReceiver<AdapterEvent>;

fn start(adapter: PtyAdapter, cfg: SpawnConfig, prompt: Option<&str>) -> (AdapterHandle, Rx) {
    let mut h = adapter
        .timing(fast())
        .stop_grace(Duration::from_millis(500))
        .spawn(cfg, prompt.map(str::to_string))
        .unwrap();
    let rx = h.take_events().unwrap();
    (h, rx)
}

fn script(body: &str) -> PtyAdapter {
    PtyAdapter::with_command(["sh", "-c", body])
}

async fn until(rx: &mut Rx, pred: impl Fn(&AdapterEvent) -> bool) -> Vec<AdapterEvent> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = rx.recv().await.expect("adapter channel closed");
            let done = pred(&ev);
            seen.push(ev);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {seen:?}"));
    seen
}

fn is_kind(ev: &AdapterEvent, kind: &str) -> bool {
    matches!(ev, AdapterEvent::Emit { body, .. } if body.kind() == kind)
}

fn is_exit(ev: &AdapterEvent) -> bool {
    matches!(ev, AdapterEvent::Exited { .. })
}

/// Concatenated text deltas.
fn deltas(evs: &[AdapterEvent]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::AgentText(t),
                ..
            } if t.delta => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn ext_of(body: &EventBody) -> Option<JsonMap> {
    let data = body.data().unwrap();
    data.get("ext")
        .and_then(|e| e.as_object())
        .map(|m| m.clone().into_iter().collect())
}

/// Every event says it is degraded.
fn assert_degraded(evs: &[AdapterEvent]) {
    for e in evs {
        if let AdapterEvent::Emit { body, .. } = e {
            let ext = ext_of(body).unwrap_or_else(|| panic!("no ext on {body:?}"));
            assert_eq!(ext["pty"]["degraded"], true, "{body:?}");
        }
    }
}

const QUESTION_SCRIPT: &str = r#"printf '> '; read cmd; echo "running $cmd"; printf 'Continue? [y/N] '; read a; echo "got $a"; printf '> '; read x; echo "bye $x""#;

#[tokio::test]
async fn question_answer_turn_end_and_exit() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SpawnConfig {
        thread: "t1".into(),
        cwd: dir.path().to_path_buf(),
        env: vec![("TWR_THREAD".into(), "t1".into())],
        ..Default::default()
    };
    let (h, mut rx) = start(script(QUESTION_SCRIPT), cfg, Some("task"));
    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    assert_degraded(&evs);
    match &evs[0] {
        AdapterEvent::Emit {
            body: EventBody::ThreadStarted(s),
            turn: None,
            ..
        } => {
            assert!(s.session_id.as_deref().unwrap().starts_with("pty-"));
            let ext = s.ext.as_ref().unwrap();
            assert!(ext["twr"]["pid"].as_u64().is_some());
            assert_eq!(ext["twr"]["exe"], "sh");
            assert!(ext["pty"]["warning"]
                .as_str()
                .unwrap()
                .starts_with("DEGRADED"));
        }
        other => panic!("first event {other:?}"),
    }
    let text = deltas(&evs);
    assert!(text.contains("running task\n"), "{text:?}");
    assert!(text.ends_with("Continue? [y/N] "), "{text:?}");
    // The typed prompt's echo is not shown as output.
    assert!(!text.lines().any(|l| l.trim() == "task"), "{text:?}");
    let a = match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::AttentionRaised(a),
            turn,
            ..
        }) => {
            assert_eq!(*turn, Some(1));
            a.clone()
        }
        _ => unreachable!(),
    };
    assert_eq!(a.reason, AttentionReason::Question);
    assert_eq!(a.options, ["y", "n"]);
    assert_eq!(a.summary, "Continue? [y/N]");
    assert_eq!(a.ext.as_ref().unwrap()["pty"]["heuristic"], true);

    h.respond(&a.attn_id, Answer::allow()).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert_degraded(&evs);
    let text = deltas(&evs);
    assert!(text.contains("got y\n"), "{text:?}");
    assert!(!text.lines().any(|l| l.trim() == "y"), "{text:?}");
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::TurnEnded(e),
            turn,
            ..
        }) => {
            assert_eq!(*turn, Some(1));
            assert_eq!(e.result, TurnResult::Completed);
            let p = &e.ext.as_ref().unwrap()["pty"];
            assert_eq!(p["heuristic"], true);
            assert_eq!(p["end"], "input_prompt");
        }
        _ => unreachable!(),
    }
    let final_text: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::AgentText(t),
                ..
            } if t.is_final => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(final_text, ["got y"]);

    // The next turn's answer ends the script: the exit ends the turn.
    h.send("again", 2).unwrap();
    let evs = until(&mut rx, is_exit).await;
    assert!(deltas(&evs).contains("bye again"), "{evs:?}");
    let ended: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(t),
                turn,
                ..
            } => Some((
                *turn,
                t.result,
                t.ext.as_ref().unwrap()["pty"]["end"].clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        [(Some(2), TurnResult::Completed, serde_json::json!("exit"))]
    );
    match evs.last() {
        Some(AdapterEvent::Exited { code, stderr_tail }) => {
            assert_eq!(*code, Some(0));
            assert!(stderr_tail.contains("bye again"), "{stderr_tail:?}");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn command_and_idle_come_from_the_spawn_env_and_are_not_passed_on() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        env: vec![
            ("TWR_THREAD".into(), "t9".into()),
            (
                COMMAND_ENV.into(),
                r#"echo "cmd=${TWR_PTY_COMMAND:-unset} idle=${TWR_PTY_IDLE_MS:-unset} term=$TERM thread=$TWR_THREAD pwd=$(pwd)"; read x"#.into(),
            ),
            (pty::IDLE_ENV.into(), "300".into()),
        ],
        ..Default::default()
    };
    let mut h = PtyAdapter::new()
        .stop_grace(Duration::from_millis(500))
        .spawn(cfg, None)
        .unwrap();
    let mut rx = h.take_events().unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "agent.text")).await;
    match &evs[0] {
        AdapterEvent::Emit {
            body: EventBody::ThreadStarted(s),
            ..
        } => {
            let p = &s.ext.as_ref().unwrap()["pty"];
            assert_eq!(p["idle_ms"], 300);
            assert_eq!(p["command"][0], "sh");
        }
        other => panic!("{other:?}"),
    }
    let text = deltas(&evs);
    let pwd = dir.path().canonicalize().unwrap();
    assert!(
        text.contains(&format!(
            "cmd=unset idle=unset term=dumb thread=t9 pwd={}",
            pwd.display()
        )),
        "{text:?}"
    );
    // Output outside a turn carries no turn.
    assert!(matches!(
        evs.last(),
        Some(AdapterEvent::Emit { turn: None, .. })
    ));
    h.stop().unwrap();
    until(&mut rx, is_exit).await;
}

const CONSENT_SCRIPT: &str = r#"printf 'Trust this directory? [y/N] '; read a; echo "consent=$a"; printf '> '; read t; echo "task=$t"; printf '> '; read x"#;

/// A CLI that starts with a consent prompt never gets the task typed
/// into it. The task waits behind a question; the user's answer goes to the prompt, and the
/// task follows once the CLI shows its input prompt. A resumed (or retried) process, which
/// starts the command afresh, gets the same gate.
#[tokio::test]
async fn a_startup_consent_prompt_is_never_answered_with_the_task() {
    for resume in [None, Some("pty-old".to_string())] {
        let cfg = SpawnConfig {
            resume_session: resume.clone(),
            prompt_turn: Some(3),
            ..Default::default()
        };
        let (h, mut rx) = start(script(CONSENT_SCRIPT), cfg, Some("build it"));
        let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
        let (a, turn) = match evs.last() {
            Some(AdapterEvent::Emit {
                body: EventBody::AttentionRaised(a),
                turn,
                ..
            }) => (a.clone(), *turn),
            _ => unreachable!(),
        };
        assert_eq!(turn, Some(3), "{resume:?}");
        assert_eq!(a.reason, AttentionReason::Question);
        assert_eq!(a.options, ["y", "n"]);
        assert_eq!(a.summary, "Trust this directory? [y/N]");
        assert_eq!(a.ext.as_ref().unwrap()["pty"]["held"], true);
        // Nothing was typed: the consent prompt is still waiting.
        let text = deltas(&evs);
        assert!(!text.contains("consent="), "{text:?}");
        assert!(!evs.iter().any(|e| is_kind(e, "turn.ended")));
        h.respond(&a.attn_id, Answer::allow()).unwrap();
        let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
        let text = deltas(&evs);
        assert!(text.contains("consent=y\n"), "{text:?}");
        assert!(text.contains("task=build it\n"), "{text:?}");
        assert!(!text.contains("consent=build"), "{text:?}");
        match evs.last() {
            Some(AdapterEvent::Emit { turn, .. }) => assert_eq!(*turn, Some(3)),
            _ => unreachable!(),
        }
        h.stop().unwrap();
        until(&mut rx, is_exit).await;
    }
}

/// twr's secrets never reach a PTY program (inherited or configured).
#[tokio::test]
async fn the_program_never_inherits_twr_secrets() {
    std::env::set_var("TWR_RUNNER_TOKEN", "runner-secret");
    let cfg = SpawnConfig {
        env: vec![
            ("TWR_ORCH_TOKEN".into(), "orch-secret".into()),
            ("TWR_THREAD".into(), "t5".into()),
        ],
        ..Default::default()
    };
    let (h, mut rx) = start(
        script(
            r#"echo "r=${TWR_RUNNER_TOKEN:-unset} o=${TWR_ORCH_TOKEN:-unset} t=$TWR_THREAD"; read x"#,
        ),
        cfg,
        None,
    );
    let evs = until(&mut rx, |e| is_kind(e, "agent.text")).await;
    let text = deltas(&evs);
    assert!(text.contains("r=unset o=unset t=t5"), "{text:?}");
    h.stop().unwrap();
    until(&mut rx, is_exit).await;
}

#[tokio::test]
async fn crash_during_a_turn_ends_it_with_an_error() {
    let (_h, mut rx) = start(
        script("printf '> '; read x; echo \"boom $x\"; exit 3"),
        SpawnConfig::default(),
        Some("now"),
    );
    let evs = until(&mut rx, is_exit).await;
    let ended: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(t),
                turn,
                ..
            } => Some((*turn, t.result)),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [(Some(1), TurnResult::Error)]);
    match evs.last() {
        Some(AdapterEvent::Exited { code, stderr_tail }) => {
            assert_eq!(*code, Some(3));
            assert!(stderr_tail.contains("boom now"), "{stderr_tail:?}");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn stop_hangs_up_then_kills_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    // Ignores the hangup; leaves a background child in its process group.
    let body = format!(
        "trap '' HUP TERM; sleep 300 & echo $! > {}; while :; do sleep 1; done",
        pidfile.display()
    );
    let (h, mut rx) = start(script(&body), SpawnConfig::default(), Some("go"));
    let child: u32 = timeout(WAIT, async {
        loop {
            let text = std::fs::read_to_string(&pidfile).unwrap_or_default();
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.stop().unwrap();
    let evs = until(&mut rx, is_exit).await;
    // twr stopped it: the hub, not the adapter, closes the turn.
    assert!(!evs.iter().any(|e| is_kind(e, "turn.ended")), "{evs:?}");
    match evs.last() {
        Some(AdapterEvent::Exited { code, .. }) => assert_eq!(*code, None, "killed"),
        _ => unreachable!(),
    }
    let gone = timeout(WAIT, async {
        loop {
            if gone_or_zombie(child) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "background child {child} survived stop");
}

#[tokio::test]
async fn stop_of_a_well_behaved_program_is_a_hangup() {
    let (h, mut rx) = start(script("read x"), SpawnConfig::default(), None);
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.stop().unwrap();
    let started = std::time::Instant::now();
    let evs = until(&mut rx, is_exit).await;
    assert!(
        started.elapsed() < Duration::from_millis(450),
        "no kill wait"
    );
    assert!(matches!(evs.last(), Some(AdapterEvent::Exited { .. })));
}

/// One prompt is one submit: a line-reading program gets the whole line once; a program that
/// enabled bracketed paste and reads raw input (as TUIs do) gets a multi-line prompt as one
/// paste followed by a single `\r`.
#[tokio::test]
async fn prompts_are_typed_with_one_submit() {
    let (h, mut rx) = start(
        script(r#"printf '> '; read -r l; echo "got:$l"; printf '> '; read -r l; echo "got:$l""#),
        SpawnConfig::default(),
        Some("hello world"),
    );
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    let text = deltas(&evs);
    assert!(text.contains("got:hello world\n"), "{text:?}");
    assert!(!text.contains("> hello world"), "echo dropped: {text:?}");
    h.stop().unwrap();
    until(&mut rx, is_exit).await;

    // Raw mode, no echo: reports what arrived.
    const RAW: &str = r#"
import os, tty
tty.setraw(0)
os.write(1, b"\x1b[?2004h> ")
buf = b""
while not buf.endswith(b"\r") or buf.count(b"\x1b[200~") != buf.count(b"\x1b[201~"):
    buf += os.read(0, 4096)
start, end = buf.find(b"\x1b[200~"), buf.find(b"\x1b[201~")
body = buf[start + 6:end] if start >= 0 else b""
outside = buf[:start] + buf[end + 6:] if start >= 0 else buf
report = "paste=%s lines=%d submits=%d body=%s" % (
    start >= 0, body.count(b"\r") + 1, outside.count(b"\r"), body.decode().replace("\r", "|"))
os.write(1, ("\r\n" + report + "\r\n> ").encode())
os.read(0, 4096)
"#;
    let (h, mut rx) = start(
        PtyAdapter::with_command(["python3", "-c", RAW]),
        SpawnConfig::default(),
        Some("first line\nsecond line\nthird"),
    );
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    let text = deltas(&evs);
    assert!(
        text.contains("paste=True lines=3 submits=1 body=first line|second line|third\n"),
        "{text:?}"
    );
    h.stop().unwrap();
    until(&mut rx, is_exit).await;
}

/// A line-oriented program that never enabled bracketed paste would take each embedded `\r`
/// of a multi-line prompt as its own submit, so a line `y` in the task would answer its next
/// permission prompt. The task arrives flattened as one line instead, and the permission
/// prompt is raised for the user.
#[tokio::test]
async fn a_multi_line_task_cannot_answer_a_later_prompt() {
    let (h, mut rx) = start(
        script(
            r#"printf '> '; read -r task; echo "task=$task"; printf 'Allow? [y/N] '; read -r a; echo "answer=$a"; printf '> '; read -r x"#,
        ),
        SpawnConfig::default(),
        Some("first line\ny\nthird line"),
    );
    let evs = until(&mut rx, |e| is_kind(e, "attention.raised") || is_exit(e)).await;
    let text = deltas(&evs);
    assert!(!text.contains("answer=y"), "{text:?}");
    assert!(
        text.contains("task=first line y third line\n"),
        "one line: {text:?}"
    );
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::AttentionRaised(a),
            ..
        }) => assert!(a.summary.contains("Allow? [y/N]"), "{a:?}"),
        other => panic!("expected the Allow prompt raised, got {other:?}"),
    }
    h.stop().unwrap();
    until(&mut rx, is_exit).await;
}

/// A leftover that left the process group with `setsid` and still holds the terminal does not
/// keep twr reading: once the exit is drained every master descriptor is closed, so the
/// leftover's writes to the terminal fail (a hangup) and it ends.
// Linux only: macOS has no `setsid` command.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_setsid_leftover_holding_the_terminal_is_hung_up() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("hup");
    let pidfile = dir.path().join("pid");
    let body = format!(
        "setsid sh -c 'echo $$ > {pid}; while echo tick; do sleep 0.1; done; echo hup > {marker}' & \
         sleep 0.5; exit 0",
        pid = pidfile.display(),
        marker = marker.display(),
    );
    let (_h, mut rx) = start(script(&body), SpawnConfig::default(), None);
    let evs = until(&mut rx, is_exit).await;
    match evs.last() {
        Some(AdapterEvent::Exited { code, stderr_tail }) => {
            assert_eq!(*code, Some(0));
            assert!(stderr_tail.contains("tick"), "{stderr_tail:?}");
        }
        other => panic!("{other:?}"),
    }
    let hung_up = timeout(WAIT, async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    // Clean up the leftover whatever happened.
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", pid.trim()])
            .stderr(std::process::Stdio::null())
            .status();
    }
    assert!(
        hung_up.is_ok(),
        "the leftover still had a reader for its terminal"
    );
}

#[tokio::test]
async fn spawn_errors() {
    let err = PtyAdapter::new()
        .spawn(SpawnConfig::default(), None)
        .unwrap_err();
    assert!(format!("{err:#}").contains(COMMAND_ENV), "{err:#}");
    let err = script("true")
        .spawn(
            SpawnConfig {
                cwd: "/nonexistent/dir".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/dir"), "{err:#}");
    let err = PtyAdapter::with_command(["/nonexistent/prog"])
        .spawn(SpawnConfig::default(), None)
        .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/prog"), "{err:#}");
    let err = script("true")
        .spawn(
            SpawnConfig {
                env: vec![(pty::IDLE_ENV.into(), "soon".into())],
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains(pty::IDLE_ENV), "{err:#}");
}

#[test]
fn command_lines_split_like_a_shell_without_expansion() {
    let ok = |line: &str| split_command_line(line).unwrap();
    assert_eq!(ok("aider --model x"), ["aider", "--model", "x"]);
    assert_eq!(ok("  aider\t --yes  "), ["aider", "--yes"]);
    assert_eq!(
        ok(r#"aider --message 'fix the "bug"' --x="a b""#),
        ["aider", "--message", r#"fix the "bug""#, "--x=a b"]
    );
    assert_eq!(
        ok(r#"say "a \"q\" \$HOME \n""#),
        ["say", r#"a "q" $HOME \n"#]
    );
    assert_eq!(ok(r"a\ b c\'d"), ["a b", "c'd"]);
    assert_eq!(ok("run '' x"), ["run", "", "x"]);
    assert_eq!(ok("it's'' ok'"), ["its ok"]);
    // Nothing is expanded or interpreted.
    assert_eq!(
        ok("echo $HOME ~ *.rs; ls | wc"),
        ["echo", "$HOME", "~", "*.rs;", "ls", "|", "wc"]
    );
    assert_eq!(
        ok("sh -c 'aider --model \"$M\"'"),
        ["sh", "-c", "aider --model \"$M\""]
    );
    for bad in ["", "   ", "'x", "\"x", "x\\", "'' x"] {
        assert!(split_command_line(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn commands_and_env() {
    assert_eq!(
        parse_command(r#"["aider", "--model", "x"]"#).unwrap(),
        ["aider", "--model", "x"]
    );
    assert_eq!(
        parse_command("aider --yes").unwrap(),
        ["sh", "-c", "aider --yes"]
    );
    assert!(parse_command("  ").is_err());
    assert!(parse_command("[]").is_err());
    assert!(parse_command("[1]").is_err());
    let cfg = SpawnConfig {
        env: vec![
            ("TERM".into(), "xterm".into()),
            (COMMAND_ENV.into(), "x".into()),
            ("A".into(), "1".into()),
        ],
        ..Default::default()
    };
    assert_eq!(
        program_env(&cfg),
        [
            ("TERM".to_string(), "xterm".to_string()),
            ("A".to_string(), "1".to_string()),
            ("NO_COLOR".to_string(), "1".to_string()),
        ]
    );
}

#[test]
fn registered_as_degraded_pty_tier() {
    let a = twr_adapters::find(pty::NAME).unwrap();
    assert_eq!(a.info().tier, Tier::Pty);
    assert!(a.info().description.starts_with("DEGRADED"));
    let cfg = SpawnConfig {
        cwd: Path::new("/tmp").to_path_buf(),
        ..Default::default()
    };
    assert!(a.attach_command(&cfg, "pty-x").is_none());
}

/// Whether process `pid` is gone or a zombie (read with `ps`, which works without `/proc`).
fn gone_or_zombie(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim_start().starts_with('Z')
}

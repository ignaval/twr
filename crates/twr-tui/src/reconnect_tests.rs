//! The TUI's loop ([`crate::show`]) against a hub that stops and starts again under it (`twr
//! hub stop` then `twr`, or `twr update`): it says the connection is lost, connects again on
//! its own, and what the new hub sends (control items, transcript events) shows.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::backend::{Backend, ClearType, TestBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use twr_core::event::{AttentionReason, EventBody};
use twr_core::proto::{StreamItem, StreamKind};
use twr_core::testkit::ThreadScript;

use crate::*;

/// What the stand-in hub's log holds, kept across its restarts as a hub's database is: the
/// control stream's events (gseq = place + 1) and the orchestrator's transcript.
#[derive(Default)]
struct Log {
    control: Vec<twr_core::event::Event>,
    transcript: Vec<twr_core::event::Event>,
}

/// A stand-in hub on a socket: every connection answers `hello` and the calls the TUI makes,
/// `control.subscribe` and `thread.subscribe` stream what [`Log`] holds and what is added to
/// it (`bump` says when). It records the methods it got.
struct Hub {
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
    got: Arc<Mutex<Vec<String>>>,
    socket: PathBuf,
}

impl Hub {
    fn start(socket: &Path, log: Arc<Mutex<Log>>, bump: tokio::sync::watch::Receiver<u64>) -> Hub {
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let got: Arc<Mutex<Vec<String>>> = Arc::default();
        let (t, g) = (tasks.clone(), got.clone());
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let task = tokio::spawn(serve(stream, log.clone(), bump.clone(), g.clone()));
                t.lock().unwrap().push(task.abort_handle());
            }
        });
        tasks.lock().unwrap().push(accept.abort_handle());
        Hub {
            tasks,
            got,
            socket: socket.to_path_buf(),
        }
    }

    /// Gone, as a stopped hub is: every connection closed, the socket removed.
    fn kill(&self) {
        for t in self.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
        let _ = std::fs::remove_file(&self.socket);
    }

    fn got(&self) -> Vec<String> {
        self.got.lock().unwrap().clone()
    }
}

async fn serve(
    stream: tokio::net::UnixStream,
    log: Arc<Mutex<Log>>,
    mut bump: tokio::sync::watch::Receiver<u64>,
    got: Arc<Mutex<Vec<String>>>,
) {
    let (r, mut w) = stream.into_split();
    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Value = serde_json::from_str(&line).unwrap();
        let method = req["method"].as_str().unwrap_or_default().to_string();
        got.lock().unwrap().push(method.clone());
        let reply = |result: Value| format!("{}\n", json!({"id": req["id"], "result": result}));
        let streaming = match method.as_str() {
            "control.subscribe" => {
                let head = log.lock().unwrap().control.len();
                Some((
                    StreamKind::Control,
                    0,
                    reply(json!({"since": 0, "head": head})),
                ))
            }
            "thread.subscribe" => {
                let since = req["params"]["since_seq"].as_u64().unwrap_or(0);
                let head = log.lock().unwrap().transcript.len();
                let ack = reply(json!({"since": since, "head": head}));
                if since > head as u64 {
                    // The probe for the head: it is dropped once read.
                    let _ = w.write_all(ack.as_bytes()).await;
                    return;
                }
                Some((StreamKind::Thread, since as usize, ack))
            }
            _ => None,
        };
        let Some((kind, mut sent, ack)) = streaming else {
            let result = match method.as_str() {
                "account.list" => json!({"runtimes": {}}),
                "orchestrator.show" => Value::Null,
                _ => json!({}),
            };
            if w.write_all(reply(result).as_bytes()).await.is_err() {
                return;
            }
            continue;
        };
        if w.write_all(ack.as_bytes()).await.is_err() {
            return;
        }
        loop {
            let items: Vec<String> = {
                let log = log.lock().unwrap();
                let events = match kind {
                    StreamKind::Control => &log.control,
                    _ => &log.transcript,
                };
                events[sent.min(events.len())..]
                    .iter()
                    .enumerate()
                    .map(|(k, event)| {
                        let gseq = (kind == StreamKind::Control).then_some((sent + k + 1) as u64);
                        let item = StreamItem {
                            stream: kind,
                            gseq,
                            event: event.clone(),
                        };
                        format!("{}\n", serde_json::to_string(&item).unwrap())
                    })
                    .collect()
            };
            sent += items.len();
            for item in items {
                if w.write_all(item.as_bytes()).await.is_err() {
                    return;
                }
            }
            if bump.changed().await.is_err() {
                return;
            }
        }
    }
}

/// A terminal for the test: [`TestBackend`], whose screen after each frame is kept as text
/// where the test can read it.
struct Watched {
    inner: TestBackend,
    screen: Arc<Mutex<String>>,
}

impl Backend for Watched {
    type Error = <TestBackend as Backend>::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.inner.draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.inner.show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<Position, Self::Error> {
        self.inner.get_cursor_position()
    }
    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.inner.clear()
    }
    fn clear_region(&mut self, clear_type: ClearType) -> Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }
    fn size(&self) -> Result<Size, Self::Error> {
        self.inner.size()
    }
    fn window_size(&mut self) -> Result<WindowSize, Self::Error> {
        self.inner.window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        let buf = self.inner.buffer();
        let mut text = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        *self.screen.lock().unwrap() = text;
        self.inner.flush()
    }
}

/// Waits until the screen says what `ok` looks for.
async fn until(screen: &Arc<Mutex<String>>, what: &str, ok: impl Fn(&str) -> bool) {
    let waited = tokio::time::timeout(Duration::from_secs(20), async {
        while !ok(&screen.lock().unwrap()) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        waited.is_ok(),
        "never saw {what}:\n{}",
        screen.lock().unwrap()
    );
}

/// The hub stops under a running TUI (its streams end), and another starts on the same
/// socket with the same log and more: the TUI says the connection is lost in a band no status
/// replaces, connects again, and shows the new hub's control items (a new thread's
/// permission) and transcript events (the orchestrator's reply); the band goes. A key press
/// meant for what showed before cannot answer the item shown again.
#[tokio::test]
async fn the_tui_reconnects_after_its_hub_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hub.sock");
    let mut orch = ThreadScript::new("t-orch");
    let mut created = orch.created("orchestrator");
    if let EventBody::ThreadCreated(p) = &mut created.body {
        p.ext = json!({"twr": {"role": "orchestrator"}})
            .as_object()
            .cloned();
    }
    let log = Arc::new(Mutex::new(Log {
        control: vec![created.clone()],
        transcript: vec![created, orch.text("before the restart")],
    }));
    let (bump, bumps) = tokio::sync::watch::channel(0u64);
    let first = Hub::start(&socket, log.clone(), bumps.clone());

    let session = Session::connect(&socket, None).await.unwrap();
    let screen = Arc::new(Mutex::new(String::new()));
    let mut terminal = ratatui::Terminal::new(Watched {
        inner: TestBackend::new(100, 30),
        screen: screen.clone(),
    })
    .unwrap();
    let (key_tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let mut input = Input { rx, thread: None };
    let (late_tx, late_rx) = tokio::sync::mpsc::unbounded_channel();
    let (_saved_tx, saved_rx) = tokio::sync::mpsc::unbounded_channel();
    let place = speaker::Place {
        remote: false,
        agent: false,
        socket_only: true,
    };
    let mut ctx = Shared {
        callers: Default::default(),
        all_callers: Vec::new(),
        late_tx: late_tx.clone(),
        late_rx,
        followed: Default::default(),
        tui: twr_core::config::TuiConfig {
            suggest: false,
            summaries: false,
            ..Default::default()
        },
        epoch: std::time::Instant::now(),
        workspaces: None,
        waiting_seen: Default::default(),
        poll_clock: Default::default(),
        keys: keys::Keys::default(),
        compact: Some(false),
        suggest: Some(false),
        sidebar_hidden: false,
        agent: None,
        saves: None,
        saved_rx,
        summaries: Some(false),
        seen: Default::default(),
        seen_file: None,
        offer: None,
        asking: Default::default(),
        summarizing: Default::default(),
        speaker: speaker::OwnSpeaker::start(None, place, late_tx),
        signals: Signals(Vec::new()),
    };
    let notifier = notify::Notifier::new(
        &twr_core::config::TuiNotifyConfig::default(),
        notify::Multiplexer::None,
    );
    let tui = show(
        &mut terminal,
        &mut input,
        notifier,
        &mut ctx,
        session,
        None,
        Carried::default(),
    );
    let script = async {
        until(&screen, "the reply before the restart", |s| {
            s.contains("before the restart")
        })
        .await;
        first.kill();
        until(&screen, "the band", |s| s.contains(HUB_LOST)).await;
        // While it is gone, the hub writes more: a worker asks for permission, the
        // orchestrator replies.
        {
            let mut log = log.lock().unwrap();
            let mut api = ThreadScript::new("t-api");
            log.control.push(api.created("api"));
            log.control.push(api.turn_started("work"));
            let mut raise = api.raise("p1", AttentionReason::Permission);
            if let EventBody::AttentionRaised(p) = &mut raise.body {
                p.summary = "Bash: cargo publish".into();
            }
            log.control.push(raise);
            log.transcript.push(orch.text("after the restart"));
        }
        // The band stays whatever else the status bar says.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(screen.lock().unwrap().contains(HUB_LOST));
        let second = Hub::start(&socket, log.clone(), bumps.clone());
        until(&screen, "the new hub's items and reply", |s| {
            s.contains("after the restart")
                && s.contains("before the restart")
                && s.contains("cargo publish")
                && !s.contains(HUB_LOST)
        })
        .await;
        // Keys pressed as the item shows again (select it, open its popup, allow) answer
        // nothing: it has not been on screen long enough to be read.
        let now = std::time::Instant::now;
        for code in [
            crossterm::event::KeyCode::Tab,
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyCode::Char('1'),
        ] {
            let k = crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
            key_tx.send((now(), Ok(TermEvent::Key(k)))).unwrap();
        }
        // Then the new hub's later events still arrive on the streams it opened.
        log.lock()
            .unwrap()
            .transcript
            .push(orch.text("and later still"));
        bump.send_modify(|n| *n += 1);
        until(&screen, "a later reply", |s| s.contains("and later still")).await;
        let got = second.got();
        assert!(got.iter().any(|m| m == "control.subscribe"), "{got:?}");
        assert!(got.iter().any(|m| m == "thread.subscribe"), "{got:?}");
        assert!(
            !got.iter().any(|m| m.contains("respond")),
            "a stale key answered the item: {got:?}"
        );
        // Ends the TUI, as a closed terminal does.
        drop(key_tx);
        second
    };
    let ((result, _writer), second) = tokio::join!(tui, script);
    result.unwrap();
    second.kill();
}

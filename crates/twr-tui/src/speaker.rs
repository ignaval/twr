//! The TUI's own speaker (`docs/read-aloud.md`, "Speakers"): on the machine you listen on, the
//! TUI reads aloud by itself, with no `twr speak` beside it.
//!
//! - **When.** [`speaks_by_itself`]: `[speech] local = "on"`, or `"auto"` with the TUI not
//!   under SSH or mosh; never in a thread's agent's TUI, nor in one opened on a hub's socket
//!   (`--socket`); and only with an engine to speak with.
//! - **What.** [`twr_speech::run`] with a local link over every workspace of the TUI's root, no
//!   keys (the TUI's keys are its own; Ctrl-R and `/read` go through the hub like any
//!   speaker's controls), named after this machine ([`crate::speech::host`]), its state in the
//!   root's data directory. The hub keeps one speaker per device, and a `twr speak` on this
//!   machine is the same device (the same state file): whichever attaches second stands by.
//! - **Where it speaks of itself.** Its notes go to the log (`tracing`), never to the terminal
//!   the TUI draws on. A failure to start (no engine with `local = "on"`, an unusable state
//!   file) is said once on the status bar.
//! - **How long.** Once per TUI process, across workspace switches; stopped as the TUI quits
//!   ([`OwnSpeaker::stop`]), or however it ends ([`OwnSpeaker`] dropped).

use tokio::sync::mpsc;
use twr_core::config::{SpeechConfig, SpeechLocal};
use twr_core::paths::Paths;
use twr_speech::{Engine, LinkSpec, SpeakerOpts, StateStore, Workspaces};

/// What the TUI's own speaker needs: the paths of the workspace the TUI opens on (the speaker
/// reads every workspace of its root, and keeps its state in the root's data directory), and
/// `[speech]`.
#[derive(Debug, Clone)]
pub struct LocalSpeech {
    pub paths: Paths,
    pub config: SpeechConfig,
}

/// Where the TUI runs, as far as its own speaker goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place {
    /// Over SSH or mosh: the terminal, and whoever listens, is on another machine.
    pub remote: bool,
    /// A thread's agent's TUI.
    pub agent: bool,
    /// Opened on a hub's socket (`--socket`): that hub's alone. The local link finds hubs by
    /// the root's workspace names, not by a socket path, so it could not be told to reach only
    /// that hub (it could be of another root, or none): such a TUI does not speak by itself.
    pub socket_only: bool,
}

/// Whether the TUI speaks by itself: `local` on, or auto and not remote; not an agent's TUI;
/// not opened on a socket; and `engine` (asked last, only when all else says yes) says an
/// engine is there.
pub fn speaks_by_itself(local: SpeechLocal, place: Place, engine: impl FnOnce() -> bool) -> bool {
    let wanted = match local {
        SpeechLocal::On => true,
        SpeechLocal::Auto => !place.remote,
        SpeechLocal::Off => false,
    };
    wanted && !place.agent && !place.socket_only && engine()
}

/// What the status bar is told: a message, and whether it is transient.
type Notices = mpsc::UnboundedSender<(String, bool)>;

/// The TUI's own speaker while it runs (none: `None`): aborted by [`OwnSpeaker::stop`], or
/// when dropped.
#[derive(Debug, Default)]
pub struct OwnSpeaker(Option<tokio::task::AbortHandle>);

impl OwnSpeaker {
    /// Starts the TUI's own speaker on the current runtime, if [`speaks_by_itself`] says so
    /// (`speech`: `None`, never). Whether it does is found out in the task, off the TUI's
    /// start: the engine's program is looked for on `PATH`. A failure to start is told on
    /// `notices`, once.
    pub fn start(speech: Option<LocalSpeech>, place: Place, notices: Notices) -> OwnSpeaker {
        Self::start_with(
            speech,
            place,
            notices,
            twr_speech::engine_available,
            |cfg| twr_speech::from_config(cfg, None),
        )
    }

    /// [`OwnSpeaker::start`] with how to tell whether there is an engine (`available`), and
    /// how to make it (`engine`).
    fn start_with(
        speech: Option<LocalSpeech>,
        place: Place,
        notices: Notices,
        available: impl FnOnce(&SpeechConfig) -> bool + Send + 'static,
        engine: impl FnOnce(&SpeechConfig) -> anyhow::Result<Box<dyn Engine>> + Send + 'static,
    ) -> OwnSpeaker {
        let Some(LocalSpeech { paths, config }) = speech else {
            return OwnSpeaker(None);
        };
        let say = move |msg: String| {
            let _ = notices.send((format!("read aloud: {msg}"), false));
        };
        let task = tokio::spawn(async move {
            let mut no_engine = false;
            let go = speaks_by_itself(config.local, place, || {
                no_engine = !available(&config);
                !no_engine
            });
            if !go {
                // Asked for by name: why it is silent. (`auto` with no engine stays quiet.)
                if no_engine && config.local == SpeechLocal::On {
                    say(format!(
                        "[speech] local = \"on\", but {}",
                        twr_speech::engine::NO_ENGINE
                    ));
                }
                return;
            }
            let engine = match engine(&config) {
                Ok(e) => e,
                Err(e) => return say(format!("{e:#}")),
            };
            let root = paths.root();
            let opts = SpeakerOpts {
                tty_keys_once_attached: false,
                name: crate::speech::host(),
                config,
                voice: None,
                rate: None,
                state: StateStore::in_data_dir(&root.data_dir),
                // The log: never the terminal the TUI draws on.
                report: None,
            };
            let link = LinkSpec::Local {
                paths: root,
                workspaces: Workspaces::All,
            };
            // Runs until aborted; it ends by itself only when it cannot start.
            let shutdown = std::future::pending::<()>();
            if let Err(e) = twr_speech::run(opts, link, engine, None, shutdown).await {
                say(format!("{e:#}"));
            }
        });
        OwnSpeaker(Some(task.abort_handle()))
    }

    /// Stops it (the TUI quits): its connections close, and what it was saying stops.
    pub fn stop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// The TUI gone, however it ended (an error, a panic unwinding): its speaker goes with it.
impl Drop for OwnSpeaker {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use twr_speech::engine::FakeEngine;

    const HERE: Place = Place {
        remote: false,
        agent: false,
        socket_only: false,
    };

    /// Every case: `local` × where (here, over ssh, over mosh: both remote) × an agent's TUI
    /// × `--socket` × an engine there.
    #[test]
    fn speaks_by_itself_only_where_it_should() {
        for local in [SpeechLocal::On, SpeechLocal::Off, SpeechLocal::Auto] {
            for (place, remote) in [("local", false), ("ssh", true), ("mosh", true)] {
                for agent in [false, true] {
                    for socket_only in [false, true] {
                        for engine in [false, true] {
                            let p = Place {
                                remote,
                                agent,
                                socket_only,
                            };
                            let want = match local {
                                SpeechLocal::On => !agent && !socket_only && engine,
                                SpeechLocal::Auto => {
                                    place == "local" && !agent && !socket_only && engine
                                }
                                SpeechLocal::Off => false,
                            };
                            assert_eq!(
                                speaks_by_itself(local, p, || engine),
                                want,
                                "{local:?} {place} agent={agent} socket={socket_only} \
                                 engine={engine}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The engine is looked for only once everything else says yes.
    #[test]
    fn the_engine_is_looked_for_last() {
        let mut asked = false;
        let off = Place {
            agent: true,
            ..HERE
        };
        assert!(!speaks_by_itself(SpeechLocal::On, off, || {
            asked = true;
            true
        }));
        assert!(!asked);
        assert!(!speaks_by_itself(SpeechLocal::Off, HERE, || {
            asked = true;
            true
        }));
        assert!(!asked);
        assert!(speaks_by_itself(SpeechLocal::Auto, HERE, || {
            asked = true;
            true
        }));
        assert!(asked);
    }

    fn fake(_: &SpeechConfig) -> anyhow::Result<Box<dyn Engine>> {
        Ok(Box::new(FakeEngine::manual().0))
    }

    async fn next<R: tokio::io::AsyncRead + Unpin>(
        lines: &mut tokio::io::Lines<BufReader<R>>,
    ) -> Option<Value> {
        let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .expect("the speaker said nothing")
            .unwrap()?;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// The speaker runs: it greets this root's `default` hub as a speaker named after this
    /// machine, and attaches; stopped (the TUI quit) its connection closes. Dropped, the same.
    #[tokio::test]
    async fn the_own_speaker_attaches_and_is_stopped_as_the_tui_quits() {
        for by_drop in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::with_home(dir.path());
            let listener = tokio::net::UnixListener::bind(&paths.socket).unwrap();
            let (tx, mut rx) = mpsc::unbounded_channel();
            let speech = LocalSpeech {
                paths: paths.clone(),
                config: SpeechConfig::default(),
            };
            let mut speaker = OwnSpeaker::start_with(Some(speech), HERE, tx, |_| true, fake);
            let (stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .expect("the speaker never connected")
                .unwrap();
            let (r, mut w) = stream.into_split();
            let mut lines = BufReader::new(r).lines();
            let hello = next(&mut lines).await.unwrap();
            assert_eq!(hello["method"], "hello");
            assert_eq!(hello["params"]["client"], "speak");
            let answer = json!({"id": hello["id"], "result": {"client": "speak", "identity": "user", "workspace": "default"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            let attach = next(&mut lines).await.unwrap();
            assert_eq!(attach["method"], "speech.attach");
            assert_eq!(attach["params"]["name"], crate::speech::host());
            assert_eq!(attach["params"]["engine"], "fake");
            if by_drop {
                drop(speaker);
            } else {
                speaker.stop();
                assert!(speaker.0.is_none());
            }
            // Aborted, its connection closes: the hub reads the end of it.
            assert_eq!(next(&mut lines).await, None);
            assert!(rx.try_recv().is_err(), "nothing said on the status bar");
        }
    }

    /// No `[speech]` given, or not wanted: nothing runs; `local = "on"` with no engine says why
    /// once; an engine that fails to start says its error.
    #[tokio::test]
    async fn a_silent_own_speaker_says_why_only_when_asked_for() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(OwnSpeaker::start(None, HERE, tx.clone()).0.is_none());
        let dir = tempfile::tempdir().unwrap();
        let speech = |local| LocalSpeech {
            paths: Paths::with_home(dir.path()),
            config: SpeechConfig {
                local,
                ..SpeechConfig::default()
            },
        };
        let wait = |s: &OwnSpeaker| {
            let h = s.0.clone().unwrap();
            async move {
                while !h.is_finished() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            }
        };
        // `auto` with no engine: quiet.
        let s = OwnSpeaker::start_with(
            Some(speech(SpeechLocal::Auto)),
            HERE,
            tx.clone(),
            |_| false,
            fake,
        );
        wait(&s).await;
        assert!(rx.try_recv().is_err());
        // `on` with no engine: said.
        let s = OwnSpeaker::start_with(
            Some(speech(SpeechLocal::On)),
            HERE,
            tx.clone(),
            |_| false,
            fake,
        );
        wait(&s).await;
        let (msg, transient) = rx.try_recv().unwrap();
        assert!(
            msg.starts_with("read aloud: [speech] local = \"on\", but no speech engine"),
            "{msg}"
        );
        assert!(!transient);
        // An agent's TUI with `on`: quiet, the engine never looked for.
        let agent = Place {
            agent: true,
            ..HERE
        };
        let s = OwnSpeaker::start_with(
            Some(speech(SpeechLocal::On)),
            agent,
            tx.clone(),
            |_| panic!("looked for an engine"),
            fake,
        );
        wait(&s).await;
        assert!(rx.try_recv().is_err());
        // An engine that can't be made: its error.
        let s = OwnSpeaker::start_with(
            Some(speech(SpeechLocal::On)),
            HERE,
            tx,
            |_| true,
            |_| anyhow::bail!("spd-say is gone"),
        );
        wait(&s).await;
        assert_eq!(rx.try_recv().unwrap().0, "read aloud: spd-say is gone");
    }
}

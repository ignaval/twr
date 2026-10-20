//! Helpers for building event sequences in tests (here and in downstream crates).
//!
//! [`ThreadScript`] stamps envelopes the way the hub does: per-thread `seq`, the current turn on
//! turn-scoped events, and a deterministic clock (one second per event from a base time) so
//! age ordering is reproducible.

use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;

use crate::event::*;

/// Base instant of the deterministic test clock (2026-01-01T00:00:00Z).
pub fn base_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
}

/// Builds a thread's events with hub-like envelopes.
#[derive(Debug, Clone)]
pub struct ThreadScript {
    thread: String,
    seq: u64,
    turns: u64,
    open_turn: Option<u64>,
    clock: i64,
    source: Source,
}

impl ThreadScript {
    /// A script for `thread`, clock at [`base_time`].
    pub fn new(thread: impl Into<String>) -> Self {
        ThreadScript {
            thread: thread.into(),
            seq: 0,
            turns: 0,
            open_turn: None,
            clock: 0,
            source: Source::Adapter("fake".into()),
        }
    }

    /// Sets the clock to `secs` seconds after [`base_time`] for the next event.
    pub fn at(&mut self, secs: i64) -> &mut Self {
        self.clock = secs;
        self
    }

    /// Stamps an arbitrary body.
    pub fn event(&mut self, body: EventBody) -> Event {
        self.seq += 1;
        let turn = match &body {
            EventBody::AgentText(_)
            | EventBody::AgentToolCall(_)
            | EventBody::AgentToolResult(_)
            | EventBody::AgentThinking(_)
            | EventBody::TurnStarted(_)
            | EventBody::TurnEnded(_)
            | EventBody::TurnWithdrawn(_)
            | EventBody::TurnInterrupted(_)
            | EventBody::AttentionRaised(_) => self.open_turn,
            _ => None,
        };
        let mut ev = Event::new(
            self.thread.clone(),
            self.seq,
            turn,
            self.source.clone(),
            body,
        );
        ev.ts = base_time() + chrono::Duration::seconds(self.clock);
        self.clock += 1;
        ev
    }

    /// `thread.created` with the given name.
    pub fn created(&mut self, name: &str) -> Event {
        self.event(EventBody::ThreadCreated(ThreadCreated {
            name: name.into(),
            project: Some("proj".into()),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ..Default::default()
        }))
    }

    /// `thread.created` with aliases.
    pub fn created_with_aliases(&mut self, name: &str, aliases: &[&str]) -> Event {
        let mut ev = self.created(name);
        if let EventBody::ThreadCreated(p) = &mut ev.body {
            p.aliases = aliases.iter().map(|a| a.to_string()).collect();
        }
        ev
    }

    /// `thread.started`.
    pub fn started(&mut self, session: &str) -> Event {
        self.event(EventBody::ThreadStarted(session_payload(session)))
    }

    /// `thread.parked`.
    pub fn parked(&mut self) -> Event {
        self.event(EventBody::ThreadParked(SessionPayload::default()))
    }

    /// `thread.resumed`.
    pub fn resumed(&mut self, session: &str) -> Event {
        self.event(EventBody::ThreadResumed(session_payload(session)))
    }

    /// `thread.stopped`.
    pub fn stopped(&mut self) -> Event {
        self.event(EventBody::ThreadStopped(SessionPayload::default()))
    }

    /// `thread.crashed`.
    pub fn crashed(&mut self, code: i32) -> Event {
        self.event(EventBody::ThreadCrashed(ThreadCrashed {
            exit_code: Some(code),
            stderr_tail: "boom".into(),
            ..Default::default()
        }))
    }

    /// `thread.attached`.
    pub fn attached(&mut self) -> Event {
        self.event(EventBody::ThreadAttached(ClientPayload {
            client: "cli".into(),
            ..Default::default()
        }))
    }

    /// `thread.detached`.
    pub fn detached(&mut self) -> Event {
        self.event(EventBody::ThreadDetached(ClientPayload {
            client: "cli".into(),
            ..Default::default()
        }))
    }

    /// `turn.started`; opens the next turn.
    pub fn turn_started(&mut self, prompt: &str) -> Event {
        self.turns += 1;
        self.open_turn = Some(self.turns);
        self.event(EventBody::TurnStarted(TurnStarted {
            prompt: prompt.into(),
            ..Default::default()
        }))
    }

    /// `turn.started` for an automatic retry of turn `retry_of`.
    pub fn retry_turn_started(&mut self, retry_of: u64) -> Event {
        let mut ev = self.turn_started("continue");
        if let EventBody::TurnStarted(p) = &mut ev.body {
            let ext = json!({"twr": {"retry_of": retry_of}});
            p.ext = ext.as_object().cloned();
        }
        ev
    }

    /// `turn.ended`; closes the open turn.
    pub fn turn_ended(&mut self, result: TurnResult) -> Event {
        let ev = self.event(EventBody::TurnEnded(TurnEnded {
            result,
            ..Default::default()
        }));
        self.open_turn = None;
        ev
    }

    /// A final `agent.text`.
    pub fn text(&mut self, text: &str) -> Event {
        self.event(EventBody::AgentText(AgentText {
            text: text.into(),
            is_final: true,
            ..Default::default()
        }))
    }

    /// `attention.raised`.
    pub fn raise(&mut self, attn_id: &str, reason: AttentionReason) -> Event {
        self.event(EventBody::AttentionRaised(AttentionRaised {
            attn_id: attn_id.into(),
            reason,
            summary: format!("{reason:?} {attn_id}"),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        }))
    }

    /// `attention.annotated`.
    pub fn annotate(&mut self, attn_id: &str, note: &str) -> Event {
        self.event(EventBody::AttentionAnnotated(AttentionAnnotated {
            attn_id: attn_id.into(),
            note: note.into(),
            ..Default::default()
        }))
    }

    /// `attention.resolved` by the user.
    pub fn resolve(&mut self, attn_id: &str) -> Event {
        self.event(EventBody::AttentionResolved(AttentionResolved {
            attn_id: attn_id.into(),
            by: ResolvedBy::User,
            answer: Some(Answer::allow()),
            ..Default::default()
        }))
    }

    /// `report.ready` for the given turn.
    pub fn report(&mut self, turn: Option<u64>, grade: Grade) -> Event {
        self.event(EventBody::ReportReady(ReportReady {
            summary: "Did the thing.".into(),
            grade,
            turn,
            written_by: "hub".into(),
            ..Default::default()
        }))
    }

    /// `git.snapshot`.
    pub fn git(&mut self, branch: &str, dirty: u64) -> Event {
        self.event(EventBody::GitSnapshot(GitSnapshot {
            branch: Some(branch.into()),
            dirty_files: dirty,
            ..Default::default()
        }))
    }
}

fn session_payload(session: &str) -> SessionPayload {
    SessionPayload {
        session_id: Some(session.into()),
        ..Default::default()
    }
}

/// Test git repositories. Every command runs with the repository's own identity, signing off
/// and hooks disabled, all set in the repository's local config (the user's global git config
/// is never touched).
pub mod git {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// Runs `git -C <dir> <args>` and returns its trimmed stdout; panics on failure.
    /// Whether the `git` on `PATH` has what merge readiness finds conflicts with: its
    /// `--attr-source` option (newer than `merge-tree --write-tree`), probed.
    pub fn merge_tree_writes() -> bool {
        Command::new("git")
            .args(["--attr-source=HEAD", "version"])
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Runs `git -C <dir> <args>` and returns its trimmed stdout; panics on failure.
    pub fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Creates a repository at `dir` (created if missing) on branch `main` with one commit
    /// (`README.md`), and returns its canonical path.
    pub fn init_repo(dir: &Path) -> PathBuf {
        std::fs::create_dir_all(dir).expect("creating the repo dir");
        run(dir, &["init", "-q", "-b", "main"]);
        let hooks = dir.join(".git").join("no-hooks");
        std::fs::create_dir_all(&hooks).expect("creating the hooks dir");
        for (k, v) in [
            ("user.name", "twr test"),
            ("user.email", "twr-test@example.invalid"),
            ("commit.gpgsign", "false"),
            ("tag.gpgsign", "false"),
            ("core.hooksPath", hooks.to_str().expect("utf-8 path")),
        ] {
            run(dir, &["config", "--local", k, v]);
        }
        std::fs::write(dir.join("README.md"), "test repo\n").expect("writing README");
        commit_all(dir, "initial commit");
        std::fs::canonicalize(dir).expect("canonical repo path")
    }

    /// Stages everything in `dir` and commits it.
    pub fn commit_all(dir: &Path, message: &str) {
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", message]);
    }

    /// Whether branch `name` exists in `repo`.
    pub fn branch_exists(repo: &Path, name: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "--verify", "--quiet"])
            .arg(format!("refs/heads/{name}"))
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// The worktree paths `git worktree list` knows for `repo` (canonical, main one first).
    pub fn worktrees(repo: &Path) -> Vec<PathBuf> {
        run(repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(PathBuf::from)
            .collect()
    }

    /// A git executable that runs the real `git`, except that it holds every
    /// `git worktree remove` (the last command of a removal, once its safety checks passed) and
    /// every `git update-ref` (the first change of a move to the trash, which keeps the
    /// thread's commits under `refs/twr-trash/`, and of a purge or restore) until
    /// [`HeldGit::release`]: it says so in [`HeldGit::held`] first, and in
    /// [`HeldGit::finished`] once the real command has run.
    pub struct HeldGit {
        /// The executable (give it to `LocalGit::with_bin`).
        pub bin: PathBuf,
        dir: PathBuf,
    }

    impl HeldGit {
        /// Installs it in `dir` (created if missing).
        pub fn install(dir: &Path) -> HeldGit {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(dir).expect("creating the held git's dir");
            let (held, go, done) = (dir.join("held"), dir.join("go"), dir.join("done"));
            let script = format!(
                "#!/bin/sh\nprev=\nhold=0\nfor a in \"$@\"; do\n  \
                 if [ \"$prev\" = worktree ] && [ \"$a\" = remove ]; then hold=1; fi\n  \
                 if [ \"$a\" = update-ref ]; then hold=1; fi\n  \
                 prev=\"$a\"\ndone\nif [ $hold = 1 ]; then\n  touch '{held}'\n  \
                 while [ ! -e '{go}' ]; do sleep 0.02; done\n  git \"$@\"\n  st=$?\n  \
                 touch '{done}'\n  exit $st\nfi\nexec git \"$@\"\n",
                held = held.display(),
                go = go.display(),
                done = done.display(),
            );
            // Written by `cp`, not by this process: a child this (multi-threaded) process
            // forks meanwhile would inherit a descriptor open for writing, and exec'ing the
            // file would fail with ETXTBSY.
            let (src, bin) = (dir.join("git-held.src"), dir.join("git-held"));
            std::fs::write(&src, script).expect("writing the held git");
            let st = Command::new("cp")
                .arg(&src)
                .arg(&bin)
                .status()
                .expect("running cp");
            assert!(st.success(), "cp failed");
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("making the held git executable");
            HeldGit {
                bin,
                dir: dir.to_path_buf(),
            }
        }

        /// Whether a `git worktree remove` is being held.
        pub fn held(&self) -> bool {
            self.dir.join("held").exists()
        }

        /// Lets held (and later) `git worktree remove`s run.
        pub fn release(&self) {
            std::fs::write(self.dir.join("go"), "").expect("releasing the held git");
        }

        /// Whether a held `git worktree remove` has run.
        pub fn finished(&self) -> bool {
            self.dir.join("done").exists()
        }
    }
}

/// One of each value the hub sends a runner it drains ([`crate::runner::DRAINABLE_PROTOCOLS`]),
/// named: the challenge, `authenticated`, welcomes, acks, each error code, ping, pong, and
/// `cmd` with each drain-safe op ([`crate::runner::Op::drain_safe`]: `send`, `respond` with
/// each shape of answer the hub forwards, `stop` with and without a run), with command ids
/// as the hub makes them. `docs/tools/capture-runner-fixtures.sh` has an older revision's
/// decoder read them (`runner-protocol-<version>/hub-accepts.json`), and
/// `tests/runner_protocol_fixtures.rs` checks that they still are what it accepted.
pub fn outbound_samples() -> Vec<(&'static str, crate::runner::HubMsg)> {
    use crate::runner::{hub_errors, HubMsg, Op};
    let nonce = "0f1e2d3c4b5a69788796a5b4c3d2e1f000112233445566778899aabbccddeeff".to_string();
    let proof = "3c5da0337b61ca60ddf4feb6bb294479b16c2308094cf65466ddf9d5fc0fdbe0".to_string();
    let cmd = |cmd_id: &str, op: Op| HubMsg::Cmd {
        cmd_id: cmd_id.into(),
        op,
    };
    let respond = |cmd_id: &str, answer: Answer, run: Option<&str>| {
        cmd(
            cmd_id,
            Op::Respond {
                thread: "t1".into(),
                attn_id: "toolu_01".into(),
                answer,
                run: run.map(str::to_string),
            },
        )
    };
    let mut questions = JsonMap::new();
    questions.insert(
        "answers".into(),
        json!({"Which database?": "Postgres", "Migrate now?": "Yes"}),
    );
    let error = |code: &str, message: &str| HubMsg::Error {
        code: code.into(),
        message: message.into(),
    };
    let ulid = "01JZ8Q7W5M3K2N1P0R9S8T7V6X";
    vec![
        ("challenge", HubMsg::Challenge { nonce, proof }),
        ("authenticated", HubMsg::Authenticated),
        (
            "welcome",
            HubMsg::Welcome {
                epoch: 3,
                watermarks: [("t1".to_string(), 5), ("t2".to_string(), 1)].into(),
            },
        ),
        (
            "welcome without watermarks",
            HubMsg::Welcome {
                epoch: 1,
                watermarks: Default::default(),
            },
        ),
        (
            "ack",
            HubMsg::Ack {
                thread: "t1".into(),
                rseq: 6,
            },
        ),
        (
            "error unauthorized",
            error(hub_errors::UNAUTHORIZED, "the box's token was revoked"),
        ),
        (
            "error protocol",
            error(
                hub_errors::PROTOCOL,
                "the answer to the challenge must be auth",
            ),
        ),
        (
            "error fenced",
            error(
                hub_errors::FENCED,
                "a newer connection of box gpu replaced this one",
            ),
        ),
        (
            "error exhausted",
            error(hub_errors::EXHAUSTED, "no more connection epochs"),
        ),
        ("error without message", error(hub_errors::FENCED, "")),
        ("ping", HubMsg::Ping),
        ("pong", HubMsg::Pong),
        (
            "cmd send",
            cmd(
                "send:t1:3",
                Op::Send {
                    thread: "t1".into(),
                    turn: 3,
                    text: "Now run the tests.\nThen commit.".into(),
                    run: Some(ulid.into()),
                },
            ),
        ),
        (
            "cmd send without run",
            cmd(
                "send:t1:4",
                Op::Send {
                    thread: "t1".into(),
                    turn: 4,
                    text: "go on".into(),
                    run: None,
                },
            ),
        ),
        (
            "cmd respond allow",
            respond(&format!("respond:2:{ulid}"), Answer::allow(), Some(ulid)),
        ),
        (
            "cmd respond deny with message",
            respond(
                &format!("respond:2:{ulid}"),
                Answer {
                    message: Some("Not on main; use a branch.".into()),
                    ..Answer::deny()
                },
                Some(ulid),
            ),
        ),
        (
            "cmd respond option",
            respond(
                &format!("respond:2:{ulid}"),
                Answer::new("Yes, and don't ask again this session"),
                Some(ulid),
            ),
        ),
        (
            "cmd respond cancel",
            respond(
                &format!("respond:2:{ulid}"),
                Answer::new("Cancel"),
                Some(ulid),
            ),
        ),
        (
            "cmd respond free text",
            respond(
                &format!("respond:2:{ulid}"),
                Answer {
                    message: Some("Use the staging database.".into()),
                    ..Answer::new("")
                },
                Some(ulid),
            ),
        ),
        (
            "cmd respond answers",
            respond(
                &format!("respond:2:{ulid}"),
                Answer {
                    extra: questions,
                    ..Answer::allow()
                },
                Some(ulid),
            ),
        ),
        (
            "cmd respond without run",
            respond(&format!("respond:{ulid}"), Answer::allow(), None),
        ),
        (
            "cmd stop",
            cmd(
                "stop:t1:2",
                Op::Stop {
                    thread: "t1".into(),
                    run: Some(ulid.into()),
                },
            ),
        ),
        (
            "cmd stop without run",
            cmd(
                ulid,
                Op::Stop {
                    thread: "t1".into(),
                    run: None,
                },
            ),
        ),
    ]
}

/// What a [`ScriptedHub`] does with one connection, after it answered `hello`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubScript {
    /// Closes it as a stopping hub does: says why (request 0, `stopping`), then hangs up.
    Stop,
    /// Reads the next request, then hangs up without answering it (a hub that crashed).
    Crash,
    /// Answers every request with `{}`.
    Serve,
}

/// A stand-in hub on a Unix socket that serves its connections in order, the i-th as
/// `scripts[i]` (later ones [`HubScript::Serve`]), and records the methods each one got.
#[derive(Debug, Clone)]
pub struct ScriptedHub {
    /// The methods each connection got, in order of connection.
    pub got: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    hung_up: std::sync::Arc<tokio::sync::Notify>,
}

impl ScriptedHub {
    /// Listens on `socket` (which must not exist).
    pub fn start(socket: &std::path::Path, scripts: Vec<HubScript>) -> ScriptedHub {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let hub = ScriptedHub {
            got: Default::default(),
            hung_up: Default::default(),
        };
        let (got, hung_up) = (hub.got.clone(), hub.hung_up.clone());
        tokio::spawn(async move {
            for n in 0.. {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let script = scripts.get(n).copied().unwrap_or(HubScript::Serve);
                got.lock().unwrap().push(Vec::new());
                let (got, hung_up) = (got.clone(), hung_up.clone());
                tokio::spawn(async move {
                    let (r, mut w) = stream.into_split();
                    let mut lines = tokio::io::BufReader::new(r).lines();
                    let reply = |id: &serde_json::Value, body: serde_json::Value| {
                        let mut r = json!({ "id": id });
                        r.as_object_mut()
                            .unwrap()
                            .extend(body.as_object().unwrap().clone());
                        format!("{r}\n")
                    };
                    while let Ok(Some(line)) = lines.next_line().await {
                        let req: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let method = req["method"].as_str().unwrap_or_default().to_string();
                        got.lock().unwrap()[n].push(method.clone());
                        let answer = match (method == "hello", script) {
                            (true, _) | (false, HubScript::Serve) => json!({"result": {}}),
                            (false, _) => break,
                        };
                        if w.write_all(reply(&req["id"], answer).as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                        if method == "hello" && script == HubScript::Stop {
                            let stopping = json!({"error": {
                                "code": crate::proto::codes::STOPPING,
                                "message": "the hub is stopping",
                            }});
                            let _ = w.write_all(reply(&json!(0), stopping).as_bytes()).await;
                            break;
                        }
                    }
                    drop(w);
                    hung_up.notify_one();
                });
            }
        });
        hub
    }

    /// Waits until a connection's script hung up on it.
    pub async fn hung_up(&self) {
        self.hung_up.notified().await;
    }

    /// The methods each connection got so far.
    pub fn got(&self) -> Vec<Vec<String>> {
        self.got.lock().unwrap().clone()
    }
}

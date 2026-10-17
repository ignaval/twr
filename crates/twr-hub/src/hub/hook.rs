//! The notification hook (`[notify]`): the hub runs a command of the user's when something
//! needs them, whether or not a TUI is open.
//!
//! What deserves a notification is [`twr_core::notify`]'s, as for the TUI's terminal alerts:
//! the [`Policy`] looks at every live control event right after it is folded
//! ([`Core::feed_hook`]), and what it gives goes to [`hook_task`], which holds it
//! ([`Held`]), checks it again ([`still_due`]) and, at most every
//! [`GAP`](twr_core::notify::GAP), runs the command once for everything due together
//! ([`digest`]). An item answered meanwhile never notifies. While
//! a TUI says the user looks at it (`notify.looking`, sent while it has focus and a key in the
//! last 30 s), what is due is dropped: they see it there.
//!
//! The command runs without a shell, in its own session and process group (no controlling
//! terminal), with the hub's environment less twr's secrets, and is ended after
//! `timeout_secs`. `{title}`, `{body}` and `{workspace}` in its arguments are filled in; its
//! stdin is one JSON object:
//! `{title, body, workspace, items: [{event, thread, name, turn, item, text?}]}`. Without
//! `detail`, neither carries what was asked or reported, only who and what kind of thing.
//! One runs at a time; what comes meanwhile waits for it, and the command's whole process
//! group is ended once it exits, times out, or the hub stops. Its processes carry the agent
//! marker [`HOOK_MARKER`]. A failure is logged, not retried.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};
use twr_core::event::Event;
use twr_core::notify::{digest, still_due, Candidate, Held, Policy};

use super::subscribe::wait_state;
use super::{Core, HubState, Inner};

/// How long a TUI's `notify.looking` holds: it says it again every 10 s while it lasts.
pub const LOOKING_FOR: Duration = Duration::from_secs(25);

/// The `TWR_AGENT` marker of the command's processes: an agent of no thread.
pub const HOOK_MARKER: &str = "notify-hook:hub";

/// While a command runs, how often what falls due is checked against a TUI being looked at.
const BUSY_CHECK: Duration = Duration::from_millis(500);

/// Longest stderr of the command kept for the log, in bytes.
const STDERR_MAX: usize = 4096;

/// What feeds the hook from the commit path: the policy and the dispatcher's channel.
pub(super) struct HookFeed {
    policy: Policy,
    tx: mpsc::UnboundedSender<Candidate>,
}

impl HookFeed {
    /// A feed for `config`'s events, with the receiver [`hook_task`] reads; `None` without a
    /// command.
    pub(super) fn new(
        config: &twr_core::config::NotifyConfig,
    ) -> Option<(HookFeed, mpsc::UnboundedReceiver<Candidate>)> {
        if config.command.is_empty() {
            return None;
        }
        let (tx, rx) = mpsc::unbounded_channel();
        Some((
            HookFeed {
                policy: Policy::new(&config.events),
                tx,
            },
            rx,
        ))
    }
}

impl Core {
    /// Live control event `ev`, committed at `gseq` and folded: what it gives the user to know
    /// goes to the hook.
    pub(super) fn feed_hook(&mut self, ev: &Event, gseq: u64) {
        let Some(feed) = self.hook.as_mut() else {
            return;
        };
        if let Some(c) = feed
            .policy
            .observe(ev, Some(gseq), &self.registry, &self.attention)
        {
            let _ = feed.tx.send(c);
        }
    }

    /// Control event `ev` of the replay at startup, at `gseq` and folded: the policy remembers
    /// what later events need (a report already known, an orchestrator turn a user started),
    /// and nothing goes to the hook.
    pub(super) fn recall_hook(&mut self, ev: &Event, gseq: u64) {
        let Some(feed) = self.hook.as_mut() else {
            return;
        };
        feed.policy
            .recall(ev, Some(gseq), &self.registry, &self.attention);
    }
}

impl Inner {
    /// Whether a TUI said, within [`LOOKING_FOR`], that the user looks at it.
    fn looked_at(&self, now: Instant) -> bool {
        self.looking_until
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|until| now < *until)
    }
}

impl super::Hub {
    /// `notify.looking {looking}` on connection `conn`: a user's TUI says whether the user
    /// looks at it now. Each connection's says so for itself; a closed one says no more.
    pub fn set_looking(&self, conn: u64, looking: bool) {
        let mut leases = self
            .inner
            .looking_until
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if looking {
            leases.insert(conn, Instant::now() + LOOKING_FOR);
        } else {
            leases.remove(&conn);
        }
    }
}

/// Holds what the policy gives and runs the command for what is due (see the module docs),
/// until the hub stops: a command still running then is ended, its group with it.
pub(super) async fn hook_task(
    hub: Weak<Inner>,
    mut rx: mpsc::UnboundedReceiver<Candidate>,
    mut state: watch::Receiver<HubState>,
    mut stop: watch::Receiver<bool>,
) {
    let mut held = Held::default();
    // While a command runs, when what falls due is next checked: set when it starts and after
    // each check, never moved by what arrives meanwhile.
    let mut busy_check = Instant::now();
    let mut last_sent: Option<Instant> = None;
    // The delivery running, and how to end it.
    let mut running: Option<(tokio::task::JoinHandle<()>, watch::Sender<bool>)> = None;
    loop {
        // While a command runs, what comes is held, revised or covered, and what falls due
        // while a TUI is looked at is still dropped (checked every [`BUSY_CHECK`]); the rest
        // goes out after it.
        let due_at = held.next_due(last_sent).map(|at| match running {
            Some(_) => at.max(busy_check),
            None => at,
        });
        tokio::select! {
            biased;
            _ = async {
                tokio::select! {
                    _ = wait_state(&mut state, |s| *s != HubState::Running) => {}
                    _ = stop.wait_for(|s| *s) => {}
                }
            } => {
                if let Some((task, cancel)) = running.take() {
                    let _ = cancel.send(true);
                    let _ = task.await;
                }
                return;
            }
            _ = async { running.as_mut().map(|(t, _)| t).unwrap().await }, if running.is_some() => {
                running = None;
            }
            // A check that is due goes before what keeps arriving.
            _ = sleep_until_opt(due_at) => {
                let Some(hub) = hub.upgrade() else {
                    return;
                };
                let (due, now) = {
                    // What came before this state, under its lock (candidates are sent under
                    // it): a revision, or what covers a held one, is reconciled first, and
                    // held from now, not from before the lock was had.
                    let core = hub.core();
                    let now = Instant::now();
                    while let Ok(c) = rx.try_recv() {
                        held.add(c, now);
                    }
                    let looked_at = hub.looked_at(now);
                    busy_check = now + BUSY_CHECK;
                    if running.is_some() && !looked_at {
                        // Not dropped, not sent yet: after the command.
                        continue;
                    }
                    let due: Vec<Candidate> = held
                        .take_due(now)
                        .into_iter()
                        .filter(|c| !looked_at && still_due(c, &core.registry, &core.attention))
                        .collect();
                    (due, now)
                };
                if due.is_empty() {
                    continue;
                }
                last_sent = Some(now);
                let (cancel, cancelled) = watch::channel(false);
                running = Some((tokio::spawn(deliver(hub, due, cancelled)), cancel));
            }
            c = rx.recv() => match c {
                Some(c) => held.add(c, Instant::now()),
                None => return,
            },
        }
    }
}

async fn sleep_until_opt(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// `arg` with `{title}`, `{body}` and `{workspace}` filled in, in one pass (a value holding
/// `{body}` stays as it is).
fn fill(arg: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(arg.len());
    let mut rest = arg;
    'scan: while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open..];
        for (name, value) in vars {
            let key = format!("{{{name}}}");
            if after.starts_with(&key) {
                out.push_str(value);
                rest = &after[key.len()..];
                continue 'scan;
            }
        }
        out.push('{');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

/// The notification for `due` (most urgent first): its title, body and stdin.
pub(super) fn message(
    mut due: Vec<Candidate>,
    detail: bool,
    workspace: &str,
) -> Option<(String, String, Value)> {
    if !detail {
        for c in &mut due {
            c.text.clear();
        }
    }
    let (title, body) = digest(&due)?;
    due.sort_by_key(|c| c.event);
    let items: Vec<Value> = due
        .iter()
        .map(|c| {
            let mut item = json!({
                "event": c.event,
                "thread": c.thread,
                "name": c.who,
                "turn": c.turn,
                "item": c.attn_id,
            });
            if detail {
                item["text"] = json!(c.text);
            }
            item
        })
        .collect();
    let input = json!({
        "title": title,
        "body": body,
        "workspace": workspace,
        "items": items,
    });
    Some((title, body, input))
}

/// Runs the command for `due` and waits for it, at most `timeout_secs`, or until
/// `cancelled` (the hub stops); its process group is ended in every case ([`run`]).
async fn deliver(hub: Arc<Inner>, due: Vec<Candidate>, cancelled: watch::Receiver<bool>) {
    let cfg = &hub.config.notify;
    let workspace = hub
        .workspace
        .clone()
        .unwrap_or_else(|| twr_core::paths::DEFAULT_WORKSPACE.to_string());
    let Some((title, body, input)) = message(due, cfg.detail, &workspace) else {
        return;
    };
    let vars = [
        ("title", title.as_str()),
        ("body", body.as_str()),
        ("workspace", workspace.as_str()),
    ];
    let argv: Vec<String> = cfg.command.iter().map(|a| fill(a, &vars)).collect();
    let timeout = Duration::from_secs(cfg.timeout_secs);
    let Some((ended, err)) = run(&argv, format!("{input}\n"), timeout, cancelled).await else {
        return;
    };
    match ended {
        Ended::Exited(Ok(status)) if status.success() => {}
        Ended::Exited(Ok(status)) => tracing::warn!(
            program = %argv[0],
            %status,
            stderr = %err.trim(),
            "the notify command failed"
        ),
        Ended::Exited(Err(e)) => {
            tracing::warn!(program = %argv[0], error = %e, "cannot wait for the notify command")
        }
        Ended::TimedOut => tracing::warn!(
            program = %argv[0],
            secs = cfg.timeout_secs,
            "the notify command timed out; ended it"
        ),
        Ended::Cancelled => {
            tracing::info!(program = %argv[0], "the hub stopped; ended the notify command")
        }
    }
}

/// Runs `argv` (see the module docs) with `input` on its stdin and waits for it, at most
/// `timeout`, or until `cancelled`; its process group is ended in every case. Returns how it
/// ended and its stderr (capped); `None` when it could not be started (logged).
///
/// Its exit is watched apart from its input: a command that exits without reading all of it
/// while a process it left running holds its stdin unread (the pipe full, the write blocked)
/// has exited all the same. The write is then dropped, which closes the hub's end of the
/// pipe, and what is left of its group is ended at once.
async fn run(
    argv: &[String],
    input: String,
    timeout: Duration,
    mut cancelled: watch::Receiver<bool>,
) -> Option<(Ended, String)> {
    let mut cmd = tokio::process::Command::new(&argv[0]);
    cmd.args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(false);
    twr_adapters::process::new_session(&mut cmd);
    twr_adapters::process::scrub(&mut cmd);
    cmd.env_remove(twr_runner::reap::RUNTIME_GEN_ENV);
    // Its processes, and a `twr` they run, are an agent of no thread: they read the hub, and
    // change nothing in it, also once one of them has left the hub's process tree.
    cmd.env(super::AGENT_ENV, HOOK_MARKER);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(program = %argv[0], error = %e, "cannot run the notify command");
            return None;
        }
    };
    let Some(pid) = child.id() else {
        let _ = child.wait().await;
        return None;
    };
    // Its stderr, kept (capped) for the log, read on the side: a member of its group can hold
    // it open after the command exited.
    let err = Arc::new(std::sync::Mutex::new(Vec::new()));
    let reader = child.stderr.take().map(|mut stderr| {
        let err = Arc::clone(&err);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                let mut err = err.lock().unwrap_or_else(|e| e.into_inner());
                let room = STDERR_MAX.saturating_sub(err.len());
                err.extend_from_slice(&buf[..n.min(room)]);
            }
        })
    });
    let mut stdin = child.stdin.take();
    let ended = {
        // Its input on the side of its exit: a command that writes before it reads, or never
        // reads, is not held up by the hub, and its exit ends the write.
        let wait = async {
            let write = async {
                if let Some(stdin) = stdin.as_mut() {
                    // A command that does not read its stdin closes it: no failure.
                    let _ = stdin.write_all(input.as_bytes()).await;
                }
                // Written: end of input. Then only the exit is waited for.
                drop(stdin.take());
                std::future::pending::<()>().await
            };
            tokio::select! {
                r = child.wait() => r,
                _ = write => unreachable!("the write waits for ever once done"),
            }
        };
        tokio::select! {
            r = wait => Ended::Exited(r),
            _ = tokio::time::sleep(timeout) => Ended::TimedOut,
            _ = cancelled.wait_for(|c| *c) => Ended::Cancelled,
        }
    };
    // A write cut short: the hub's end of the pipe closes here.
    drop(stdin);
    // What is left of its group goes with it, whatever ended it.
    if !matches!(ended, Ended::Exited(_)) || twr_runner::reap::group_running(pid) == Some(true) {
        super::jobs::end_group(pid, &mut child).await;
    }
    if let Some(reader) = reader {
        reader.abort();
    }
    let err = String::from_utf8_lossy(&err.lock().unwrap_or_else(|e| e.into_inner())).to_string();
    Some((ended, err))
}

/// How a run of the command ended.
#[derive(Debug)]
enum Ended {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::notify::NotifyEvent;

    fn candidate(event: NotifyEvent, who: &str, text: &str) -> Candidate {
        Candidate {
            event,
            key: format!("k:{who}"),
            thread: format!("id-{who}"),
            turn: Some(3),
            attn_id: Some(format!("a-{who}")),
            who: who.into(),
            text: text.into(),
            revision: false,
        }
    }

    /// A command that exits without reading its input while a process it left running holds
    /// its stdin unread (the write blocked on a full pipe): its exit is seen at once, not at
    /// the timeout, and that process is ended with it.
    #[tokio::test]
    async fn an_exit_is_seen_while_a_left_process_holds_the_input_unread() {
        let dir = tempfile::tempdir().unwrap();
        let ids = dir.path().join("ids");
        // `sleep` keeps a copy of the pipe's read end (fd 3) and reads nothing.
        let argv: Vec<String> = vec![
            "sh".into(),
            "-c".into(),
            "exec 3<&0; sleep 300 & echo $$ $! > \"$0\"; exit 3".into(),
            ids.to_string_lossy().into_owned(),
        ];
        let (_cancel, cancelled) = watch::channel(false);
        // Far more than a pipe holds.
        let input = "x".repeat(4 << 20);
        let started = Instant::now();
        let (ended, _) = run(&argv, input, Duration::from_secs(60), cancelled)
            .await
            .unwrap();
        assert!(
            matches!(&ended, Ended::Exited(Ok(s)) if s.code() == Some(3)),
            "{ended:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "seen after {:?}",
            started.elapsed()
        );
        let text = std::fs::read_to_string(&ids).unwrap();
        let pgid: u32 = text.split_whitespace().next().unwrap().parse().unwrap();
        // The group, `sleep` with it, is ended (a zombie waiting for its reaper counts as
        // ended).
        assert_ne!(
            twr_runner::reap::group_running(pgid),
            Some(true),
            "what the command left running outlived it"
        );
    }

    #[test]
    fn placeholders_are_filled_once() {
        let vars = [("title", "twr: {body}"), ("body", "b"), ("workspace", "w")];
        assert_eq!(fill("Title: {title}", &vars), "Title: twr: {body}");
        assert_eq!(fill("{body}/{workspace}{x}{", &vars), "b/w{x}{");
        assert_eq!(fill("plain", &vars), "plain");
    }

    #[test]
    fn without_detail_only_who_and_what() {
        let due = vec![candidate(
            NotifyEvent::Permission,
            "api",
            "Bash curl -H 'Authorization: s3cret'",
        )];
        let (title, body, input) = message(due.clone(), false, "default").unwrap();
        assert_eq!(title, "twr: api");
        assert_eq!(body, "api needs permission");
        assert!(!input.to_string().contains("s3cret"), "{input}");
        assert_eq!(input["items"][0]["event"], "permission");
        assert_eq!(input["items"][0]["item"], "a-api");
        assert_eq!(input["workspace"], "default");
        let (_, body, input) = message(due, true, "default").unwrap();
        assert!(body.contains("s3cret"), "{body}");
        assert!(input["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("s3cret"));
        let two = vec![
            candidate(NotifyEvent::Report, "web", ""),
            candidate(NotifyEvent::Question, "api", ""),
        ];
        let (title, body, input) = message(two, false, "w").unwrap();
        assert_eq!(title, "twr: 1 need you");
        assert_eq!(body, "api asks, web finished");
        assert_eq!(input["items"][0]["name"], "api");
    }
}

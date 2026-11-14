//! A review run on an ACP agent (docs/design/loops.md, "Milestone 4: implementation notes ›
//! ACP reviewers"): the round's prompt sent over the Agent Client Protocol itself, to the
//! agent of a `[runtimes.<name>]` preset whose `[runtimes.<name>.review]` names its read-only
//! mode. No command line of the agent's is needed beyond the preset's own `acp` one.
//!
//! - **The session.** The hub starts the preset's agent with twr's ACP adapter
//!   (`twr_adapters::acp`) in the thread's working directory, asks for one new session in the
//!   preset's read-only mode, on the round's model, and (when the preset lists efforts) at the
//!   round's effort; no MCP server, no file or terminal capability of the client's. The
//!   adapter holds the session to that mode (it switches a report it does not accept, and
//!   fails the session when it can't), and the run goes further: a session whose agent reports
//!   no mode at all, or that opened in any mode but the one asked for or another read-only one
//!   twr knows, is refused before the prompt is sent.
//! - **Permissions.** Every permission request (and any question) during the run is answered
//!   with a refusal (`reject_once`), never an allow, and noted in the round's log. twr never
//!   widens anything for a reviewer.
//! - **The answer.** The prompt is the round's, with [`OUTPUT_CONTRACT`] after it: the
//!   findings in one fenced `json` block matching `findings.schema.json`. The hub reads the
//!   LAST such block of the reply (`contract::findings_in_text`, the same `parse_findings` the
//!   other reviewers' answers go through) and writes it to the answer file. A reply without a
//!   valid block is asked once more, in the same session ([`ask_again`]); a second bad reply is
//!   a failed run, which the ladder retries as any failed run. The reply is data: nothing of
//!   it but the block is read, and the findings reach the thread as data, as every reviewer's.
//! - **Usage** is what the agent's prompt results report (summed over the two prompts), or
//!   nothing.
//! - **Process.** As every review run (`crate::one_shot`): a session and process group of its
//!   own, killed when the run ends however it ends (an answer, the round's timeout, a cancel);
//!   twr's tokens and markers scrubbed from its environment, `TWR_AGENT=<thread>:loop-<id>` in
//!   their place. What the agent said and did goes to `round-<round>.log` for people.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use twr_adapters::acp::translate::{mode_level, ModeLevel};
use twr_adapters::acp::AcpAdapter;
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, Answer, SpawnConfig};
use twr_core::event::{EventBody, TurnResult, Usage};
use twr_core::loops::contract::{findings_in_text, Finding, FINDINGS_SCHEMA, MAX_ANSWER_BYTES};

use super::review::{span, Call, Ended};
use crate::one_shot::{kill_group, scrubbed_names, Group};

/// How long the agent has to exit once its session is stopped, before its group is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The most of the agent's reply text kept per prompt, in bytes: far more than an answer
/// block may be ([`MAX_ANSWER_BYTES`]).
const REPLY_MAX: usize = 8 * MAX_ANSWER_BYTES;

/// The ACP agent a review run asks: a `[runtimes.<name>]` preset that can review.
#[derive(Debug, Clone, PartialEq)]
pub struct AcpReviewer {
    /// The preset's name.
    pub preset: String,
    /// The agent's argv (the preset's `acp` command line, split).
    pub argv: Vec<String>,
    /// Its read-only mode (`[runtimes.<name>.review] mode`).
    pub mode: String,
    /// Whether a round's effort is passed (the preset lists the efforts it takes).
    pub efforts: bool,
}

/// What follows the round's prompt for an agent that no schema holds to an answer's shape.
pub fn output_contract() -> String {
    format!(
        "OUTPUT CONTRACT (nothing enforces the answer's shape on this run, so follow it \
         exactly): end your reply with your findings as ONE fenced code block whose info \
         string is `json`, holding one JSON object that matches this JSON Schema:\n\n\
         ```json\n{}\n```\n\n\
         `{{\"findings\": []}}` means you found nothing. Only the LAST ```json block of your \
         reply is read; nothing outside it is. This run is read-only: edit no file, run nothing \
         that changes the working tree, and ask for no permission (every request is refused).",
        FINDINGS_SCHEMA.trim()
    )
}

/// The one follow-up prompt after a reply without a valid answer block (`why`).
pub fn ask_again(why: &str) -> String {
    format!(
        "[twr review run: your reply had no valid answer: {why}. Do not review again. Reply \
         with the findings of the review you just did, as the output contract says: one fenced \
         ```json block holding {{\"findings\": [...]}} that matches the schema \
         ({{\"findings\": []}} when you found nothing), and nothing else.]"
    )
}

/// Whether mode `reported` (what the session opened in) is one a review run accepts when
/// `asked` was asked for: exactly it, or a read-only mode twr knows.
pub fn read_only_mode(reported: &str, asked: &str) -> bool {
    let reported = reported.trim();
    !reported.is_empty()
        && (reported == asked.trim() || mode_level(reported) == Some(ModeLevel::Restricted))
}

/// The round's log, appended to (for people; never read to judge the round).
struct Log(Option<std::fs::File>);

impl Log {
    fn line(&mut self, text: &str) {
        if let Some(f) = &mut self.0 {
            let _ = writeln!(f, "{text}");
        }
    }
}

/// Runs `call` on `acp` (module docs); `spawned` is told the agent's process group.
pub async fn run(
    call: Call,
    acp: AcpReviewer,
    spawned: impl FnOnce(u32),
    mut cancel: tokio::sync::watch::Receiver<Option<String>>,
) -> Ended {
    // A previous run's answer is never taken for this one's.
    let _ = std::fs::remove_file(&call.answer_file);
    let mut log = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&call.log_file)
    {
        Ok(f) => Log(Some(f)),
        Err(e) => return Ended::Failed(format!("cannot open {}: {e}", call.log_file.display())),
    };
    if !super::review::plain(&call.model) {
        return Ended::Failed(format!("{:?} is no model name", call.model));
    }
    if acp.efforts && !super::review::plain(&call.effort) {
        return Ended::Failed(format!("{:?} is no effort", call.effort));
    }
    log.line(&format!(
        "twr: acp reviewer {} ({}) in its {:?} mode, model {}, effort {}",
        acp.preset,
        acp.argv.join(" "),
        acp.mode,
        call.model,
        if acp.efforts {
            call.effort.as_str()
        } else {
            "the agent's own"
        }
    ));
    let cfg = SpawnConfig {
        thread: format!("{}:review", call.marker),
        cwd: call.cwd.clone(),
        mode: acp.mode.clone(),
        model: Some(call.model.clone()),
        effort: acp.efforts.then(|| call.effort.clone()),
        env: {
            let mut env = vec![(crate::reap::AGENT_ENV.to_string(), call.marker.clone())];
            if let Some(p) = &call.path {
                env.push(("PATH".into(), p.to_string_lossy().into_owned()));
            }
            env
        },
        // Not twr's: no token, no marker of a runtime or a thread but its own; nor the hub's
        // own Claude Code session's, should the hub run in one (as `review::command`).
        env_remove: scrubbed_names(
            &[
                "TWR_",
                "CLAUDECODE",
                "CLAUDE_CODE_ENTRYPOINT",
                "CLAUDE_CODE_SSE_PORT",
            ],
            &[crate::reap::AGENT_ENV],
        ),
        ..Default::default()
    };
    let adapter = AcpAdapter::with_command(acp.argv.clone()).stop_grace(STOP_GRACE);
    let mut handle = match adapter.spawn(cfg, None) {
        Ok(h) => h,
        Err(e) => {
            let why = format!("cannot start acp preset {}: {e:#}", acp.preset);
            log.line(&format!("twr: {why}"));
            return Ended::Failed(why);
        }
    };
    let pid = handle.pid();
    let mut group = Group(pid);
    if let Some(pid) = pid {
        spawned(pid);
    }
    let Some(mut events) = handle.take_events() else {
        return Ended::Failed("the agent's events are gone".into());
    };
    let prompt = format!("{}\n\n{}", call.prompt.trim_end(), output_contract());
    let cancelled = async {
        loop {
            if let Some(why) = cancel.borrow().clone() {
                return why;
            }
            if cancel.changed().await.is_err() {
                return "the hub let go of it".to_string();
            }
        }
    };
    let outcome = tokio::select! {
        r = converse(&handle, &mut events, &acp, &prompt, &mut log) => r,
        _ = tokio::time::sleep(call.timeout) => Err(Ended::Failed(format!(
            "it timed out after {}",
            span(call.timeout)
        ))),
        why = cancelled => Err(Ended::Cancelled(why)),
    };
    // The session ends; what it started goes with it, whatever way the run ended.
    let _ = handle.stop();
    let _ = tokio::time::timeout(STOP_GRACE, async {
        while let Some(ev) = events.recv().await {
            if matches!(ev, AdapterEvent::Exited { .. }) {
                return;
            }
        }
    })
    .await;
    if let Some(pgid) = group.0.take() {
        kill_group(pgid);
    }
    let (body, findings, usage) = match outcome {
        Ok(r) => r,
        Err(Ended::Failed(why)) => {
            log.line(&format!("twr: the run failed: {why}"));
            return Ended::Failed(why);
        }
        Err(e) => return e,
    };
    if let Err(e) = std::fs::write(&call.answer_file, &body) {
        return Ended::Failed(format!("cannot write {}: {e}", call.answer_file.display()));
    }
    Ended::Answered { findings, usage }
}

/// The run's conversation: wait for the session, refuse a mode it can't vouch for, send the
/// prompt, read the reply, ask again once. Its answer block, findings and usage, or how the
/// run failed.
async fn converse(
    handle: &AdapterHandle,
    events: &mut UnboundedReceiver<AdapterEvent>,
    acp: &AcpReviewer,
    prompt: &str,
    log: &mut Log,
) -> Result<(String, Vec<Finding>, Option<Usage>), Ended> {
    let failed = |why: String| Ended::Failed(why);
    let mut usage: Option<Usage> = None;
    // The session first: nothing is sent before it is open in a mode the run accepts.
    loop {
        match next(events, handle, acp, log, &mut usage).await? {
            Next::Started(ext) => {
                check_mode(acp, &ext).map_err(failed)?;
                break;
            }
            Next::Ended(_) => {
                return Err(failed(
                    "the agent ended a turn before its session opened".into(),
                ))
            }
            Next::Text(_) | Next::Other => {}
        }
    }
    let mut ask = prompt.to_string();
    for turn in 1..=2u64 {
        handle
            .send(ask.clone(), turn)
            .map_err(|_| failed("the agent's session is gone".into()))?;
        let text = reply(events, handle, acp, log, &mut usage).await?;
        match findings_in_text(&text) {
            Ok((body, findings)) => return Ok((body, findings, usage)),
            Err(why) if turn == 1 => {
                log.line(&format!("twr: no valid answer ({why}); asking once more"));
                ask = ask_again(&why);
            }
            Err(why) => {
                return Err(failed(format!(
                    "its reply had no valid answer twice: {why}"
                )))
            }
        }
    }
    unreachable!("the loop returns on its second turn")
}

/// Reads the events of one prompt's turn up to its end (completed or interrupted): its
/// reply's text.
async fn reply(
    events: &mut UnboundedReceiver<AdapterEvent>,
    handle: &AdapterHandle,
    acp: &AcpReviewer,
    log: &mut Log,
    usage: &mut Option<Usage>,
) -> Result<String, Ended> {
    let mut text = String::new();
    loop {
        match next(events, handle, acp, log, usage).await? {
            Next::Text(t) => {
                if text.len() + t.len() <= REPLY_MAX {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&t);
                }
            }
            Next::Ended(Ok(())) => return Ok(text),
            Next::Ended(Err(why)) => return Err(Ended::Failed(why)),
            Next::Started(_) | Next::Other => {}
        }
    }
}

/// One event of the session, as the run reads it.
enum Next {
    /// The session opened: `thread.started`'s `ext.acp`.
    Started(Value),
    /// A final message of the agent's.
    Text(String),
    /// A turn ended: completed or interrupted (`Ok`), or with an error (why).
    Ended(Result<(), String>),
    /// Anything else (already logged or answered).
    Other,
}

/// The next event of the session, logged; a permission request or question refused on the
/// spot. `Err` when the agent is gone.
async fn next(
    events: &mut UnboundedReceiver<AdapterEvent>,
    handle: &AdapterHandle,
    acp: &AcpReviewer,
    log: &mut Log,
    usage: &mut Option<Usage>,
) -> Result<Next, Ended> {
    let Some(ev) = events.recv().await else {
        return Err(Ended::Failed(
            "the agent's session ended before it answered".into(),
        ));
    };
    let body = match ev {
        AdapterEvent::Exited { code, stderr_tail } => {
            log.line(&format!("twr: the agent exited ({code:?})"));
            for l in stderr_tail.lines().filter(|l| !l.trim().is_empty()) {
                log.line(&format!("stderr: {l}"));
            }
            return Err(Ended::Failed(format!(
                "acp preset {} exited before it answered{}",
                acp.preset,
                why_exited(&stderr_tail)
            )));
        }
        AdapterEvent::Emit { body, .. } => body,
    };
    Ok(match body {
        EventBody::ThreadStarted(p) => {
            let ext = p
                .ext
                .as_ref()
                .and_then(|x| x.get(twr_adapters::acp::EXT))
                .cloned()
                .unwrap_or(Value::Null);
            log.line(&format!(
                "twr: session {} open in mode {}",
                p.session_id.as_deref().unwrap_or("?"),
                ext["mode"].as_str().unwrap_or("(none reported)")
            ));
            Next::Started(ext)
        }
        EventBody::AgentText(t) if t.is_final => {
            log.line(&format!("agent: {}", t.text));
            Next::Text(t.text)
        }
        EventBody::AgentToolCall(c) => {
            log.line(&format!("tool: {} {}", c.tool, c.input_summary));
            Next::Other
        }
        EventBody::AttentionRaised(p) => {
            // Read-only rests on the agent's mode and on this: nothing it asks is allowed.
            log.line(&format!(
                "twr: refused a {:?} request during the review run (a review run allows \
                 nothing): {}",
                p.reason, p.summary
            ));
            let _ = handle.respond(p.attn_id, Answer::deny());
            Next::Other
        }
        EventBody::TurnEnded(p) => {
            if let Some(u) = &p.usage {
                let total = usage.get_or_insert(Usage::default());
                total.input += u.input;
                total.output += u.output;
            }
            match p.result {
                TurnResult::Error => {
                    let why = p
                        .ext
                        .as_ref()
                        .and_then(|x| x.get("twr"))
                        .and_then(|t| t.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("its turn ended in an error")
                        .to_string();
                    log.line(&format!("twr: the turn failed: {why}"));
                    Next::Ended(Err(format!(
                        "acp preset {}: {}",
                        acp.preset,
                        super::one_line(&why)
                    )))
                }
                _ => Next::Ended(Ok(())),
            }
        }
        _ => Next::Other,
    })
}

/// Whether the session `ext` (`thread.started`'s `ext.acp`) opened in a mode the run accepts.
fn check_mode(acp: &AcpReviewer, ext: &Value) -> Result<(), String> {
    if ext["mode_unenforced"] == true {
        return Err(format!(
            "acp preset {}'s agent reports no permission mode, so twr can't hold it to its \
             read-only mode {:?}",
            acp.preset, acp.mode
        ));
    }
    let reported = ext["mode"].as_str().unwrap_or_default();
    if !read_only_mode(reported, &acp.mode) {
        return Err(format!(
            "acp preset {}'s session opened in mode {reported:?}, not its read-only mode {:?}",
            acp.preset, acp.mode
        ));
    }
    Ok(())
}

/// `: <why>` of an agent that exited: the adapter's own reason (its `twr:` line) when it gave
/// one, else the last line it printed; empty without one.
fn why_exited(stderr_tail: &str) -> String {
    let lines: Vec<&str> = stderr_tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let why = lines
        .iter()
        .rev()
        .find_map(|l| l.strip_prefix("twr: "))
        .or_else(|| lines.last().copied());
    match why {
        Some(w) => format!(": {}", super::one_line(&truncate(w, 300))),
        None => String::new(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Whether `path` names a program twr would find: an absolute or relative path to a file, or
/// a bare name on the hub's `PATH`.
pub fn installed(program: &str) -> bool {
    twr_adapters::resolve::resolve(Path::new(program), &[], None, Path::new(""))
        .found
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_accepts_only_read_only_modes() {
        assert!(read_only_mode("read-only", "read-only"));
        assert!(read_only_mode("ro-review", "ro-review"));
        // Another read-only mode twr knows is as narrow.
        assert!(read_only_mode("plan", "read-only"));
        for (reported, asked) in [
            ("", "plan"),
            ("default", "plan"),
            ("auto", "read-only"),
            ("yolo", "ro-review"),
            ("other", "ro-review"),
        ] {
            assert!(!read_only_mode(reported, asked), "{reported} for {asked}");
        }
        let acp = AcpReviewer {
            preset: "ex".into(),
            argv: vec!["ex".into()],
            mode: "ro-review".into(),
            efforts: false,
        };
        assert!(check_mode(&acp, &serde_json::json!({"mode": "ro-review"})).is_ok());
        let e = check_mode(&acp, &serde_json::json!({"mode_unenforced": true})).unwrap_err();
        assert!(e.contains("reports no permission mode"), "{e}");
        let e = check_mode(&acp, &serde_json::json!({"mode": "default"})).unwrap_err();
        assert!(e.contains("opened in mode \"default\""), "{e}");
    }

    #[test]
    fn the_prompts_name_the_contract() {
        let c = output_contract();
        assert!(c.contains("ONE fenced code block whose info string is `json`"));
        assert!(c.contains("\"findings\""));
        assert!(c.contains("Only the LAST ```json block"));
        let a = ask_again("its reply has no closed ```json block with the findings");
        assert!(a.starts_with("[twr review run: your reply had no valid answer: its reply"));
        assert!(a.contains("Do not review again."));
        assert_eq!(
            why_exited("noise\ntwr: the agent reports no mode\nmore\n"),
            ": the agent reports no mode"
        );
        assert_eq!(why_exited("a\nlast\n"), ": last");
        assert_eq!(why_exited(""), "");
        assert!(installed("sh"));
        assert!(!installed("/nonexistent/twr-agent"));
    }
}

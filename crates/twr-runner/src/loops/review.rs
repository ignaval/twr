//! A review loop's review run: one headless, read-only, schema-constrained call of the
//! reviewer's CLI in the thread's working directory (docs/design/loops.md, "The review run"),
//! on the box the thread runs on: the hub's own (`hub::loops`), or a runner's
//! ([`super::runs`]), with that box's own CLI and login.
//!
//! | Reviewer | Call | Schema, answer | Read-only (hardcoded) |
//! |---|---|---|---|
//! | Codex | `codex exec --ephemeral --skip-git-repo-check --ignore-rules --color never --json -m <model> -c model_reasoning_effort="<effort>" -` (the prompt on stdin) | `--output-schema <file> -o <answer file>` | `-s read-only -c approval_policy="never" -c mcp_servers={}`: its shell runs in Codex's read-only sandbox, nothing asks, no MCP server |
//! | Claude Code | `claude -p --model <model> --effort <effort> --output-format json --no-session-persistence --add-dir <loop folder>` (the prompt on stdin) | `--json-schema <schema>`: the result's `structured_output` is written to the answer file | `--permission-mode plan --permission-prompts none --tools Read,Grep,Glob --strict-mcp-config`: no write or shell tool, no MCP server, and anything that would ask is refused |
//! | Grok Build | `grok -p <prompt> -m <model> --effort <effort> --output-format json` | `--json-schema <schema>`: the result's `structuredOutput` is written to the answer file | `--permission-mode plan --sandbox read-only --tools read_file,grep,list_dir --disable-web-search --no-subagents`: no write or shell tool, under its read-only sandbox profile |
//! | ACP preset | the preset's agent over ACP: one session, one prompt ([`super::acp_review`]) | none: the findings in the reply's last fenced `json` block, asked again once | the preset's `[runtimes.<name>.review] mode`, which the session must report; every permission request refused |
//!
//! Nothing in the settings reaches these flags but the model and the effort (each checked to
//! be a plain name first, [`plain`]): no setting can widen what the reviewer may do. Unlike a
//! suggestion call (`hub::suggest`) the reviewer keeps the user's own CLI configuration (their
//! model providers, instructions, rules; Codex its sandboxed shell), but none of what would let
//! it write.
//!
//! It runs in the thread's working directory, in a session and process group of its own
//! (`crate::one_shot`), killed with everything it started when the run ends however it ends: an
//! answer, the round's timeout (`[loops] round_timeout`), or a cancel (the loop stopped, the
//! thread stopped, parked or deleted, the hub stopping, a runner's connection to the hub
//! ending). Its environment is the box's (the hub's, or a runner's) without twr's tokens and
//! markers (every `TWR_*` variable, the runtime generation, the hub's own Claude Code
//! session's markers), with `TWR_AGENT=<thread>:loop-<loop id>` and, for a reviewer on a
//! named account, that account's variables, as a thread on it gets them.
//!
//! What it printed goes to `round-<round>.log` in the loop folder, for people, and is never
//! read to judge the round: the hub reads the answer file alone ([`parse_findings`]), and of
//! the log only what the run spent (Codex's last `turn.completed` line; Claude Code's and Grok
//! Build's result object, which also carries their answer). A non-zero exit, a timeout, a
//! missing or unparseable answer is a failed run, with the reason.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use twr_core::event::Usage;
use twr_core::loops::contract::{parse_findings, Finding, FINDINGS_SCHEMA, MAX_ANSWER_BYTES};

use crate::one_shot::{kill_group, scrub_env, Group};

/// The most of a CLI's result object (Claude Code's, Grok Build's stdout) read, in bytes.
const RESULT_MAX: u64 = 8 << 20;

/// The most of the end of Codex's session stream read for what the run spent, in bytes.
const USAGE_TAIL: u64 = 256 * 1024;

/// One review run, ready to start.
#[derive(Debug, Clone)]
pub struct Call {
    /// The reviewer's runtime (`codex`, `claude-code`, `grok-build`).
    pub runtime: String,
    /// Its CLI.
    pub bin: PathBuf,
    /// The model.
    pub model: String,
    /// The round's effort.
    pub effort: String,
    /// The thread's working directory.
    pub cwd: PathBuf,
    /// The loop folder.
    pub folder: PathBuf,
    /// The prompt.
    pub prompt: String,
    /// `findings.schema.json` in the folder.
    pub schema_file: PathBuf,
    /// `round-<round>.answer.json`.
    pub answer_file: PathBuf,
    /// `round-<round>.log`.
    pub log_file: PathBuf,
    /// A named account's variables to set, and those to remove.
    pub account_env: Option<twr_adapters::account::AccountEnv>,
    /// Its `TWR_AGENT` marker.
    pub marker: String,
    /// The round's timeout.
    pub timeout: Duration,
    /// An ACP preset's agent, when the reviewer is one (`Inner::acp_reviewer`): `Err` says
    /// why the preset can't review now. `None`: a built-in reviewer's CLI.
    pub acp: Option<Result<super::acp_review::AcpReviewer, String>>,
    /// The `PATH` it runs with (a box's login shell's before the runner's, as its runtimes
    /// get); `None`: this process's.
    pub path: Option<std::ffi::OsString>,
}

/// How a review run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// Its answer, read.
    Answered {
        /// The findings.
        findings: Vec<Finding>,
        /// What it spent, when the CLI said.
        usage: Option<Usage>,
    },
    /// A failed run: why.
    Failed(String),
    /// Cancelled (why): nothing is recorded of it as a failure.
    Cancelled(String),
}

/// Whether `s` is a plain model or effort name, never anything a CLI would read as a flag or
/// a TOML value of its own.
pub fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '@' | '[' | ']')
        })
}

/// The argv of `call` (module docs), without the program.
pub fn argv(call: &Call) -> Result<Vec<String>, String> {
    if !plain(&call.model) {
        return Err(format!("{:?} is no model name", call.model));
    }
    if !plain(&call.effort) {
        return Err(format!("{:?} is no effort", call.effort));
    }
    let path = |p: &Path| p.to_string_lossy().into_owned();
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    Ok(match call.runtime.as_str() {
        "codex" => {
            let mut a = s(&[
                "exec",
                "--ephemeral",
                "--skip-git-repo-check",
                "--ignore-rules",
                "--color",
                "never",
                "--json",
                "-s",
                "read-only",
                "-c",
                "approval_policy=\"never\"",
                "-c",
                "mcp_servers={}",
                "-m",
            ]);
            a.push(call.model.clone());
            a.push("-c".into());
            a.push(format!("model_reasoning_effort=\"{}\"", call.effort));
            a.push("--output-schema".into());
            a.push(path(&call.schema_file));
            a.push("-o".into());
            a.push(path(&call.answer_file));
            a.push("-".into());
            a
        }
        "claude-code" => {
            let mut a = s(&["-p", "--model"]);
            a.push(call.model.clone());
            a.push("--effort".into());
            a.push(call.effort.clone());
            a.extend(s(&[
                "--output-format",
                "json",
                "--json-schema",
                FINDINGS_SCHEMA,
                "--permission-mode",
                "plan",
                "--permission-prompts",
                "none",
                "--tools",
                "Read,Grep,Glob",
                "--strict-mcp-config",
                "--no-session-persistence",
                "--add-dir",
            ]));
            a.push(path(&call.folder));
            a
        }
        "grok-build" => {
            let mut a = s(&["-p"]);
            a.push(call.prompt.clone());
            a.push("-m".into());
            a.push(call.model.clone());
            a.push("--effort".into());
            a.push(call.effort.clone());
            a.extend(s(&[
                "--output-format",
                "json",
                "--json-schema",
                FINDINGS_SCHEMA,
                "--permission-mode",
                "plan",
                "--sandbox",
                "read-only",
                "--tools",
                "read_file,grep,list_dir",
                "--disable-web-search",
                "--no-subagents",
            ]));
            // No `--add-dir` for the loop folder: a headless Grok session refuses it
            // ("additionalDirectories is not supported: a session works only in its cwd",
            // grok 1.0.70), and its read tools reach the folder's files by their absolute
            // paths without it, under the read-only sandbox too.
            a
        }
        other => return Err(format!("{other} can't review in this version")),
    })
}

/// The command of `call`: its program, argv, working directory and environment (module docs).
pub fn command(call: &Call) -> Result<tokio::process::Command, String> {
    let args = argv(call)?;
    let mut cmd = tokio::process::Command::new(&call.bin);
    cmd.args(&args).current_dir(&call.cwd);
    // Not twr's: no token, no marker of a runtime or a thread but its own; nor the hub's own
    // Claude Code session's, should the hub run in one.
    scrub_env(
        &mut cmd,
        &call.marker,
        &[
            "TWR_",
            "CLAUDECODE",
            "CLAUDE_CODE_ENTRYPOINT",
            "CLAUDE_CODE_SSE_PORT",
        ],
        &[],
    );
    if let Some(p) = &call.path {
        cmd.env("PATH", p);
    }
    if let Some((set, remove)) = &call.account_env {
        for k in remove {
            cmd.env_remove(k);
        }
        cmd.envs(set.iter().map(|(k, v)| (k, v)));
    }
    Ok(cmd)
}

/// Runs `call` (module docs); `spawned` is told its process group as soon as it started.
/// `cancel` ends it (its group killed) with the reason it then holds.
pub async fn run(
    call: Call,
    spawned: impl FnOnce(u32),
    mut cancel: tokio::sync::watch::Receiver<Option<String>>,
) -> Ended {
    match call.acp.clone() {
        Some(Ok(acp)) => return super::acp_review::run(call, acp, spawned, cancel).await,
        Some(Err(why)) => return Ended::Failed(why),
        None => {}
    }
    let mut cmd = match command(&call) {
        Ok(c) => c,
        Err(e) => return Ended::Failed(e),
    };
    // A previous run's answer is never taken for this one's.
    let _ = std::fs::remove_file(&call.answer_file);
    let log = match std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&call.log_file)
    {
        Ok(f) => f,
        Err(e) => return Ended::Failed(format!("cannot open {}: {e}", call.log_file.display())),
    };
    let err = match log.try_clone() {
        Ok(f) => f,
        Err(e) => return Ended::Failed(format!("cannot open its log: {e}")),
    };
    let codex = call.runtime == "codex";
    cmd.stdin(if call.runtime == "grok-build" {
        std::process::Stdio::null()
    } else {
        std::process::Stdio::piped()
    })
    .stderr(err)
    .kill_on_drop(true);
    // Codex's session stream goes straight to the log; the others print one result object,
    // read here (then kept in the log too).
    if codex {
        cmd.stdout(log);
    } else {
        cmd.stdout(std::process::Stdio::piped());
    }
    twr_adapters::process::new_session(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ended::Failed(format!(
                "the reviewer's CLI ({}) is not installed",
                call.bin.display()
            ))
        }
        Err(e) => return Ended::Failed(format!("cannot start {}: {e}", call.bin.display())),
    };
    let mut group = Group(child.id());
    if let Some(pid) = child.id() {
        spawned(pid);
    }
    let write = {
        let stdin = child.stdin.take();
        let prompt = call.prompt.clone();
        async move {
            if let Some(mut stdin) = stdin {
                let _ = stdin.write_all(prompt.as_bytes()).await;
                let _ = stdin.shutdown().await;
            }
        }
    };
    let read = {
        let stdout = child.stdout.take();
        async move {
            let mut out = Vec::new();
            if let Some(stdout) = stdout {
                let _ = stdout.take(RESULT_MAX).read_to_end(&mut out).await;
            }
            out
        }
    };
    let run = async {
        let ((), out) = tokio::join!(write, read);
        let status = child.wait().await;
        (out, status)
    };
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
        r = run => Ok(r),
        _ = tokio::time::sleep(call.timeout) => Err(Ended::Failed(format!(
            "it timed out after {}",
            span(call.timeout)
        ))),
        why = cancelled => Err(Ended::Cancelled(why)),
    };
    // What it started goes with it, whatever way it ended.
    if let Some(pgid) = group.0.take() {
        kill_group(pgid);
    }
    let (out, status) = match outcome {
        Ok(r) => r,
        Err(e) => return e,
    };
    if !out.is_empty() {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .open(&call.log_file)
        {
            let _ = f.write_all(&out);
        }
    }
    let status = match status {
        Ok(s) => s,
        Err(e) => return Ended::Failed(format!("cannot wait for it: {e}")),
    };
    if codex {
        if !status.success() {
            return Ended::Failed(format!(
                "codex exited with {status}{}",
                last_line(&call.log_file)
            ));
        }
        let answer = match read_answer(&call.answer_file) {
            Ok(a) => a,
            Err(e) => return Ended::Failed(e),
        };
        return match parse_findings(&answer) {
            Ok(findings) => Ended::Answered {
                findings,
                usage: codex_usage(&call.log_file),
            },
            Err(e) => Ended::Failed(e.to_string()),
        };
    }
    result_answer(&call, &out, status)
}

/// What the result object `out` of Claude Code or Grok Build (exit `status`) answered: its
/// structured output, written to the answer file and read from there as Codex's is.
fn result_answer(call: &Call, out: &[u8], status: std::process::ExitStatus) -> Ended {
    let what = if call.runtime == "grok-build" {
        "grok"
    } else {
        "claude"
    };
    let v: Value = match serde_json::from_slice(out) {
        Ok(v) => v,
        Err(_) if !status.success() => {
            return Ended::Failed(format!(
                "{what} exited with {status}{}",
                last_line(&call.log_file)
            ))
        }
        Err(_) => return Ended::Failed(format!("{what}'s result was not JSON")),
    };
    if v["is_error"] == true {
        let why = v["result"].as_str().unwrap_or("an error");
        return Ended::Failed(format!(
            "{what} failed: {}",
            super::one_line(why.lines().next().unwrap_or(why))
        ));
    }
    let structured = ["structured_output", "structuredOutput"]
        .iter()
        .map(|k| &v[*k])
        .find(|s| s.is_object())
        .cloned()
        .or_else(|| {
            // Its final text, when it is the answer itself.
            ["result", "text"]
                .iter()
                .filter_map(|k| v[*k].as_str())
                .find_map(|t| serde_json::from_str::<Value>(t.trim()).ok())
                .filter(Value::is_object)
        });
    let Some(answer) = structured else {
        return Ended::Failed(if status.success() {
            format!("{what} gave no structured answer")
        } else {
            format!("{what} exited with {status} and no answer")
        });
    };
    let text = answer.to_string();
    if let Err(e) = std::fs::write(&call.answer_file, &text) {
        return Ended::Failed(format!("cannot write {}: {e}", call.answer_file.display()));
    }
    match parse_findings(&text) {
        Ok(findings) => Ended::Answered {
            findings,
            usage: result_usage(&v),
        },
        Err(e) => Ended::Failed(e.to_string()),
    }
}

/// The answer file, read (bounded): `Err` when there is none or it is empty.
fn read_answer(path: &Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|_| "it wrote no answer file".to_string())?;
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut std::io::Read::take(f, MAX_ANSWER_BYTES as u64 + 1),
        &mut text,
    )
    .map_err(|e| format!("its answer file can't be read: {e}"))?;
    if text.trim().is_empty() {
        return Err("its answer file is empty".into());
    }
    Ok(text)
}

/// The end of `path`, at most `max` bytes.
fn tail(path: &Path, max: u64) -> String {
    use std::io::{Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(max)));
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(&mut std::io::Read::take(f, max), &mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// `: <the log's last line>`, for a failed run's reason (empty without one).
fn last_line(log: &Path) -> String {
    let t = tail(log, 4096);
    match t.lines().rev().find(|l| !l.trim().is_empty()) {
        Some(l) => format!(": {}", super::one_line(&truncate(l, 300))),
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

/// What a Codex run spent: its session stream's last `turn.completed` `usage` (`input_tokens`
/// counts the cached ones). Codex reports no cost.
fn codex_usage(log: &Path) -> Option<Usage> {
    let t = tail(log, USAGE_TAIL);
    let v = t
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "turn.completed")?;
    let u = &v["usage"];
    Some(Usage {
        input: u["input_tokens"].as_u64().unwrap_or(0),
        output: u["output_tokens"].as_u64().unwrap_or(0),
        cost: None,
    })
}

/// What a Claude Code or Grok Build run spent, from its result object: its input tokens with
/// the cache's reads and writes, its output tokens, and its cost.
fn result_usage(v: &Value) -> Option<Usage> {
    let u = v.get("usage").filter(|u| u.is_object())?;
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Some(Usage {
        input: n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
        output: n("output_tokens"),
        cost: v["total_cost_usd"].as_f64(),
    })
}

/// `d` for people: `1h`, `25m`, `40s`.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 && s.is_multiple_of(3600) {
        format!("{}h", s / 3600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(runtime: &str) -> Call {
        Call {
            runtime: runtime.into(),
            bin: "x".into(),
            model: "m-1".into(),
            effort: "xhigh".into(),
            cwd: "/w".into(),
            folder: "/data/loops/l1".into(),
            prompt: "review".into(),
            schema_file: "/data/loops/l1/findings.schema.json".into(),
            answer_file: "/data/loops/l1/round-m1.answer.json".into(),
            log_file: "/data/loops/l1/round-m1.log".into(),
            account_env: None,
            marker: "t1:loop-l1".into(),
            timeout: Duration::from_secs(60),
            acp: None,
            path: None,
        }
    }

    /// Each reviewer is read-only by flags no setting reaches.
    #[test]
    fn every_reviewer_is_read_only() {
        let codex = argv(&call("codex")).unwrap();
        let has = |a: &[String], pair: [&str; 2]| {
            a.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
        };
        assert!(has(&codex, ["-s", "read-only"]));
        assert!(has(&codex, ["-c", "approval_policy=\"never\""]));
        assert!(has(&codex, ["-c", "model_reasoning_effort=\"xhigh\""]));
        assert!(has(&codex, ["-o", "/data/loops/l1/round-m1.answer.json"]));
        let claude = argv(&call("claude-code")).unwrap();
        assert!(has(&claude, ["--permission-mode", "plan"]));
        assert!(has(&claude, ["--tools", "Read,Grep,Glob"]));
        assert!(has(&claude, ["--permission-prompts", "none"]));
        assert!(claude.contains(&"--strict-mcp-config".to_string()));
        let grok = argv(&call("grok-build")).unwrap();
        assert!(has(&grok, ["--sandbox", "read-only"]));
        assert!(has(&grok, ["--permission-mode", "plan"]));
        assert!(has(&grok, ["--tools", "read_file,grep,list_dir"]));
        // A headless Grok session refuses `--add-dir`: the run would fail before it starts.
        assert!(!grok.iter().any(|a| a == "--add-dir"));
        assert!(argv(&call("acp")).is_err());
        // A model or an effort that would read as a flag or a value of its own is refused.
        for bad in ["-s", "x\"y", "a b", "", "m=1"] {
            let mut c = call("codex");
            c.model = bad.into();
            assert!(argv(&c).is_err(), "{bad}");
            let mut c = call("codex");
            c.effort = bad.into();
            assert!(argv(&c).is_err(), "{bad}");
        }
    }

    #[test]
    fn usage_is_read_from_the_result() {
        let v: Value = serde_json::json!({"usage": {"input_tokens": 6,
            "cache_creation_input_tokens": 9083, "cache_read_input_tokens": 8923,
            "output_tokens": 173}, "total_cost_usd": 0.002});
        assert_eq!(
            result_usage(&v),
            Some(Usage {
                input: 18012,
                output: 173,
                cost: Some(0.002)
            })
        );
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        std::fs::write(
            &log,
            "{\"type\":\"turn.started\"}\nnoise\n{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":21937,\"output_tokens\":16}}\n",
        )
        .unwrap();
        assert_eq!(
            codex_usage(&log),
            Some(Usage {
                input: 21937,
                output: 16,
                cost: None
            })
        );
        assert_eq!(span(Duration::from_secs(3600)), "1h");
        assert_eq!(span(Duration::from_secs(90)), "1m");
    }
}

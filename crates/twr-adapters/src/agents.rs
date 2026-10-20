//! The box's agent probe: for each built-in agent runtime with a CLI of its own
//! ([`crate::resolve::BUILTIN_AGENTS`]: `claude-code` → `claude`, `codex` → `codex`,
//! `grok-build` → `grok`), the executable the box would start ([`crate::resolve::resolve`]
//! with the box's baseline environment: no thread `PATH`, the login shell's merged before the
//! runner's), its version and whether it is signed in ([`AgentInfo`]). Shared by the runner
//! (its inventory and `Op::Agents`) and `twr runner --agents` ([`report`]).
//!
//! - Version: `<program> --version`, its first non-empty line.
//! - Claude Code: `claude auth status` (JSON): only `loggedIn` and `authMethod` are read, the
//!   latter only as one of a few known words ([`claude_method`]; anything else is `other`).
//!   Nothing else of its output (the account's email, organization, plan) is kept.
//! - Codex: `codex login status`: exit 0 is signed in, any other exit is not; no text kept.
//! - Grok Build: no status command: unknown.
//!
//! Each command runs as a probe ([`crate::process::run_bounded`]: no terminal, stdin
//! `/dev/null`, stderr discarded, twr's secrets removed) for at most [`COMMAND_TIMEOUT`],
//! reading at most [`MAX_OUTPUT`] bytes, with the `PATH` the runtime would get (a CLI that is
//! a Node script finds `node` there). A command that fails gives `unknown` with a fixed reason
//! ([`AgentInfo::note`]); what any command printed is never logged. The agents are probed at
//! once, their commands too, so a probe takes at most about [`COMMAND_TIMEOUT`].

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use twr_core::runner::{AgentInfo, AgentLogin, AgentsReport, LoginPathReport};

use crate::process::{self, Bounded};
use crate::resolve::{self, LoginPath, Resolved, BUILTIN_AGENTS};

/// How long each command of the probe may take.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// The most a command of the probe may print.
pub const MAX_OUTPUT: usize = 64 << 10;

/// Longest [`AgentInfo::path`] kept, in bytes.
const MAX_PATH: usize = 1024;

/// Longest [`AgentInfo::version`] kept, in bytes.
const MAX_VERSION: usize = 100;

/// Claude Code's `authMethod` words kept as they are (`apiKey` is spelled `api-key`).
const CLAUDE_METHODS: &[&str] = &["claude.ai", "console", "api-key", "bedrock", "vertex"];

/// The probe's notes that take no value ([`AgentInfo::note`]).
const NOT_ON_PATH: &str = "not found on PATH";
const NO_STATUS_COMMAND: &str = "it has no login status command";
const NO_VERSION: &str = "`--version` printed no version";
const NO_JSON_STATUS: &str = "`claude auth status` printed no JSON status";
const NOT_SAID: &str = "`claude auth status` did not say whether it is signed in";

/// The login status commands whose failure is a note ([`failure`]).
const STATUS_COMMANDS: &[&str] = &["claude auth status", "codex login status"];

/// The note kept in place of one a box reported that the probe never makes ([`kept_report`]).
pub const OTHER_NOTE: &str = "the box reported something else";

/// The runtime an agent name names: `claude` / `claude-code`, `codex`, `grok` /
/// `grok-build` (what `twr box login` and `twr runner --login` take).
pub fn agent_runtime(name: &str) -> Option<&'static str> {
    match name {
        "claude" | "claude-code" => Some(crate::claude_code::NAME),
        "codex" => Some(crate::codex::NAME),
        "grok" | "grok-build" => Some(crate::grok_build::NAME),
        _ => None,
    }
}

/// The arguments of `runtime`'s login command, run in the user's terminal: `claude auth
/// login`, `codex login --device-auth`, `grok login --device-auth` (a device code works over
/// ssh, where no browser opens).
pub fn login_args(runtime: &str) -> Option<&'static [&'static str]> {
    match runtime {
        crate::claude_code::NAME => Some(&["auth", "login"]),
        crate::codex::NAME => Some(&["login", "--device-auth"]),
        crate::grok_build::NAME => Some(&["login", "--device-auth"]),
        _ => None,
    }
}

/// The login command of built-in agent runtime `runtime` as the box would run the agent
/// ([`agent_command`] with its [`login_args`], the baseline environment and `login`). An error
/// when the runtime has no CLI of its own or its program is not found.
pub fn login_command(
    runtime: &str,
    login: Option<&LoginPath>,
    var: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<std::process::Command> {
    let Some(args) = login_args(runtime) else {
        anyhow::bail!("{runtime} has no login command");
    };
    agent_command(runtime, args, &[], login, var)
}

/// What `twr runner --exec <runtime> [--path <PATH>] [--env NAME=VALUE]... [--unset NAME]...
/// -- <args>` runs (a remote attach to a thread on an SSH box): the runtime's program resolved
/// exactly as a spawn of the thread resolves it ([`agent_command`]: on the thread's `PATH`
/// `thread_path` when it configures one, else on `login`, asked with this process's
/// environment, merged before this process's `PATH`), with `args`, which the hub built, and
/// the rest of the thread's environment `env` (`(name, Some(value))` sets, `(name, None)`
/// removes) set on the command only, as a spawn's runtime gets it: it neither picks the
/// program nor changes its `PATH`. An error, besides [`agent_command`]'s, for a name of `env`
/// that is no variable name or is twr's own (`TWR_*`, which `[threads] env` refuses too), or
/// is `PATH` (that is `thread_path`).
pub fn exec_command(
    runtime: &str,
    args: &[String],
    thread_path: Option<&str>,
    env: &[(String, Option<String>)],
    login: Option<&LoginPath>,
    var: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<std::process::Command> {
    for (k, _) in env {
        twr_core::config::check_env_name(k).map_err(anyhow::Error::msg)?;
        if k == "PATH" {
            anyhow::bail!("the thread's PATH is --path, not --env");
        }
    }
    let path: Vec<(String, String)> = thread_path
        .map(|p| ("PATH".to_string(), p.to_string()))
        .into_iter()
        .collect();
    let login = if path.is_empty() { login } else { None };
    let mut cmd = agent_command(runtime, args, &path, login, var)?;
    for (k, v) in env {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    Ok(cmd)
}

/// Built-in agent runtime `runtime`'s program with `args`, as a spawn on this box in the
/// current directory with `env` added would run it: the program chosen as every path chooses
/// it ([`resolve::runtime_program`]: `TWR_*_BIN` as `var` reads it, else the command), found
/// on the `PATH` the runtime gets ([`resolve::resolve`] of `env` and `login`), with that
/// `PATH` and twr's secrets removed. An error when the runtime has no CLI of its own or its
/// program is not found.
pub fn agent_command<A: AsRef<std::ffi::OsStr>>(
    runtime: &str,
    args: &[A],
    env: &[(String, String)],
    login: Option<&LoginPath>,
    var: impl Fn(&str) -> Option<OsString>,
) -> anyhow::Result<std::process::Command> {
    let Some(program) = resolve::runtime_program(runtime, var) else {
        anyhow::bail!("{runtime} is not an agent runtime with a CLI of its own");
    };
    let r = resolve::resolve(&program, env, login, Path::new(""));
    let Some(found) = r.found else {
        if program.components().count() > 1 {
            anyhow::bail!(
                "`{}` ({}) is not an executable file on this box",
                program.display(),
                bin_env(runtime)
            );
        }
        anyhow::bail!(
            "`{}` is not on {}: install it there (or point twr at it with {})",
            program.display(),
            match (crate::process::env_path(env), login) {
                (Some(_), _) => "the thread's PATH",
                (None, Some(_)) => "this box's PATH (the login shell's included)",
                (None, None) => "this box's PATH",
            },
            bin_env(runtime)
        );
    };
    let mut cmd = std::process::Command::new(found);
    cmd.args(args);
    if let Some(p) = &r.path {
        cmd.env("PATH", p);
    }
    process::scrub_std(&mut cmd);
    Ok(cmd)
}

/// The `TWR_*_BIN` of `runtime`.
fn bin_env(runtime: &str) -> &'static str {
    BUILTIN_AGENTS
        .iter()
        .find(|(r, ..)| *r == runtime)
        .map_or("its TWR_*_BIN", |(.., v)| v)
}

/// What `twr runner --agents` prints: how asking the login shell `shell` for its `PATH` went
/// (`login`, as the runner asked it: `twr_runner::agents::agents_report`), then the agents
/// probed with that `PATH` ([`probe`]).
pub async fn report(shell: Option<&Path>, login: Result<LoginPath, String>) -> AgentsReport {
    let agents = probe(login.as_ref().ok()).await;
    AgentsReport {
        login_path: LoginPathReport {
            shell: shell.map(|p| printable(&p.to_string_lossy(), MAX_PATH)),
            entries: login.as_ref().ok().map(|p| p.entries().len()),
            error: login.err(),
        },
        agents,
    }
}

/// Probes every built-in agent with the box's baseline environment: no thread `PATH`, `login`
/// merged before the runner's ([`resolve::runtime_path`]), `TWR_*_BIN` from the process
/// environment.
pub async fn probe(login: Option<&LoginPath>) -> Vec<AgentInfo> {
    let mut env = Vec::new();
    resolve::with_login_path(&mut env, login);
    probe_with(&env, |k| std::env::var_os(k), COMMAND_TIMEOUT).await
}

/// [`probe`] with the environment `env` the runtimes get (its `PATH`, else the runner's),
/// `TWR_*_BIN` as `var` reads them, each command bounded by `limit`.
pub async fn probe_with(
    env: &[(String, String)],
    var: impl Fn(&str) -> Option<OsString>,
    limit: Duration,
) -> Vec<AgentInfo> {
    let one = |i: usize| {
        let (runtime, ..) = BUILTIN_AGENTS[i];
        let program = resolve::runtime_program(runtime, &var).unwrap_or_default();
        let r = resolve::resolve(&program, env, None, Path::new(""));
        probe_one(runtime, r, &[], &[], limit)
    };
    let (a, b, c) = tokio::join!(one(0), one(1), one(2));
    vec![a, b, c]
}

// `probe_with` probes each of them.
const _: () = assert!(BUILTIN_AGENTS.len() == 3);

/// One account of built-in agent runtime `runtime`: [`probe_with`]'s probe of that runtime
/// alone (its program resolved the same way, on `env`'s `PATH` else the runner's), its
/// commands run with `env` set and then `remove` removed (the account's variables and the
/// stray credentials of [`crate::account::account_env`]). Grok Build stays unknown (no status
/// command).
pub async fn probe_account(
    runtime: &str,
    env: &[(String, String)],
    remove: &[String],
    var: impl Fn(&str) -> Option<OsString>,
    limit: Duration,
) -> AgentInfo {
    let program = resolve::runtime_program(runtime, &var).unwrap_or_default();
    let r = resolve::resolve(&program, env, None, Path::new(""));
    probe_one(runtime, r, env, remove, limit).await
}

/// One agent: its version and login state, both at once, each command with `extra` set and
/// `remove` removed.
async fn probe_one(
    runtime: &str,
    r: Resolved,
    extra: &[(String, String)],
    remove: &[String],
    limit: Duration,
) -> AgentInfo {
    let mut info = AgentInfo {
        runtime: runtime.to_string(),
        ..Default::default()
    };
    let Some(found) = r.found.clone() else {
        info.note = Some(if r.program.components().count() > 1 {
            format!("{} names no executable file", bin_env(runtime))
        } else {
            NOT_ON_PATH.to_string()
        });
        return info;
    };
    info.found = true;
    info.path = Some(printable(&found.to_string_lossy(), MAX_PATH));
    let command = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new(&found);
        cmd.args(args).envs(extra.iter().cloned());
        if let Some(p) = &r.path {
            cmd.env("PATH", p);
        }
        for k in remove {
            cmd.env_remove(k);
        }
        cmd
    };
    let version = process::run_bounded(command(&["--version"]), limit, MAX_OUTPUT, |_| false);
    let status = async {
        match runtime {
            crate::claude_code::NAME => {
                let out =
                    process::run_bounded(command(&["auth", "status"]), limit, MAX_OUTPUT, |_| {
                        false
                    })
                    .await;
                claude_login(&out, limit)
            }
            crate::codex::NAME => {
                let out =
                    process::run_bounded(command(&["login", "status"]), limit, MAX_OUTPUT, |_| {
                        false
                    })
                    .await;
                codex_login(&out, limit)
            }
            _ => (
                AgentLogin::Unknown,
                None,
                Some(NO_STATUS_COMMAND.to_string()),
            ),
        }
    };
    let (version, (login, method, note)) = tokio::join!(version, status);
    info.version = match version {
        Bounded::Exited(Some(0), out) => first_line(&out),
        _ => None,
    };
    info.login = login;
    info.method = method;
    info.note = note.or_else(|| info.version.is_none().then(|| NO_VERSION.to_string()));
    tracing::debug!(
        runtime,
        found = %found.display(),
        login = ?info.login,
        "probed an agent CLI"
    );
    info
}

/// The login state `claude auth status` gave (`out`): its JSON's `loggedIn`, and how
/// ([`claude_method`]) when signed in. Nothing else of the JSON is read.
fn claude_login(out: &Bounded, limit: Duration) -> (AgentLogin, Option<String>, Option<String>) {
    let unknown = |why: String| (AgentLogin::Unknown, None, Some(why));
    let printed = match out {
        Bounded::Exited(Some(0 | 1), o) => o,
        other => return unknown(failure("claude auth status", other, limit)),
    };
    let Ok(serde_json::Value::Object(status)) = serde_json::from_slice(printed) else {
        return unknown(NO_JSON_STATUS.into());
    };
    match status.get("loggedIn") {
        Some(serde_json::Value::Bool(true)) => (
            AgentLogin::LoggedIn,
            status.get("authMethod").and_then(claude_method),
            None,
        ),
        Some(serde_json::Value::Bool(false)) => (AgentLogin::LoggedOut, None, None),
        _ => unknown(NOT_SAID.into()),
    }
}

/// Claude Code's `authMethod` as twr keeps it: one of [`CLAUDE_METHODS`] (`apiKey` /
/// `api_key` as `api-key`), any other string `other`; `None` when it is not a string.
pub fn claude_method(v: &serde_json::Value) -> Option<String> {
    let m = v.as_str()?;
    let m = match m {
        "apiKey" | "api_key" => "api-key",
        m => m,
    };
    Some(
        CLAUDE_METHODS
            .iter()
            .find(|k| **k == m)
            .map_or("other", |k| k)
            .to_string(),
    )
}

/// The login state `codex login status` gave: exit 0 signed in, another exit not.
fn codex_login(out: &Bounded, limit: Duration) -> (AgentLogin, Option<String>, Option<String>) {
    match out {
        Bounded::Exited(Some(0), _) => (AgentLogin::LoggedIn, None, None),
        Bounded::Exited(Some(_), _) => (AgentLogin::LoggedOut, None, None),
        other => (
            AgentLogin::Unknown,
            None,
            Some(failure("codex login status", other, limit)),
        ),
    }
}

/// Why `command` gave no answer (a fixed phrase; never its output).
fn failure(command: &str, out: &Bounded, limit: Duration) -> String {
    match out {
        Bounded::TimedOut => format!("`{command}` took longer than {}s", limit.as_secs()),
        Bounded::TooLarge => format!("`{command}` printed too much"),
        Bounded::NotStarted => format!("`{command}` could not be run"),
        Bounded::Exited(None, _) => format!("`{command}` was killed by a signal"),
        Bounded::Exited(Some(code), _) => format!("`{command}` failed (exit code {code})"),
        Bounded::Enough(_) | Bounded::Failed => format!("`{command}` could not be read"),
    }
}

/// The first non-empty line of `out`, made printable and short.
fn first_line(out: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(out);
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    Some(printable(line, MAX_VERSION)).filter(|l| !l.is_empty())
}

/// Whether `note` is one the probe makes: one of its fixed phrases, or a failure of a login
/// status command ([`failure`]) as it words them.
pub fn is_probe_note(note: &str) -> bool {
    if [
        NOT_ON_PATH,
        NO_STATUS_COMMAND,
        NO_VERSION,
        NO_JSON_STATUS,
        NOT_SAID,
    ]
    .contains(&note)
    {
        return true;
    }
    if BUILTIN_AGENTS
        .iter()
        .any(|(_, _, env)| note.strip_prefix(*env) == Some(" names no executable file"))
    {
        return true;
    }
    STATUS_COMMANDS.iter().any(|command| {
        let Some(rest) = note
            .strip_prefix('`')
            .and_then(|n| n.strip_prefix(command))
            .and_then(|n| n.strip_prefix("` "))
        else {
            return false;
        };
        // A timeout in seconds or an exit code: a handful of digits, never more.
        let digits = |n: &str| (1..=10).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit());
        matches!(
            rest,
            "printed too much"
                | "could not be run"
                | "was killed by a signal"
                | "could not be read"
        ) || rest
            .strip_prefix("took longer than ")
            .and_then(|n| n.strip_suffix('s'))
            .is_some_and(digits)
            || rest
                .strip_prefix("failed (exit code ")
                .and_then(|n| n.strip_suffix(')'))
                .is_some_and(|n| digits(n.strip_prefix('-').unwrap_or(n)))
    })
}

/// A box's report of its agent CLIs as twr keeps it (the hub, from a runner's inventory and
/// `Op::Agents`; `twr box doctor` and `twr box login`, from `twr runner --agents` on the box):
/// the probe's own filters applied again, since the box's twr (of another version, or not
/// twr's) may say anything. Only the built-in agent runtimes ([`BUILTIN_AGENTS`]), once each
/// (its first entry); a sign-in method only for Claude Code and only as [`claude_method`]
/// keeps it (`other` for anything else, an email included); a note only when it is one the
/// probe makes ([`is_probe_note`]), else [`OTHER_NOTE`]; the path and version printable and
/// as short as the probe keeps them.
pub fn kept_report(agents: Vec<AgentInfo>) -> Vec<AgentInfo> {
    let mut kept: Vec<AgentInfo> = Vec::new();
    for mut a in agents {
        let Some((runtime, ..)) = BUILTIN_AGENTS.iter().find(|(r, ..)| *r == a.runtime) else {
            continue;
        };
        if kept.iter().any(|k| k.runtime == *runtime) {
            continue;
        }
        a.path = a.path.map(|p| printable(&p, MAX_PATH));
        a.version = a
            .version
            .map(|v| printable(&v, MAX_VERSION))
            .filter(|v| !v.is_empty());
        a.method = match *runtime {
            crate::claude_code::NAME => a
                .method
                .and_then(|m| claude_method(&serde_json::Value::String(m))),
            _ => None,
        };
        // A note the probe makes is short: a longer one is not one, whatever it says.
        a.note = a
            .note
            .map(|n| match n.len() <= MAX_NOTE && is_probe_note(&n) {
                true => n,
                false => OTHER_NOTE.to_string(),
            });
        kept.push(a);
    }
    kept
}

/// The longest note a box's report may keep ([`kept_report`]): every note the probe makes is
/// far shorter.
const MAX_NOTE: usize = 160;

/// `s` without control characters, trimmed, at most `max` bytes (cut at a character).
pub fn printable(s: &str, max: usize) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    let mut end = clean.len().min(max);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    clean[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The login command runs what the runtime runs: `TWR_*_BIN` first, else the command on
    /// the login shell's PATH, with that PATH; an agent that is not there is said so.
    #[test]
    fn login_commands_run_what_the_runtime_runs() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in ["codex", "my-grok"] {
            let p = dir.path().join(name);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let login = LoginPath::parse(dir.path().to_str().unwrap().as_bytes()).unwrap();
        let cmd = login_command("codex", Some(&login), |_| None).unwrap();
        assert_eq!(cmd.get_program(), dir.path().join("codex").as_os_str());
        let args: Vec<_> = cmd.get_args().collect();
        assert_eq!(args, ["login", "--device-auth"]);
        let path = cmd
            .get_envs()
            .find(|(k, _)| *k == "PATH")
            .and_then(|(_, v)| v)
            .unwrap();
        assert_eq!(
            path,
            login
                .merged(std::env::var_os("PATH").as_deref())
                .as_os_str()
        );
        // Its secrets are removed, as from a runtime.
        assert!(cmd
            .get_envs()
            .any(|(k, v)| k == "TWR_RUNNER_TOKEN" && v.is_none()));
        let pinned = dir.path().join("my-grok");
        let var = |k: &str| (k == "TWR_GROK_BIN").then(|| pinned.clone().into_os_string());
        let cmd = login_command("grok-build", Some(&login), var).unwrap();
        assert_eq!(cmd.get_program(), pinned.as_os_str());
        let e = login_command("claude-code", Some(&login), |k| {
            (k == "TWR_CLAUDE_BIN").then(|| "/nonexistent/claude".into())
        })
        .unwrap_err()
        .to_string();
        assert_eq!(
            e,
            "`/nonexistent/claude` (TWR_CLAUDE_BIN) is not an executable file on this box"
        );
        // (A bare name no PATH has: the real `claude` may be on this one.)
        let e = login_command("claude-code", Some(&login), |k| {
            (k == "TWR_CLAUDE_BIN").then(|| "twr-test-no-such-claude".into())
        })
        .unwrap_err()
        .to_string();
        assert_eq!(
            e,
            "`twr-test-no-such-claude` is not on this box's PATH (the login shell's included): \
             install it there (or point twr at it with TWR_CLAUDE_BIN)"
        );
        assert!(login_command("pty", None, |_| None).is_err());
    }

    /// `--exec` runs the runtime's program as a spawn of the thread would: on the thread's
    /// PATH when it has one (the login PATH then plays no part), else on the login PATH;
    /// with the hub's arguments as given and the thread's other variables set on it.
    #[test]
    fn exec_commands_resolve_as_a_spawn_does() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (thread, login_dir) = (dir.path().join("thread"), dir.path().join("login"));
        for d in [&thread, &login_dir] {
            std::fs::create_dir(d).unwrap();
            let p = d.join("claude");
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let login = LoginPath::parse(login_dir.to_str().unwrap().as_bytes()).unwrap();
        let args = ["--resume".to_string(), "s-1".to_string()];
        let path_of = |c: &std::process::Command| {
            c.get_envs()
                .find(|(k, _)| *k == "PATH")
                .and_then(|(_, v)| v)
                .map(|v| v.to_os_string())
        };
        let c = exec_command("claude-code", &args, None, &[], Some(&login), |_| None).unwrap();
        assert_eq!(c.get_program(), login_dir.join("claude").as_os_str());
        assert_eq!(c.get_args().collect::<Vec<_>>(), ["--resume", "s-1"]);
        assert_eq!(
            path_of(&c).unwrap(),
            login.merged(std::env::var_os("PATH").as_deref())
        );
        let t = thread.to_str().unwrap();
        let c = exec_command("claude-code", &args, Some(t), &[], Some(&login), |_| None).unwrap();
        assert_eq!(c.get_program(), thread.join("claude").as_os_str());
        assert_eq!(path_of(&c).unwrap(), t);
        // The rest of the thread's environment is set on the command, after the program and
        // its PATH are chosen; twr's own variables, PATH, and what is no name are refused.
        let env = [
            ("HOME".to_string(), Some("/elsewhere".to_string())),
            ("SHELL".to_string(), Some("/bin/false".to_string())),
            ("CLAUDE_CODE_SIMPLE".to_string(), None),
        ];
        let c = exec_command("claude-code", &args, None, &env, Some(&login), |_| None).unwrap();
        assert_eq!(c.get_program(), login_dir.join("claude").as_os_str());
        let envs: Vec<_> = c.get_envs().collect();
        assert!(
            envs.contains(&("HOME".as_ref(), Some("/elsewhere".as_ref()))),
            "{envs:?}"
        );
        assert!(
            envs.contains(&("SHELL".as_ref(), Some("/bin/false".as_ref()))),
            "{envs:?}"
        );
        assert!(
            envs.contains(&("CLAUDE_CODE_SIMPLE".as_ref(), None)),
            "{envs:?}"
        );
        assert!(
            envs.contains(&("TWR_RUNNER_TOKEN".as_ref(), None)),
            "{envs:?}"
        );
        assert_eq!(
            path_of(&c).unwrap(),
            login.merged(std::env::var_os("PATH").as_deref())
        );
        for bad in ["TWR_CLAUDE_BIN", "twr_x", "PATH", "A=B", ""] {
            let env = [(bad.to_string(), Some("x".to_string()))];
            assert!(
                exec_command("claude-code", &args, None, &env, Some(&login), |_| None).is_err(),
                "{bad}"
            );
        }
        let e = exec_command(
            "claude-code",
            &args,
            Some("/nonexistent"),
            &[],
            Some(&login),
            |k| (k == "TWR_CLAUDE_BIN").then(|| "twr-test-no-such-claude".into()),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("is not on the thread's PATH"), "{e}");
        assert!(exec_command("acp", &args, None, &[], None, |_| None).is_err());
    }

    #[test]
    fn names_methods_and_strings() {
        assert_eq!(agent_runtime("claude"), Some("claude-code"));
        assert_eq!(agent_runtime("claude-code"), Some("claude-code"));
        assert_eq!(agent_runtime("codex"), Some("codex"));
        assert_eq!(agent_runtime("grok"), Some("grok-build"));
        assert_eq!(agent_runtime("grok-build"), Some("grok-build"));
        assert_eq!(agent_runtime("acp"), None);
        assert_eq!(login_args("claude-code"), Some(&["auth", "login"][..]));
        assert_eq!(login_args("codex"), Some(&["login", "--device-auth"][..]));
        assert_eq!(
            login_args("grok-build"),
            Some(&["login", "--device-auth"][..])
        );
        assert_eq!(login_args("pty"), None);
        for (v, want) in [
            (json!("claude.ai"), Some("claude.ai")),
            (json!("console"), Some("console")),
            (json!("apiKey"), Some("api-key")),
            (json!("api-key"), Some("api-key")),
            (json!("bedrock"), Some("bedrock")),
            (json!("vertex"), Some("vertex")),
            (json!("someone@example.com"), Some("other")),
            (json!("oauth_token"), Some("other")),
            (json!(7), None),
            (json!(null), None),
        ] {
            assert_eq!(claude_method(&v).as_deref(), want, "{v}");
        }
        assert_eq!(printable(" 1.2\u{1b}[31m.3\n", 100), "1.2[31m.3");
        assert_eq!(printable("ééé", 3), "é");
        assert_eq!(
            first_line(b"\n\n  2.1.302 (Claude Code)\nmore"),
            Some("2.1.302 (Claude Code)".into())
        );
        assert_eq!(first_line(b"\n \n"), None);
    }

    /// Every note the probe makes is one [`kept_report`] keeps; anything else a box says is
    /// replaced, a sign-in method kept only for Claude Code and only as a known word, and
    /// only the built-in runtimes are kept, once each.
    #[test]
    fn a_boxs_report_is_filtered_as_the_probe_filters() {
        let limit = Duration::from_secs(10);
        let mut made = vec![
            NOT_ON_PATH.to_string(),
            NO_STATUS_COMMAND.into(),
            NO_VERSION.into(),
            NO_JSON_STATUS.into(),
            NOT_SAID.into(),
        ];
        for (runtime, ..) in BUILTIN_AGENTS {
            made.push(format!("{} names no executable file", bin_env(runtime)));
        }
        for command in STATUS_COMMANDS {
            for out in [
                Bounded::TimedOut,
                Bounded::TooLarge,
                Bounded::NotStarted,
                Bounded::Exited(None, vec![]),
                Bounded::Exited(Some(3), vec![]),
                Bounded::Exited(Some(-1), vec![]),
                Bounded::Enough(vec![]),
                Bounded::Failed,
            ] {
                made.push(failure(command, &out, limit));
            }
        }
        for n in &made {
            assert!(is_probe_note(n), "{n}");
        }
        for n in [
            "/home/alice/.claude/credentials.json",
            "alice@example.com",
            "`claude auth status` took longer than 10 s",
            "`claude auth status` failed (exit code x)",
            "`rm -rf /` could not be run",
            "",
        ] {
            assert!(!is_probe_note(n), "{n}");
        }
        // A probe note's numbers are a handful of digits: a huge one is not a probe note, and
        // a note longer than any the probe makes is not kept whatever it says.
        let huge = format!(
            "`claude auth status` took longer than {}s",
            "9".repeat(1 << 20)
        );
        assert!(!is_probe_note(&huge));
        assert!(!is_probe_note(&format!(
            "`codex login status` failed (exit code {})",
            "1".repeat(11)
        )));
        let long = AgentInfo {
            runtime: "codex".into(),
            found: true,
            path: None,
            version: None,
            login: AgentLogin::Unknown,
            method: None,
            note: Some(huge),
        };
        assert_eq!(kept_report(vec![long])[0].note.as_deref(), Some(OTHER_NOTE));
        let one = |runtime: &str, method: Option<&str>, note: Option<&str>| AgentInfo {
            runtime: runtime.into(),
            found: true,
            path: Some(format!("/bin/{runtime}\u{1b}[2J")),
            version: Some("v".repeat(500)),
            login: AgentLogin::LoggedIn,
            method: method.map(str::to_string),
            note: note.map(str::to_string),
        };
        let kept = kept_report(vec![
            one("evil\u{1b}]0;x\u{7}", None, None),
            one(
                "claude-code",
                Some("alice@example.com"),
                Some("signed in as alice@example.com"),
            ),
            one("claude-code", Some("claude.ai"), None),
            one("codex", Some("claude.ai"), Some(NOT_SAID)),
            one("pty", None, None),
            one("grok-build", None, Some("`grok` took longer than 1s")),
        ]);
        let runtimes: Vec<&str> = kept.iter().map(|a| a.runtime.as_str()).collect();
        assert_eq!(runtimes, ["claude-code", "codex", "grok-build"]);
        assert_eq!(kept[0].method.as_deref(), Some("other"));
        assert_eq!(kept[0].note.as_deref(), Some(OTHER_NOTE));
        assert_eq!(kept[0].path.as_deref(), Some("/bin/claude-code[2J"));
        assert_eq!(kept[0].version.as_deref().map(str::len), Some(MAX_VERSION));
        assert_eq!(kept[1].method, None);
        assert_eq!(kept[1].note.as_deref(), Some(NOT_SAID));
        assert_eq!(kept[2].note.as_deref(), Some(OTHER_NOTE));
        let all = serde_json::to_string(&kept).unwrap();
        assert!(
            !all.contains("example.com") && !all.contains('\u{1b}'),
            "{all}"
        );
    }
}

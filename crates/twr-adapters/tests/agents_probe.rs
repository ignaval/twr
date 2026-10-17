//! The box's agent probe (`twr_adapters::agents`) against fake `claude`, `codex` and `grok`
//! scripts, and the one resolution (`twr_adapters::resolve`) it shares with the spawn: what
//! the probe finds is what Claude Code's capability probe reads and the spawn runs.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use twr_adapters::agents::{probe_with, COMMAND_TIMEOUT};
use twr_adapters::claude_code::ClaudeCodeAdapter;
use twr_adapters::resolve::{with_login_path, LoginPath};
use twr_core::adapter::{Adapter, AdapterEvent, SpawnConfig};
use twr_core::runner::{AgentInfo, AgentLogin};

const EMAIL: &str = "someone.secret@example.com";

/// Creates the executable `path` with `content` without this process holding it open for
/// writing (a fork holding it would make exec'ing it fail with ETXTBSY): `cp` writes it.
fn install(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    let src = path.with_extension("src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A fake CLI `name` in `dir`: `--version` prints `version`, anything else runs `rest`.
fn cli(dir: &Path, name: &str, version: &str, rest: &str) -> PathBuf {
    let p = dir.join(name);
    install(
        &p,
        &format!(
            "#!/bin/sh\nPATH=/usr/bin:/bin\n\
             if [ \"$1\" = --version ]; then echo '{version}'; echo 'second line'; exit 0; fi\n\
             {rest}\n"
        ),
    );
    p
}

/// `claude auth status` answering `json` with exit `code`, as Claude Code 2.1.302 does
/// (account details included).
fn claude_status(json: &str, code: i32) -> String {
    format!(
        "if [ \"$1 $2\" = 'auth status' ]; then cat <<'EOF'\n{json}\nEOF\nexit {code}; fi\nexit 9"
    )
}

fn env_of(dir: &Path) -> Vec<(String, String)> {
    vec![("PATH".into(), dir.display().to_string())]
}

fn no_var(_: &str) -> Option<OsString> {
    None
}

fn by_runtime<'a>(agents: &'a [AgentInfo], runtime: &str) -> &'a AgentInfo {
    agents.iter().find(|a| a.runtime == runtime).unwrap()
}

/// Signed in: the version's first line, Claude Code's method from its short list, and nothing
/// else of `auth status` (the email is in neither the info nor its Debug output). Grok has no
/// status command.
#[tokio::test]
async fn signed_in_agents_and_nothing_more() {
    let dir = tempfile::tempdir().unwrap();
    let json = format!(
        r#"{{"loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
            "email": "{EMAIL}", "orgId": "org-123", "orgName": "Secret Org",
            "subscriptionType": "max"}}"#
    );
    cli(
        dir.path(),
        "claude",
        "2.1.302 (Claude Code)",
        &claude_status(&json, 0),
    );
    cli(
        dir.path(),
        "codex",
        "codex-cli 0.160.0",
        "[ \"$1 $2\" = 'login status' ] && { echo 'Logged in using ChatGPT'; exit 0; }; exit 9",
    );
    cli(dir.path(), "grok", "grok 1.0.46", "exit 0");
    let agents = probe_with(&env_of(dir.path()), no_var, COMMAND_TIMEOUT).await;
    assert_eq!(
        agents
            .iter()
            .map(|a| a.runtime.as_str())
            .collect::<Vec<_>>(),
        ["claude-code", "codex", "grok-build"]
    );
    let claude = by_runtime(&agents, "claude-code");
    assert_eq!(
        *claude,
        AgentInfo {
            runtime: "claude-code".into(),
            found: true,
            path: Some(dir.path().join("claude").display().to_string()),
            version: Some("2.1.302 (Claude Code)".into()),
            login: AgentLogin::LoggedIn,
            method: Some("claude.ai".into()),
            note: None,
        }
    );
    let codex = by_runtime(&agents, "codex");
    assert_eq!(
        (
            codex.login,
            codex.version.as_deref(),
            &codex.method,
            &codex.note
        ),
        (
            AgentLogin::LoggedIn,
            Some("codex-cli 0.160.0"),
            &None,
            &None
        )
    );
    let grok = by_runtime(&agents, "grok-build");
    assert_eq!(grok.login, AgentLogin::Unknown);
    assert_eq!(grok.version.as_deref(), Some("grok 1.0.46"));
    assert_eq!(grok.note.as_deref(), Some("it has no login status command"));
    for shown in [
        format!("{agents:?}"),
        serde_json::to_string(&agents).unwrap(),
    ] {
        for secret in [EMAIL, "org-123", "Secret Org", "firstParty"] {
            assert!(!shown.contains(secret), "{secret} in {shown}");
        }
    }
}

/// Signed out: Claude Code's `loggedIn: false` (exit 1), Codex's non-zero exit; an unknown
/// method word is `other`.
#[tokio::test]
async fn signed_out_agents() {
    let dir = tempfile::tempdir().unwrap();
    cli(
        dir.path(),
        "claude",
        "2.1.302 (Claude Code)",
        &claude_status(r#"{"loggedIn": false, "authMethod": "none"}"#, 1),
    );
    cli(
        dir.path(),
        "codex",
        "codex-cli 0.160.0",
        "echo 'Not logged in'; exit 1",
    );
    let agents = probe_with(&env_of(dir.path()), no_var, COMMAND_TIMEOUT).await;
    let claude = by_runtime(&agents, "claude-code");
    assert_eq!(
        (claude.login, &claude.method),
        (AgentLogin::LoggedOut, &None)
    );
    assert_eq!(by_runtime(&agents, "codex").login, AgentLogin::LoggedOut);
    // Not installed: not found, and why.
    let grok = by_runtime(&agents, "grok-build");
    assert!(!grok.found && grok.path.is_none() && grok.version.is_none());
    assert_eq!(grok.note.as_deref(), Some("not found on PATH"));

    let other = tempfile::tempdir().unwrap();
    cli(
        other.path(),
        "claude",
        "2.1.302 (Claude Code)",
        &claude_status(
            &format!(r#"{{"loggedIn": true, "authMethod": "{EMAIL}"}}"#),
            0,
        ),
    );
    let agents = probe_with(&env_of(other.path()), no_var, COMMAND_TIMEOUT).await;
    let claude = by_runtime(&agents, "claude-code");
    assert_eq!(claude.method.as_deref(), Some("other"));
    assert!(!format!("{agents:?}").contains(EMAIL));
}

/// A status command that fails, prints no JSON, is too slow, or prints too much: unknown with
/// a fixed reason, never its output; each command bounded.
#[tokio::test]
async fn failing_status_commands_are_unknown() {
    let limit = Duration::from_millis(1500);
    let cases: &[(&str, &str, &str)] = &[
        (
            "echo \"boom {EMAIL}\"; exit 2",
            "boom",
            "`claude auth status` failed (exit code 2)",
        ),
        (
            "echo \"not json {EMAIL}\"; exit 0",
            "not json",
            "`claude auth status` printed no JSON status",
        ),
        (
            "echo '{\"email\": \"x\"}'; exit 0",
            "email",
            "`claude auth status` did not say whether it is signed in",
        ),
        (
            "exec sleep 30",
            "sleep",
            "`claude auth status` took longer than 1s",
        ),
        (
            "yes \"{EMAIL}\" | head -c 300000; exit 0",
            "yes",
            "`claude auth status` printed too much",
        ),
    ];
    for (body, label, why) in cases {
        let dir = tempfile::tempdir().unwrap();
        cli(
            dir.path(),
            "claude",
            "2.1.302 (Claude Code)",
            &body.replace("{EMAIL}", EMAIL),
        );
        cli(dir.path(), "codex", "codex-cli 0.160.0", "exec sleep 30");
        let started = std::time::Instant::now();
        let agents = probe_with(&env_of(dir.path()), no_var, limit).await;
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "{label}: {:?}",
            started.elapsed()
        );
        let claude = by_runtime(&agents, "claude-code");
        assert!(claude.found, "{label}");
        assert_eq!(claude.login, AgentLogin::Unknown, "{label}");
        assert_eq!(claude.note.as_deref(), Some(*why), "{label}");
        assert!(!format!("{agents:?}").contains(EMAIL), "{label}");
        let codex = by_runtime(&agents, "codex");
        assert_eq!(codex.login, AgentLogin::Unknown, "{label}");
        assert_eq!(
            codex.note.as_deref(),
            Some("`codex login status` took longer than 1s"),
            "{label}"
        );
    }
    // A version that prints nothing, or fails: no version, and that is the note.
    let dir = tempfile::tempdir().unwrap();
    install(&dir.path().join("grok"), "#!/bin/sh\nexit 3\n");
    let agents = probe_with(&env_of(dir.path()), no_var, limit).await;
    let grok = by_runtime(&agents, "grok-build");
    assert!(grok.found && grok.version.is_none());
}

/// `TWR_*_BIN` wins over the PATH: a path as given, one that names no executable said so.
#[tokio::test]
async fn bin_overrides_win() {
    let dir = tempfile::tempdir().unwrap();
    let on_path = dir.path().join("path");
    let pinned = dir.path().join("pinned");
    std::fs::create_dir_all(&on_path).unwrap();
    std::fs::create_dir_all(&pinned).unwrap();
    cli(
        &on_path,
        "claude",
        "1.0.0 (on PATH)",
        &claude_status(r#"{"loggedIn": false}"#, 1),
    );
    let mine = cli(
        &pinned,
        "my-claude",
        "2.0.0 (pinned)",
        &claude_status(r#"{"loggedIn": true, "authMethod": "apiKey"}"#, 0),
    );
    let var = |k: &str| match k {
        "TWR_CLAUDE_BIN" => Some(mine.clone().into_os_string()),
        "TWR_CODEX_BIN" => Some(OsString::from("/nonexistent/codex")),
        _ => None,
    };
    let agents = probe_with(&env_of(&on_path), var, COMMAND_TIMEOUT).await;
    let claude = by_runtime(&agents, "claude-code");
    assert_eq!(claude.path.as_deref(), Some(mine.to_str().unwrap()));
    assert_eq!(claude.version.as_deref(), Some("2.0.0 (pinned)"));
    assert_eq!(claude.method.as_deref(), Some("api-key"));
    let codex = by_runtime(&agents, "codex");
    assert!(!codex.found);
    assert_eq!(
        codex.note.as_deref(),
        Some("TWR_CODEX_BIN names no executable file")
    );
}

/// The probe and the spawn get the same binary: the thread's `PATH` (or the one a runner put
/// in its environment from the login shell) is where Claude Code's capability probe reads
/// `--help` and where the session runs from, and where the agent probe looks. A relative
/// `PATH` entry is the thread's directory's, for the capability probe as for the session,
/// though the test (the runner) runs elsewhere.
#[tokio::test]
async fn the_probe_and_the_spawn_get_the_same_binary() {
    for mode in ["thread", "login", "relative"] {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let d = dir.path().display();
        install(
            &bin.join("claude"),
            &format!(
                "#!/bin/sh\nPATH=/usr/bin:/bin\ncase \"$1\" in\n\
                 --help) echo help >> {d}/calls; exit 0;;\n\
                 --version) echo '2.1.302 (Claude Code)'; exit 0;;\n\
                 auth) echo '{{\"loggedIn\": true, \"authMethod\": \"console\"}}'; exit 0;;\n\
                 esac\n\
                 echo \"run $0\" >> {d}/calls\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}}'\n\
                 cat > /dev/null\n"
            ),
        );
        // The thread's PATH, or none and the login shell's put in by the runner.
        let mut env = Vec::new();
        let login_path = LoginPath::parse(bin.to_str().unwrap().as_bytes()).unwrap();
        match mode {
            "login" => with_login_path(&mut env, Some(&login_path)),
            "thread" => env = env_of(&bin),
            _ => env = vec![("PATH".into(), "bin:/usr/bin:/bin".into())],
        }
        assert_ne!(std::env::current_dir().unwrap(), dir.path());
        // (The box-wide probe has no thread directory: it looks only where entries are
        // absolute.)
        if mode != "relative" {
            let agents = probe_with(&env, no_var, COMMAND_TIMEOUT).await;
            let claude = by_runtime(&agents, "claude-code");
            assert_eq!(
                claude.path.as_deref(),
                Some(bin.join("claude").to_str().unwrap())
            );
            assert_eq!(claude.method.as_deref(), Some("console"));
        }

        let cfg = SpawnConfig {
            env: env.clone(),
            cwd: dir.path().to_path_buf(),
            ..Default::default()
        };
        // `claude` as named, whatever `TWR_CLAUDE_BIN` this test inherits: the fixture on the
        // test's PATH, as the probe above (no overrides) found it.
        let mut h = ClaudeCodeAdapter::with_bin("claude")
            .stop_grace(Duration::from_millis(300))
            .spawn(cfg, None)
            .unwrap();
        let mut rx = h.take_events().unwrap();
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                match rx.recv().await {
                    Some(AdapterEvent::Emit { body, .. }) if body.kind() == "thread.started" => {
                        break
                    }
                    Some(AdapterEvent::Exited { stderr_tail, .. }) => {
                        panic!("exited: {stderr_tail}")
                    }
                    Some(_) => {}
                    None => panic!("no events"),
                }
            }
        })
        .await
        .expect("the session started");
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        let calls: Vec<&str> = calls.lines().collect();
        assert_eq!(calls[0], "help", "{mode}: {calls:?}");
        assert!(
            calls.iter().any(|c| c.starts_with("run ")),
            "{mode}: {calls:?}"
        );
        drop(h);
    }
}

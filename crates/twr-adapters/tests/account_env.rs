//! A named account's removals (`SpawnConfig::env_remove`) reach every command twr builds for
//! a runtime: the attach commands of each adapter (removed after the thread's variables, so a
//! name in both is removed) and a spawned process.

use std::ffi::OsStr;
use std::path::PathBuf;

use twr_adapters::{acp, AcpAdapter, ClaudeCodeAdapter, FakeAdapter, GrokBuildAdapter};
use twr_core::adapter::{Adapter, SpawnConfig};

/// A thread on a named Claude account, with a stray key in its configured variables.
fn cfg() -> SpawnConfig {
    SpawnConfig {
        thread: "T1".into(),
        cwd: PathBuf::from("/work/repo"),
        mode: "manual".into(),
        env: vec![
            ("CLAUDE_CONFIG_DIR".into(), "/acct/work".into()),
            ("ANTHROPIC_API_KEY".into(), "stray".into()),
        ],
        env_remove: vec!["ANTHROPIC_API_KEY".into(), "CLAUDE_CODE_OAUTH_TOKEN".into()],
        ..Default::default()
    }
}

/// The account's variable is set and each removal is removed (`(key, None)`).
fn assert_removed(cmd: &std::process::Command, what: &str) {
    let envs: Vec<_> = cmd.get_envs().collect();
    assert!(
        envs.contains(&(
            OsStr::new("CLAUDE_CONFIG_DIR"),
            Some(OsStr::new("/acct/work"))
        )),
        "{what}: {envs:?}"
    );
    for k in ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]
        .iter()
        .chain(twr_adapters::process::SCRUBBED_ENV)
    {
        assert!(
            envs.contains(&(OsStr::new(k), None)),
            "{what}: {k}: {envs:?}"
        );
    }
}

#[test]
fn claude_interactive_removes_the_stray_credentials() {
    let a = ClaudeCodeAdapter::with_bin("/opt/claude");
    assert_removed(&a.attach_command(&cfg(), "s-1").unwrap(), "attach");
    assert_removed(&a.remote_attach_command(&cfg(), "s-1").unwrap(), "remote");
}

#[test]
fn codex_resume_command_removes_the_stray_credentials() {
    let a = twr_adapters::codex::CodexAdapter::with_bin("/opt/codex");
    assert_removed(&a.attach_command(&cfg(), "th-1").unwrap(), "attach");
    assert_removed(&a.remote_attach_command(&cfg(), "th-1").unwrap(), "remote");
}

#[test]
fn grok_attach_removes_the_stray_credentials() {
    let a = GrokBuildAdapter::with_bin("/opt/grok");
    assert_removed(&a.attach_command(&cfg(), "s-1").unwrap(), "attach");
}

#[test]
fn acp_attach_removes_the_stray_credentials() {
    let mut c = cfg();
    c.env.push((
        acp::ATTACH_ENV.into(),
        serde_json::json!(["gemini", "--resume", "{session}"]).to_string(),
    ));
    let cmd = AcpAdapter::new().attach_command(&c, "S1").unwrap();
    assert_removed(&cmd, "acp");
}

#[test]
fn fake_attach_removes_the_stray_credentials() {
    let cmd = FakeAdapter::new().attach_command(&cfg(), "S1").unwrap();
    assert_removed(&cmd, "fake");
}

/// A spawned ACP agent (the launch Grok Build's spawn goes through too) does not see a removed
/// variable, set or inherited, and sees the account's variable.
#[tokio::test]
async fn a_spawned_process_does_not_see_a_removed_variable() {
    use tokio::io::AsyncReadExt;
    let dir = tempfile::tempdir().unwrap();
    let mut child = acp::driver::launch(
        &[],
        std::path::Path::new("/bin/sh"),
        &[
            "-c".into(),
            "printf '%s|%s' \"${ANTHROPIC_API_KEY-unset}\" \"$CLAUDE_CONFIG_DIR\"".into(),
        ],
        &cfg().env,
        &cfg().env_remove,
        dir.path(),
        "install sh",
    )
    .unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .await
        .unwrap();
    child.wait().await.unwrap();
    assert_eq!(out, "unset|/acct/work");
}

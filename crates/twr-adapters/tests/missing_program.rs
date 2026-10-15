//! A runtime whose agent CLI is not installed fails to start with an error
//! that names the program first and says what to do (install it, or the `TWR_*_BIN` override /
//! the runtime's command), for every adapter that starts a program.

use std::sync::Arc;

use twr_adapters::{acp, claude_code, codex, grok_build, pty};
use twr_core::adapter::{Adapter, SpawnConfig};

const MISSING: &str = "twr-test-no-such-cli";

fn start_error(adapter: Arc<dyn Adapter>, env: Vec<(String, String)>) -> String {
    let dir = tempfile::tempdir().unwrap();
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        env,
        ..Default::default()
    };
    match adapter.spawn(cfg, None) {
        Ok(_) => panic!("{} started a missing program", adapter.info().name),
        Err(e) => format!("{e:#}"),
    }
}

#[tokio::test]
async fn a_missing_agent_cli_is_named_with_what_to_do() {
    let argv = format!("[\"{MISSING}\", \"--flag\"]");
    for (adapter, env, fix) in [
        (
            Arc::new(claude_code::ClaudeCodeAdapter::with_bin(MISSING)) as Arc<dyn Adapter>,
            vec![],
            "(install Claude Code, or set TWR_CLAUDE_BIN)",
        ),
        (
            Arc::new(codex::CodexAdapter::with_bin(MISSING)),
            vec![],
            "(install Codex, or set TWR_CODEX_BIN)",
        ),
        (
            Arc::new(grok_build::GrokBuildAdapter::with_bin(MISSING)),
            vec![],
            "(install Grok Build, or set TWR_GROK_BIN)",
        ),
        (
            Arc::new(acp::AcpAdapter::new()),
            vec![(acp::COMMAND_ENV.to_string(), argv.clone())],
            "(install the agent, or fix the runtime's command",
        ),
        (
            Arc::new(pty::PtyAdapter::new()),
            vec![(pty::COMMAND_ENV.to_string(), argv.clone())],
            "(install it, or fix the command after `pty:`)",
        ),
    ] {
        let name = adapter.info().name;
        let e = start_error(adapter, env);
        let want = format!("`{MISSING}` not found on PATH {fix}");
        assert!(e.starts_with(&want), "{name}: {e}");
    }
}

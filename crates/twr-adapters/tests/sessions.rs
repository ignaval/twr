//! Every headless runtime (Claude Code, Codex, Grok Build, an ACP agent) starts in a session
//! of its own, without a controlling terminal: a runtime, or a command it runs, that reads
//! the terminal of whoever started the hub gets an error instead of being stopped by it
//! (SIGTTIN), and its process group is its own (its pid), which twr kills as a whole. Each
//! runtime here is a `sh` stub that records its own `/proc/<pid>/stat`.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use twr_adapters::{acp, claude_code, codex, grok_build};
use twr_core::adapter::{Adapter, SpawnConfig};

const WAIT: Duration = Duration::from_secs(20);

/// Creates the executable `path` with `content` without this process ever holding it open for
/// writing (a fork holding it would make exec'ing it fail with ETXTBSY): `cp` writes it.
fn install_executable(path: &Path, content: &str) {
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

/// A runtime stub `dir/<name>` that answers Claude Code's `--help` / `--version` probe, else
/// writes its own `/proc/<pid>/stat` to `dir/<name>.stat` and waits for its stdin to close.
fn stub(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    let d = dir.display();
    install_executable(
        &path,
        &format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --help) exit 0;;\n\
             --version) echo '2.1.287 (Claude Code)'; exit 0;;\n\
             esac\n\
             cat /proc/$$/stat > {d}/{name}.stat.part && mv {d}/{name}.stat.part {d}/{name}.stat\n\
             exec cat > /dev/null\n"
        ),
    );
    path
}

/// The process id, process group, session and controlling terminal (`tty_nr`) a
/// `/proc/<pid>/stat` line says.
fn ids(stat: &str) -> (i64, i64, i64, i64) {
    let pid = stat.split_whitespace().next().unwrap().parse().unwrap();
    // After the command name, which may hold spaces: state, ppid, pgrp, session, tty_nr.
    let rest: Vec<i64> = stat[stat.rfind(')').unwrap() + 1..]
        .split_whitespace()
        .skip(2)
        .take(3)
        .map(|f| f.parse().unwrap())
        .collect();
    (pid, rest[0], rest[1], rest[2])
}

async fn stat_of(file: &Path) -> String {
    let until = Instant::now() + WAIT;
    loop {
        if let Ok(s) = std::fs::read_to_string(file) {
            return s;
        }
        assert!(
            Instant::now() < until,
            "{} was never written",
            file.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A runtime: the name its stub records its stat under, its adapter, and the environment it
/// is spawned with.
type Runtime = (&'static str, Arc<dyn Adapter>, Vec<(String, String)>);

#[tokio::test]
async fn every_runtime_starts_in_a_session_of_its_own_without_a_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let acp_argv = json!([stub(d, "acp-agent")]).to_string();
    let runtimes: Vec<Runtime> = vec![
        (
            "claude",
            Arc::new(claude_code::ClaudeCodeAdapter::with_bin(stub(d, "claude"))),
            vec![],
        ),
        (
            "codex",
            Arc::new(codex::CodexAdapter::with_bin(stub(d, "codex"))),
            vec![],
        ),
        (
            "grok",
            Arc::new(grok_build::GrokBuildAdapter::with_bin(stub(d, "grok"))),
            vec![],
        ),
        (
            "acp-agent",
            Arc::new(acp::AcpAdapter::new()),
            vec![(acp::COMMAND_ENV.to_string(), acp_argv)],
        ),
    ];
    for (name, adapter, env) in runtimes {
        let cfg = SpawnConfig {
            thread: "t1".into(),
            cwd: d.to_path_buf(),
            env,
            mode: "manual".into(),
            ..Default::default()
        };
        let handle = adapter
            .spawn(cfg, None)
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        let stat = stat_of(&d.join(format!("{name}.stat"))).await;
        let (pid, pgrp, session, tty) = ids(&stat);
        // SAFETY: kill(2) of the stub's group (its pid), which is still this process's child.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        drop(handle);
        assert_eq!(session, pid, "{name} leads a session of its own: {stat}");
        assert_eq!(pgrp, pid, "{name} leads a process group of its own: {stat}");
        assert_eq!(tty, 0, "{name} has no controlling terminal: {stat}");
    }
}

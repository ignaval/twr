//! Opt-in limits for local workers (`[threads] cpu_weight`, `memory_high`; `hub::limits`):
//! the hub starts each local worker's runtime under `systemd-run --user --scope` in
//! `twr.slice`, whose CPU weight it sets. Here `systemd-run` and `systemctl` are stand-ins that
//! write down how they were called (and `systemd-run` runs the rest of its command line in its
//! place, as the real one does); the worker is the scripted ACP stub of the adapter tests
//! (python3). One ignored test, [`real_systemd_scope`], uses the real ones.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use twr_adapters::acp::{self, AcpAdapter};
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::{Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

fn stub() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../twr-adapters/tests/fixtures/acp/stub_agent.py")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// A stand-in for `name` in `dir` that appends its arguments to `<dir>/<name>.log`, then runs
/// `then`.
fn tool(dir: &Path, name: &str, then: &str) -> PathBuf {
    let path = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\n{then}\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn log(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{name}.log"))).unwrap_or_default()
}

fn opts(dir: &Path, config: Config) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == acp::NAME).then(|| {
            Arc::new(AcpAdapter::new().stop_grace(Duration::from_secs(1))) as Arc<dyn Adapter>
        })
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.stop_timeout = Duration::from_secs(5);
    let run = tool(
        dir,
        "systemd-run",
        "while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"",
    );
    let ctl = tool(dir, "systemctl", "exit 0");
    o.systemd_tools = Some((run, ctl));
    o
}

/// Runs one turn on an ACP stub worker; its final text.
async fn one_turn(hub: &Hub, dir: &Path) -> String {
    let state = dir.join("agent");
    std::fs::create_dir_all(&state).unwrap();
    let t = hub
        .new_thread(
            NewThread {
                project: dir.to_string_lossy().into_owned(),
                runtime: Some(format!("acp:python3 '{}' {}", stub(), state.display())),
                name: Some("gem".into()),
                no_worktree: true,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hello", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let mut text = String::new();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            match &ev.body {
                EventBody::AgentText(t) if t.is_final => text.push_str(&t.text),
                EventBody::TurnEnded(_) => return,
                _ => {}
            }
        }
    })
    .await
    .expect("the turn ends");
    text
}

#[tokio::test]
async fn a_local_worker_runs_in_a_scope_of_the_workers_slice() {
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.threads.cpu_weight = Some(20);
    config.threads.memory_high = Some("1G".into());
    let hub = Hub::open(opts(&dir, config)).await.unwrap();
    assert_eq!(hub.info()["worker_limits"], "systemd");
    // The slice's weight, once.
    assert_eq!(
        log(&dir, "systemctl").trim(),
        "--user set-property --runtime twr.slice CPUWeight=20"
    );
    assert_eq!(one_turn(&hub, &dir).await, "Hello!");
    let calls = log(&dir, "systemd-run");
    let calls: Vec<&str> = calls.lines().collect();
    // The check when the hub opened, then the worker.
    assert_eq!(calls.len(), 2, "{calls:?}");
    let wrap = "--user --scope --quiet --collect --expand-environment=no --slice=twr.slice \
                --description=twr worker -p MemoryHigh=1G --";
    assert_eq!(calls[0], format!("{wrap} true"));
    assert!(
        calls[1].starts_with(&format!("{wrap} python3 ")),
        "{calls:?}"
    );
    assert!(calls[1].contains(&stub()), "{calls:?}");
    hub.shutdown().await;
}

/// A worker's own `XDG_RUNTIME_DIR` (`[threads] env`) is its runtime's: `systemd-run` keeps
/// the hub's, by which it finds the user manager.
#[tokio::test]
async fn a_workers_runtime_dir_does_not_reach_systemd_run() {
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.threads.memory_high = Some("1G".into());
    config
        .threads
        .env
        .insert("XDG_RUNTIME_DIR".into(), "/tmp/agent-private".into());
    let mut o = opts(&dir, config);
    let seen = dir.join("run-env.log");
    let run = tool(
        &dir,
        "systemd-run",
        &format!(
            "echo \"${{XDG_RUNTIME_DIR-unset}}\" >> '{}'\n\
             while [ \"$1\" != -- ]; do shift; done; shift; exec \"$@\"",
            seen.display()
        ),
    );
    o.systemd_tools = Some((run, o.systemd_tools.take().unwrap().1));
    let hub = Hub::open(o).await.unwrap();
    assert_eq!(one_turn(&hub, &dir).await, "Hello!");
    let hub_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "unset".into());
    let seen = std::fs::read_to_string(&seen).unwrap();
    assert!(
        seen.lines().count() == 2 && seen.lines().all(|l| l == hub_dir),
        "{seen}"
    );
    let calls = log(&dir, "systemd-run");
    let worker = calls.lines().nth(1).unwrap();
    assert!(
        worker.contains("-- ")
            && worker.contains("/env XDG_RUNTIME_DIR=/tmp/agent-private python3 "),
        "{worker}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn without_limits_nothing_runs_under_systemd() {
    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    let config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    let hub = Hub::open(opts(&dir, config)).await.unwrap();
    assert!(hub.info()["worker_limits"].is_null());
    assert_eq!(one_turn(&hub, &dir).await, "Hello!");
    assert_eq!(log(&dir, "systemd-run"), "");
    assert_eq!(log(&dir, "systemctl"), "");
    hub.shutdown().await;
}

/// `systemctl --user show <unit> -p <property> --value`.
#[cfg(target_os = "linux")]
fn show(unit: &str, property: &str) -> String {
    let out = std::process::Command::new("systemctl")
        .args(["--user", "show", unit, "-p", property, "--value"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "systemctl --user show {unit}: {out:?}"
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Puts the weight of [`twr_core::config::WORKERS_SLICE`] back as it was before
/// [`real_systemd_scope`] set it on the user's manager, assertion failure or not: a number or
/// `idle` as it was, and a weight that wasn't set (`[not set]`, empty) reset with an empty
/// assignment.
#[cfg(target_os = "linux")]
struct RestoreWeight(String);

#[cfg(target_os = "linux")]
impl Drop for RestoreWeight {
    fn drop(&mut self) {
        let was = self.0.trim();
        let value = if was == "idle" || was.parse::<u64>().is_ok() {
            was.to_string()
        } else {
            String::new()
        };
        let _ = std::process::Command::new("systemctl")
            .args([
                "--user",
                "set-property",
                "--runtime",
                twr_core::config::WORKERS_SLICE,
            ])
            .arg(format!("CPUWeight={value}"))
            .status();
    }
}

/// The limits under the real `systemd-run` and `systemctl` (CI's real-systemd job; locally
/// `cargo test -p twr-hub --test limits -- --ignored real_systemd`): one worker under
/// `cpu_weight` and `memory_high`, its runtime the ACP stub, which keeps running between
/// turns, so its scope is there to inspect. It sets the weight of `twr.slice` on the user's
/// own manager while it runs (the old value goes back afterwards). Where `systemd-run --user
/// --scope` doesn't work (no user manager), it says so and passes, unless
/// `TWR_TEST_REQUIRE_SYSTEMD` is set.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "uses the user's systemd manager"]
async fn real_systemd_scope() {
    use std::process::{Command, Stdio};
    use twr_core::config::WORKERS_SLICE;

    let usable = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "--collect", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !usable {
        // CI's real-systemd job sets it once a user manager answers: a skip there would hide
        // a broken check.
        assert!(
            std::env::var_os("TWR_TEST_REQUIRE_SYSTEMD").is_none(),
            "`systemd-run --user --scope true` fails, and TWR_TEST_REQUIRE_SYSTEMD is set"
        );
        eprintln!(
            "skipped: `systemd-run --user --scope true` fails here (no systemd user manager)"
        );
        return;
    }
    let before = show(WORKERS_SLICE, "CPUWeight");
    let _restore = RestoreWeight(before.clone());
    // Not the weight it had, so the check below sees the hub's.
    let weight: u64 = if before == "37" { 38 } else { 37 };

    let dir = tempfile::tempdir().unwrap();
    let dir = std::fs::canonicalize(dir.path()).unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.threads.cpu_weight = Some(weight);
    config.threads.memory_high = Some("512M".into());
    let mut o = opts(&dir, config);
    o.systemd_tools = None;
    let hub = Hub::open(o).await.unwrap();
    assert_eq!(hub.info()["worker_limits"], "systemd");
    assert_eq!(one_turn(&hub, &dir).await, "Hello!");

    // The stub's pid (it writes run.json when it starts), and its cgroup: under cgroup v2 one
    // line `0::<path>`, under v1 the `name=systemd` hierarchy's. The scope is the last part.
    let run: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("agent/run.json")).unwrap())
            .unwrap();
    let pid = run["pid"].as_u64().unwrap();
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let path = cgroup
        .lines()
        .find_map(|l| {
            l.strip_prefix("0::")
                .or_else(|| l.split_once(":name=systemd:").map(|(_, p)| p))
        })
        .unwrap_or_else(|| panic!("no systemd cgroup for {pid}: {cgroup}"));
    let scope = path.rsplit('/').next().unwrap();
    assert!(
        scope.ends_with(".scope") && path.contains(&format!("/{WORKERS_SLICE}/")),
        "the worker {pid} runs in {path}"
    );
    assert_eq!(show(scope, "Slice"), WORKERS_SLICE);
    assert_eq!(show(scope, "MemoryHigh"), (512u64 << 20).to_string());
    assert_eq!(show(WORKERS_SLICE, "CPUWeight"), weight.to_string());
    hub.shutdown().await;
}

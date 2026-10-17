//! A remote attach on an SSH box runs the agent through the box's own twr (`twr runner
//! --exec`, the command `twr_hub::remote::ssh_exec_command` builds), and that finds the same
//! program, with the same `PATH` and environment, as a spawn of the thread by the box's daemon
//! (an in-process `LocalBox` here): the login shell is asked with the box's own environment
//! by both, never with the thread's (whose `HOME` or `SHELL` would pick other startup files
//! or no login shell at all), and the thread's environment reaches the agent alone. The
//! attach's remote part runs under `sh` as the box's shell would, with this build of twr as
//! the box's build.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use twr_core::adapter::Adapter;
use twr_core::runner::{AttachInfo, Op, WireSpawnConfig};
use twr_runner::{BoxEnv, LocalBox, LocalBoxOptions};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const BUILD: &str = "0123456789abcdef";

/// Creates the executable `path` with `content`, written by `cp` (a fork of this process
/// holding it open for writing would make exec'ing it fail with ETXTBSY).
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

/// What the fake `claude` wrote to `out`: its program, `PATH`, `HOME` and `SHELL`.
fn ran(out: &Path) -> Vec<String> {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(out) {
            if s.ends_with("end\n") {
                return s.lines().map(str::to_string).collect();
            }
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "{} was never written",
            out.display()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_attach_runs_what_a_spawn_runs_whatever_the_threads_home_and_shell() {
    let dir = tempfile::tempdir().unwrap();
    let path = |p: &str| dir.path().join(p);
    for d in ["bin", "box-home", "thread-home", "shell", "work", "data"] {
        std::fs::create_dir_all(path(d)).unwrap();
    }
    // The agent: only on the PATH the box's login shell gives (its profile, in the box's
    // HOME, puts `bin` first). It writes where it ran from and with what to `$OUT`.
    install(
        &path("bin/claude"),
        "#!/bin/sh\ncase \"$1\" in --help) exit 0;; --version) echo '2.1.302 (Claude Code)'; \
         exit 0;; esac\nprintf '%s\\n' \"$0\" \"$PATH\" \"$HOME\" \"$SHELL\" end > \"$OUT\"\n",
    );
    std::fs::write(
        path("box-home/profile"),
        format!("PATH='{}':\"$PATH\"; export PATH\n", path("bin").display()),
    )
    .unwrap();
    // A login shell (asked as bash is) that reads the profile of the HOME it runs with.
    install(
        &path("shell/bash"),
        "#!/bin/sh\n[ -f \"$HOME/profile\" ] && . \"$HOME/profile\"\n\
         shift; script=$1; shift; exec /bin/sh -c \"$script\" \"$@\"\n",
    );
    // The daemon's baseline environment has the box's HOME.
    install(
        &path("shell/daemon-bash"),
        &format!(
            "#!/bin/sh\nHOME='{}' exec '{}' \"$@\"\n",
            path("box-home").display(),
            path("shell/bash").display()
        ),
    );
    // This build as the box's.
    let build = path(&format!("data/twr/runner/builds/{BUILD}"));
    std::fs::create_dir_all(&build).unwrap();
    std::os::unix::fs::symlink(TWR, build.join("twr")).unwrap();

    let mut o = LocalBoxOptions::new(&path("data"), "gpu");
    o.env = BoxEnv {
        login_shell: Some(twr_adapters::resolve::LoginShell::Posix(path(
            "shell/daemon-bash",
        ))),
        probe_agents: false,
        bin_vars: Some(Vec::new()),
    };
    // `claude` as named, whatever `TWR_CLAUDE_BIN` this test inherits.
    o.adapters = Arc::new(|n: &str| {
        (n == twr_adapters::claude_code::NAME).then(|| {
            Arc::new(twr_adapters::claude_code::ClaudeCodeAdapter::with_bin(
                "claude",
            )) as Arc<dyn Adapter>
        })
    });
    let lbox = LocalBox::open(o).unwrap();

    let thread_path = format!("{}:/usr/bin:/bin", path("bin").display());
    for (case, own_path) in [("login", None), ("thread", Some(thread_path.as_str()))] {
        let env_of = |out: &Path| {
            let mut env = vec![
                (
                    "HOME".to_string(),
                    path("thread-home").display().to_string(),
                ),
                ("OUT".to_string(), out.display().to_string()),
                ("SHELL".to_string(), "/bin/false".to_string()),
            ];
            env.extend(own_path.map(|p| ("PATH".to_string(), p.to_string())));
            env
        };
        // The daemon's spawn.
        let spawned = path(&format!("spawn-{case}"));
        let r = lbox
            .handle_cmd(
                &format!("s-{case}"),
                Op::Spawn {
                    thread: format!("t-{case}"),
                    runtime: twr_adapters::claude_code::NAME.into(),
                    cfg: WireSpawnConfig {
                        cwd: path("work"),
                        env: env_of(&spawned),
                        ..Default::default()
                    },
                    prompt: None,
                    run: None,
                },
            )
            .await;
        r.into_outcome().unwrap();
        let spawn = ran(&spawned);

        // The attach, as the hub builds it, run as the box's shell runs it.
        let attached = path(&format!("attach-{case}"));
        let mut env: Vec<(String, Option<String>)> = env_of(&attached)
            .into_iter()
            .map(|(k, v)| (k, Some(v)))
            .collect();
        env.push(("TWR_RUNNER_TOKEN".into(), None));
        let info = AttachInfo {
            program: "claude".into(),
            args: vec!["--resume".into(), "s-1".into()],
            cwd: Some(path("work")),
            env,
        };
        let c = twr_hub::remote::ssh_exec_command("me@gpu", BUILD, "claude-code", &info).unwrap();
        let remote = c.get_args().last().unwrap().to_string_lossy().into_owned();
        let mut sh = std::process::Command::new("sh");
        sh.args(["-c", &remote])
            .env("XDG_DATA_HOME", path("data"))
            .env("HOME", path("box-home"))
            .env("SHELL", path("shell/bash"))
            .env("TWR_RUNNER_TOKEN", "secret");
        for (_, _, var) in twr_adapters::resolve::BUILTIN_AGENTS {
            sh.env_remove(var);
        }
        let out = sh.output().unwrap();
        assert!(
            out.status.success(),
            "{case}: {remote}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let attach = ran(&attached);

        assert_eq!(attach, spawn, "{case}");
        let expected_path = match own_path {
            Some(p) => p.to_string(),
            None => lbox
                .login_path()
                .await
                .unwrap()
                .merged(std::env::var_os("PATH").as_deref())
                .to_string_lossy()
                .into_owned(),
        };
        let thread_home = path("thread-home").display().to_string();
        assert_eq!(
            attach,
            [
                path("bin/claude").display().to_string(),
                expected_path,
                thread_home,
                "/bin/false".to_string(),
                "end".to_string(),
            ],
            "{case}"
        );
    }
}

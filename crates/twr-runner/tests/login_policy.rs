//! A box resolves an agent the same way wherever it starts it: a spawn by its daemon (an
//! in-process `LocalBox` here), `twr runner --exec` (a remote attach,
//! [`twr_runner::agents::exec_command`]) and `twr runner --login`
//! ([`twr_runner::agents::login_command`]) all take the box's baseline login `PATH` from the
//! same [`BoxEnv::user`] decision ([`BoxEnv::baseline_login_path`]): the login shell's `PATH`
//! before the inherited one, or, with `TWR_BOX_PROBE=off`, no login shell asked and the
//! inherited `PATH` alone, for all three.
//!
//! The only test of this binary: it sets this process's `PATH` (the inherited one a spawn
//! looks the agent up on), which no other test here may read meanwhile.
#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use twr_core::adapter::Adapter;
use twr_core::runner::{Op, WireSpawnConfig};
use twr_runner::{BoxEnv, LocalBox, LocalBoxOptions, BOX_PROBE_ENV};

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

/// The program the fake `claude` that wrote `out` ran as.
fn ran(out: &Path) -> String {
    let start = Instant::now();
    loop {
        if let Ok(s) = std::fs::read_to_string(out) {
            if let Some(p) = s.strip_suffix('\n') {
                return p.to_string();
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
async fn spawn_exec_and_login_take_the_same_login_path_decision() {
    let dir = tempfile::tempdir().unwrap();
    let path = |p: &str| dir.path().join(p);
    for d in ["inherited", "login", "shell", "work", "data"] {
        std::fs::create_dir_all(path(d)).unwrap();
    }
    // A `claude` on each PATH, writing where it ran from to `$OUT`.
    for d in ["inherited", "login"] {
        install(
            &path(&format!("{d}/claude")),
            "#!/bin/sh\ncase \"$1\" in --help) exit 0;; --version) echo '2.1.302 (Claude Code)'; \
             exit 0;; esac\nprintf '%s\\n' \"$0\" > \"$OUT\"\n",
        );
    }
    // A login shell (asked as bash is) whose profile puts `login` first.
    install(
        &path("shell/bash"),
        &format!(
            "#!/bin/sh\nPATH='{}':\"$PATH\"; export PATH\nshift; script=$1; shift; \
             exec /bin/sh -c \"$script\" \"$@\"\n",
            path("login").display()
        ),
    );
    // The inherited PATH: the runner's own, which a runtime keeps without a login PATH.
    std::env::set_var(
        "PATH",
        format!("{}:/usr/bin:/bin", path("inherited").display()),
    );
    let no_bins = |_: &str| -> Option<OsString> { None };

    for (case, probe, from) in [("on", None, "login"), ("off", Some("off"), "inherited")] {
        let shell: OsString = path("shell/bash").into();
        let env = BoxEnv::user_with(|k| match k {
            "SHELL" => Some(shell.clone()),
            BOX_PROBE_ENV => probe.map(OsString::from),
            _ => None,
        });
        assert_eq!(env.login_shell.is_some(), probe.is_none(), "{case}");
        let want = path(&format!("{from}/claude")).display().to_string();

        // The daemon's spawn, with that environment.
        let mut o = LocalBoxOptions::new(&path("data"), &format!("gpu-{case}"));
        o.env = BoxEnv {
            probe_agents: false,
            bin_vars: Some(Vec::new()),
            ..env.clone()
        };
        o.adapters = Arc::new(|n: &str| {
            (n == twr_adapters::claude_code::NAME).then(|| {
                Arc::new(twr_adapters::claude_code::ClaudeCodeAdapter::with_bin(
                    "claude",
                )) as Arc<dyn Adapter>
            })
        });
        let lbox = LocalBox::open(o).unwrap();
        let out = path(&format!("spawn-{case}"));
        lbox.handle_cmd(
            &format!("s-{case}"),
            Op::Spawn {
                thread: format!("t-{case}"),
                runtime: twr_adapters::claude_code::NAME.into(),
                cfg: WireSpawnConfig {
                    cwd: path("work"),
                    env: vec![("OUT".into(), out.display().to_string())],
                    ..Default::default()
                },
                prompt: None,
                run: None,
            },
        )
        .await
        .into_outcome()
        .unwrap();
        assert_eq!(ran(&out), want, "{case}: spawn");

        // `twr runner --exec` and `--login`, with the same environment.
        let mut failed = Vec::new();
        let exec = twr_runner::agents::exec_command(
            &env,
            twr_adapters::claude_code::NAME,
            &[],
            None,
            &[],
            no_bins,
            |why| failed.push(why.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(exec.get_program().to_string_lossy(), want, "{case}: exec");
        let login = twr_runner::agents::login_command(
            &env,
            twr_adapters::claude_code::NAME,
            no_bins,
            |why| failed.push(why.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(login.get_program().to_string_lossy(), want, "{case}: login");
        assert!(failed.is_empty(), "{case}: {failed:?}");
    }
}

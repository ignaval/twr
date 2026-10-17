//! Opt-in resource limits for the workers on the hub's machine: `[threads] cpu_weight` and
//! `memory_high`, both off by default (some want their agents to have the whole machine).
//!
//! With either set, the hub checks once, when it opens, that it can start a transient systemd
//! user scope (`systemd-run --user --scope`), and sets the CPU weight of
//! [`WORKERS_SLICE`] (`systemctl --user set-property --runtime`, until the next boot). Every
//! local worker's runtime then starts under `systemd-run --user --scope --quiet --collect
//! --slice=twr.slice [-p MemoryHigh=<size>] --`, which registers the scope and then runs the
//! runtime in its own place: the same process (its id, process group and parent, the hub), so
//! stopping it, the agent guard and the reaping see it as before. Everything the runtime starts
//! stays in its scope.
//!
//! Without systemd (macOS, a container) the weight becomes a niceness (`nice -n`), and there is
//! no memory limit; the hub says so in its log. The orchestrator, `twr attach` and a runner
//! box's workers always run unlimited.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use twr_core::config::{ThreadsConfig, WORKERS_SLICE};

/// The variables `systemd-run` finds the user manager by: a worker's own values (`[threads]
/// env`) are its runtime's, never `systemd-run`'s ([`Limits::wrap_for`]).
const MANAGER_ENV: [&str; 2] = ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"];

/// How long the hub waits for `systemd-run` or `systemctl` when it opens.
const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// How the workers' runtimes are limited: the command line each starts under
/// ([`twr_core::adapter::SpawnConfig::wrap`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Limits {
    /// The command line, empty for none.
    wrap: Vec<String>,
    /// By what: `systemd` or `nice` (`hub.info`'s `worker_limits`).
    by: Option<&'static str>,
    /// With systemd: `env`, and the hub's own [`MANAGER_ENV`] (`None`: unset), which
    /// `systemd-run` runs with whatever a worker's environment says.
    manager: Option<Manager>,
}

/// [`Limits`]'s `manager`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Manager {
    /// `env`'s full path.
    env: String,
    /// Each of [`MANAGER_ENV`] the hub has (`None`: unset).
    vars: Vec<(String, Option<String>)>,
}

impl Limits {
    /// Works out the limits `threads` asks for, with `systemd_run` and `systemctl` (blocking:
    /// it runs them, each for at most [`TOOL_TIMEOUT`]). Nothing runs when it asks for none.
    pub(super) fn prepare(threads: &ThreadsConfig, systemd_run: &Path, systemctl: &Path) -> Limits {
        if threads.cpu_weight.is_none() && threads.memory_high.is_none() {
            return Limits::default();
        }
        let slice = format!("--slice={WORKERS_SLICE}");
        // Its full path, found on the hub's `PATH`: a worker's `[threads] env` may set
        // another.
        if let Some(run) = resolve(systemd_run) {
            let mut base: Vec<String> = vec![
                run.to_string_lossy().into_owned(),
                "--user".into(),
                "--scope".into(),
                "--quiet".into(),
                "--collect".into(),
                slice.clone(),
                "--description=twr worker".into(),
            ];
            if let Some(m) = &threads.memory_high {
                base.extend(["-p".to_string(), format!("MemoryHigh={m}")]);
            }
            // A systemd that expands `$VAR` in a scope's command line (258 does) must not
            // touch the runtime's arguments; one before 254 has no such option, and expands
            // nothing. Each tried as the workers will run, with `true` for the runtime.
            let mut quoted = base.clone();
            quoted.insert(5, "--expand-environment=no".into());
            let wrap = [quoted, base].into_iter().find_map(|mut w| {
                w.push("--".into());
                let mut probe = w.clone();
                probe.push("true".into());
                run_quiet(&probe).then_some(w)
            });
            if let Some(mut wrap) = wrap {
                // The slice's weight against the user manager's other units; 100 (systemd's
                // default) when only the memory is limited, undoing an earlier weight.
                let weight = format!("CPUWeight={}", threads.cpu_weight.unwrap_or(100));
                let set = resolve(systemctl).map(|ctl| {
                    vec![
                        ctl.to_string_lossy().into_owned(),
                        "--user".into(),
                        "set-property".into(),
                        "--runtime".into(),
                        WORKERS_SLICE.into(),
                        weight,
                    ]
                });
                let mut by = "systemd";
                if !set.is_some_and(|set| run_quiet(&set)) {
                    tracing::warn!(
                        slice = WORKERS_SLICE,
                        "cannot set the workers' CPU weight (systemctl --user set-property)"
                    );
                    // The weight asked for, as a niceness instead: alone, without a scope;
                    // under each memory-limited scope, in it.
                    if let Some(w) = threads.cpu_weight {
                        if threads.memory_high.is_none() {
                            return Limits::niced(w);
                        }
                        let niced = Limits::niced(w);
                        if !niced.wrap.is_empty() {
                            wrap.extend(niced.wrap);
                            by = "systemd and nice";
                        }
                    }
                }
                tracing::info!(
                    cpu_weight = threads.cpu_weight,
                    memory_high = threads.memory_high.as_deref(),
                    "local workers run in systemd scopes in {WORKERS_SLICE}"
                );
                // The hub's own values; one that is not text is left as the worker has it.
                let manager = resolve(Path::new("env")).map(|env| {
                    let vars = MANAGER_ENV
                        .iter()
                        .filter_map(|k| match std::env::var_os(k) {
                            None => Some((k.to_string(), None)),
                            Some(v) => v.into_string().ok().map(|v| (k.to_string(), Some(v))),
                        })
                        .collect();
                    Manager {
                        env: env.to_string_lossy().into_owned(),
                        vars,
                    }
                });
                return Limits {
                    wrap,
                    by: Some(by),
                    manager,
                };
            }
        }
        if threads.memory_high.is_some() {
            tracing::warn!(
                "[threads] memory_high needs systemd (systemd-run --user --scope did not work \
                 here): workers run without a memory limit"
            );
        }
        threads.cpu_weight.map(Limits::niced).unwrap_or_default()
    }

    /// CPU weight `w` as a niceness (`nice -n`, [`niceness`]); none when it is 0 or there is
    /// no `nice`.
    fn niced(w: u64) -> Limits {
        let n = niceness(w);
        match resolve(Path::new("nice")) {
            Some(nice) if n > 0 => {
                tracing::info!(niceness = n, "local workers run under nice");
                Limits {
                    wrap: vec![
                        nice.to_string_lossy().into_owned(),
                        "-n".into(),
                        n.to_string(),
                    ],
                    by: Some("nice"),
                    manager: None,
                }
            }
            _ => Limits::default(),
        }
    }

    /// The command line a local worker's runtime (or job) whose environment adds `env`
    /// starts under (empty: none). When `env` sets one of [`MANAGER_ENV`], `systemd-run` runs
    /// with the hub's value (`env K=V systemd-run ...`) and the runtime with the worker's
    /// (`... -- env K=V <runtime>`): a private `XDG_RUNTIME_DIR` for the agent must not hide
    /// the user manager from `systemd-run`.
    pub(super) fn wrap_for(&self, env: &[(String, String)]) -> Vec<String> {
        let mut overridden: Vec<(&str, &str)> = Vec::new();
        for (k, v) in env {
            if MANAGER_ENV.contains(&k.as_str()) {
                // The last one, as the command's environment takes it.
                overridden.retain(|(o, _)| o != k);
                overridden.push((k, v));
            }
        }
        let sep = self.wrap.iter().position(|a| a == "--");
        let (
            Some(Manager {
                env: env_bin,
                vars: hub,
            }),
            Some(sep),
            false,
        ) = (&self.manager, sep, overridden.is_empty())
        else {
            return self.wrap.clone();
        };
        let mut out = vec![env_bin.clone()];
        for (k, v) in hub {
            if v.is_none() {
                out.extend(["-u".to_string(), k.clone()]);
            }
        }
        for (k, v) in hub {
            if let Some(v) = v {
                out.push(format!("{k}={v}"));
            }
        }
        out.extend(self.wrap[..=sep].iter().cloned());
        out.push(env_bin.clone());
        out.extend(overridden.iter().map(|(k, v)| format!("{k}={v}")));
        out.extend(self.wrap[sep + 1..].iter().cloned());
        out
    }

    /// `systemd`, `nice`, `systemd and nice` (memory in a scope, the weight as a niceness) or
    /// none.
    pub(super) fn by(&self) -> Option<&'static str> {
        self.by
    }
}

/// The niceness that weighs about as much as CPU weight `w` against 100 (the kernel's weights
/// go down by about 1.25 a step): 0 for 100 and over, 19 at most.
pub(super) fn niceness(w: u64) -> u32 {
    if w >= 100 || w == 0 {
        return 0;
    }
    let steps = (100.0 / w as f64).ln() / 1.25f64.ln();
    (steps.round() as u32).min(19)
}

/// `program` with its full path: as it is when it has a `/`, else found on the hub's `PATH`
/// ([`resolve_in`] the hub's working directory).
fn resolve(program: &Path) -> Option<PathBuf> {
    let path = std::env::var_os("PATH");
    let cwd = std::env::current_dir().ok();
    resolve_in(program, path.as_deref(), cwd.as_deref())
}

/// `program` with its full path, found as the hub, working in `cwd`, finds it: as it is when
/// it has a `/`, else on `path` (a `PATH` value). A relative `program` or `PATH` entry is
/// taken from `cwd` and made absolute: a worker's wrapper runs in its worktree, where the same
/// relative path names another file (one the repository may hold). Without `cwd` (the hub's
/// working directory is gone), only absolute ones count.
fn resolve_in(
    program: &Path,
    path: Option<&std::ffi::OsStr>,
    cwd: Option<&Path>,
) -> Option<PathBuf> {
    let absolute = |p: &Path| match cwd {
        _ if p.is_absolute() => Some(p.to_path_buf()),
        Some(cwd) => Some(cwd.join(p)),
        None => None,
    };
    if program.components().count() > 1 || program.is_absolute() {
        let p = absolute(program)?;
        return twr_adapters::process::find_program(&p, None, Path::new("/"));
    }
    // Every entry made absolute; an empty one (the current directory, to some shells) is
    // skipped, as `find_program` does.
    let dirs: Vec<PathBuf> = std::env::split_paths(path?)
        .filter(|d| !d.as_os_str().is_empty())
        .filter_map(|d| absolute(&d))
        .collect();
    let path = std::env::join_paths(dirs).ok()?;
    twr_adapters::process::find_program(program, Some(&path), Path::new("/"))
}

/// Runs `argv` with no input or output; whether it exited 0 within [`TOOL_TIMEOUT`] (killed
/// when not).
fn run_quiet(argv: &[String]) -> bool {
    let Some((program, args)) = argv.split_first() else {
        return false;
    };
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // No terminal (the hub's, when it runs in the foreground of one): a `systemd-run` or
    // `systemctl` found on `PATH` may be a wrapper that touches it.
    twr_adapters::process::new_session_std(&mut cmd);
    let child = cmd.spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let deadline = Instant::now() + TOOL_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// The default `systemd-run` and `systemctl`: found on `PATH`.
pub(super) fn default_tools() -> (PathBuf, PathBuf) {
    (PathBuf::from("systemd-run"), PathBuf::from("systemctl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::install;

    /// A weight systemctl cannot set becomes a niceness: alone without a memory limit, inside
    /// the scope with one.
    #[test]
    fn a_weight_systemctl_cannot_set_is_a_niceness() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("systemd-run");
        let ctl = dir.path().join("systemctl");
        for (p, code) in [(&run, 0), (&ctl, 1)] {
            install(p, format!("#!/bin/sh\nexit {code}\n"));
        }
        let cpu = ThreadsConfig {
            cpu_weight: Some(20),
            ..Default::default()
        };
        let l = Limits::prepare(&cpu, &run, &ctl);
        assert_eq!(l.by(), Some("nice"), "no nice found: {l:?}");
        assert!(
            l.wrap_for(&[])[0].ends_with("/nice"),
            "{:?}",
            l.wrap_for(&[])
        );
        let both = ThreadsConfig {
            cpu_weight: Some(20),
            memory_high: Some("2G".into()),
            ..Default::default()
        };
        let l = Limits::prepare(&both, &run, &ctl);
        assert_eq!(
            l.by(),
            Some("systemd and nice"),
            "the probe of the fake systemd-run failed, or no nice: {l:?}"
        );
        let wrap = l.wrap_for(&[]);
        assert_eq!(wrap[0], run.to_string_lossy());
        assert_eq!(
            wrap[wrap.len() - 6..wrap.len() - 3],
            ["-p", "MemoryHigh=2G", "--"]
        );
        assert!(wrap[wrap.len() - 3].ends_with("/nice"), "{wrap:?}");
        assert_eq!(wrap[wrap.len() - 2..], ["-n", "7"]);
    }

    /// A worker's own `XDG_RUNTIME_DIR` goes to its runtime, past `systemd-run`, which keeps
    /// the hub's.
    #[test]
    fn a_workers_manager_variables_skip_systemd_run() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("systemd-run");
        install(&run, "#!/bin/sh\nexit 0\n");
        let threads = ThreadsConfig {
            memory_high: Some("2G".into()),
            ..Default::default()
        };
        let l = Limits::prepare(&threads, &run, Path::new("/nonexistent/systemctl"));
        assert_eq!(
            l.by(),
            Some("systemd"),
            "the probe of the fake systemd-run failed: {l:?}"
        );
        let plain = l.wrap_for(&[("CARGO_INCREMENTAL".into(), "0".into())]);
        assert_eq!(plain[0], run.to_string_lossy());
        let env = [
            ("XDG_RUNTIME_DIR".to_string(), "/tmp/a".to_string()),
            ("XDG_RUNTIME_DIR".to_string(), "/tmp/private".to_string()),
        ];
        let w = l.wrap_for(&env);
        assert!(w[0].ends_with("/env"), "{w:?}");
        let hub = match std::env::var("XDG_RUNTIME_DIR") {
            Ok(v) => format!("XDG_RUNTIME_DIR={v}"),
            Err(_) => "XDG_RUNTIME_DIR".into(),
        };
        let at = w.iter().position(|a| *a == run.to_string_lossy()).unwrap();
        assert!(w[..at].contains(&hub), "{w:?}");
        let sep = w.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            w[sep + 1..],
            [w[0].clone(), "XDG_RUNTIME_DIR=/tmp/private".into()]
        );
    }

    /// A systemd without `--expand-environment` (before 254) runs the workers without it.
    #[test]
    fn an_older_systemd_runs_without_expand_environment() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("systemd-run");
        install(
            &run,
            "#!/bin/sh\ncase \"$*\" in *--expand-environment*) exit 1;; esac\nexit 0\n",
        );
        let threads = ThreadsConfig {
            memory_high: Some("2G".into()),
            ..Default::default()
        };
        let l = Limits::prepare(&threads, &run, Path::new("/nonexistent/systemctl"));
        assert_eq!(
            l.by(),
            Some("systemd"),
            "the probe of the fake systemd-run failed: {l:?}"
        );
        let wrap = l.wrap_for(&[]);
        assert!(!wrap.iter().any(|a| a.contains("expand")), "{wrap:?}");
        assert_eq!(wrap[wrap.len() - 3..], ["-p", "MemoryHigh=2G", "--"]);
    }

    /// A tool found through a relative `PATH` entry (or named by a relative path) is kept with
    /// its full path from the hub's working directory: a worker runs its wrapper in its own
    /// worktree, where the relative one would name the repository's file.
    #[test]
    fn tools_on_a_relative_path_are_kept_absolute() {
        use std::os::unix::fs::PermissionsExt;
        let hub = tempfile::tempdir().unwrap();
        let bin = hub.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let nice = bin.join("nice");
        std::fs::write(&nice, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&nice, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::ffi::OsString::from("bin:/nonexistent");
        let found = resolve_in(Path::new("nice"), Some(&path), Some(hub.path())).unwrap();
        assert_eq!(found, nice);
        let named = resolve_in(Path::new("bin/nice"), None, Some(hub.path())).unwrap();
        assert_eq!(named, nice);
        // From another directory (a worktree with a `bin/nice` of its own), the same entry
        // names that one: found from the hub's directory only.
        let worktree = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_in(Path::new("nice"), Some(&path), Some(worktree.path())),
            None
        );
        // Without a working directory, relative ones count for nothing.
        assert_eq!(resolve_in(Path::new("nice"), Some(&path), None), None);
        assert_eq!(resolve_in(Path::new("bin/nice"), None, None), None);
        let absolute = std::ffi::OsString::from(bin.as_os_str());
        assert_eq!(
            resolve_in(Path::new("nice"), Some(&absolute), None),
            Some(nice)
        );
    }

    #[test]
    fn a_weight_becomes_a_niceness() {
        assert_eq!(niceness(100), 0);
        assert_eq!(niceness(500), 0);
        assert_eq!(niceness(50), 3);
        assert_eq!(niceness(20), 7);
        assert_eq!(niceness(10), 10);
        assert_eq!(niceness(1), 19);
    }

    #[test]
    fn no_limits_run_nothing() {
        let missing = Path::new("/nonexistent/systemd-run");
        let l = Limits::prepare(&ThreadsConfig::default(), missing, missing);
        assert_eq!(l, Limits::default());
    }

    #[test]
    fn without_systemd_the_weight_is_a_niceness() {
        let missing = Path::new("/nonexistent/systemd-run");
        let threads = ThreadsConfig {
            cpu_weight: Some(20),
            memory_high: Some("1G".into()),
            ..Default::default()
        };
        let l = Limits::prepare(&threads, missing, missing);
        let wrap = l.wrap_for(&[]);
        assert!(
            Path::new(&wrap[0]).is_absolute() && wrap[0].ends_with("/nice"),
            "{wrap:?}"
        );
        assert_eq!(wrap[1..], ["-n", "7"]);
        assert_eq!(l.by(), Some("nice"));
        let threads = ThreadsConfig {
            memory_high: Some("1G".into()),
            ..Default::default()
        };
        assert_eq!(
            Limits::prepare(&threads, missing, missing),
            Limits::default()
        );
    }
}

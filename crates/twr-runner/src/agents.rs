//! What a box learns of its environment for its agents ([`BoxEnv`]): the user's login shell's
//! `PATH`, asked once ([`twr_adapters::resolve`]), which every runtime the box starts gets
//! before the runner's own unless its thread configures a `PATH`; and its agent CLIs
//! ([`twr_adapters::agents`]), probed at start, every [`AGENTS_EVERY`] and when the hub asks
//! for a fresh look (`Op::Agents { refresh: true }`), kept for the inventory and the op.
//!
//! Both start in the background when the runner starts ([`crate::run_with`]): neither delays
//! the hub's connection. A spawn or resume waits for the login `PATH` (at most
//! [`twr_adapters::resolve::LOGIN_PATH_TIMEOUT`], once), so a runtime started right after the
//! runner gets it too; an inventory sent before the first probe ended carries what is known
//! (nothing). The login `PATH` is asked once per runner: a change to the rc files takes a
//! runner restart.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use twr_adapters::resolve::{LoginPath, LoginShell, LOGIN_PATH_TIMEOUT};
use twr_core::runner::AgentInfo;

/// `TWR_BOX_PROBE`: `off` keeps a runner (`twr runner`, a box daemon) from asking the login
/// shell for its `PATH` and from probing its agent CLIs ([`BoxEnv::user`] is then
/// [`BoxEnv::none`]): for tests and troubleshooting.
pub const BOX_PROBE_ENV: &str = "TWR_BOX_PROBE";

/// How often a runner probes its agent CLIs again.
pub const AGENTS_EVERY: Duration = Duration::from_secs(10 * 60);

/// What a box asks of its environment ([`crate::LocalBoxOptions::env`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BoxEnv {
    /// The login shell asked for its `PATH` (once); `None`: none is asked, and the runtimes
    /// keep the runner's `PATH`.
    pub login_shell: Option<LoginShell>,
    /// Whether the box probes its agent CLIs; without, it reports none.
    pub probe_agents: bool,
    /// The `TWR_*_BIN` values the agent probe reads: `None`, this process's environment (a
    /// runner's, as its adapters read it); `Some`, these only (a test, so nothing it inherits
    /// changes which program it probes).
    pub bin_vars: Option<Vec<(String, std::ffi::OsString)>>,
}

impl BoxEnv {
    /// A runner's (`twr runner`, a box daemon): the user's login shell (`$SHELL`), and its
    /// agents probed; none of it with [`BOX_PROBE_ENV`] `off`.
    pub fn user() -> BoxEnv {
        BoxEnv::user_with(|k| std::env::var_os(k))
    }

    /// [`BoxEnv::user`] with the environment `var` reads.
    pub fn user_with(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> BoxEnv {
        if var(BOX_PROBE_ENV).is_some_and(|v| v == "off") {
            return BoxEnv::none();
        }
        BoxEnv {
            login_shell: Some(LoginShell::of(var("SHELL").as_deref())),
            probe_agents: true,
            bin_vars: None,
        }
    }

    /// None of it (the default): the runtimes keep the runner's `PATH`, and no agent is
    /// probed or reported. Tests use it, so no real shell or agent CLI runs under them.
    pub fn none() -> BoxEnv {
        BoxEnv::default()
    }

    /// The box's baseline login `PATH`, the one put before the runner's own for an agent of
    /// the box when its thread configures no `PATH`: its login shell's
    /// ([`twr_adapters::resolve::ask_login_path`], within [`LOGIN_PATH_TIMEOUT`]). `None`:
    /// no shell is asked ([`BoxEnv::login_shell`] `None`: [`BOX_PROBE_ENV`] `off`, or a test),
    /// and the agents are looked up on the runner's `PATH` alone; `Some(Err(why))`: the shell
    /// gave none (the same then). The one choice the box's daemon (once,
    /// [`BoxAgents::login_path`]), `twr runner --exec` ([`exec_command`]), `--login`
    /// ([`login_command`]) and `--agents` make, each with [`BoxEnv::user`].
    pub async fn baseline_login_path(&self) -> Option<Result<LoginPath, String>> {
        let shell = self.login_shell.as_ref()?;
        Some(twr_adapters::resolve::ask_login_path(shell, LOGIN_PATH_TIMEOUT).await)
    }

    /// [`BoxEnv::baseline_login_path`], `why` the shell gave none passed to `failed`.
    async fn login_path_or(&self, failed: impl FnOnce(&str)) -> Option<LoginPath> {
        match self.baseline_login_path().await? {
            Ok(p) => Some(p),
            Err(why) => {
                failed(&why);
                None
            }
        }
    }
}

/// What `twr runner --login <runtime>` runs: the runtime's login command
/// ([`twr_adapters::agents::login_command`]) resolved on `env`'s baseline login `PATH`
/// ([`BoxEnv::baseline_login_path`]), as the box's daemon resolves a spawn's program with the
/// same `env` (`TWR_*_BIN` as `var` reads them). `failed` gets why the login shell gave no
/// `PATH`, when it gave none.
pub async fn login_command(
    env: &BoxEnv,
    runtime: &str,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    failed: impl FnOnce(&str),
) -> anyhow::Result<std::process::Command> {
    let login = env.login_path_or(failed).await;
    twr_adapters::agents::login_command(runtime, login.as_ref(), var)
}

/// What `twr runner --agents` prints ([`twr_adapters::agents::report`]): the agents probed
/// with `env`'s baseline login `PATH` ([`BoxEnv::baseline_login_path`]), and how asking for it
/// went: with [`BOX_PROBE_ENV`] `off` no shell is asked, which the report says, `$SHELL`
/// named as the shell not asked.
pub async fn agents_report(env: &BoxEnv) -> twr_core::runner::AgentsReport {
    let (shell, login) = reported_login_path(env).await;
    twr_adapters::agents::report(shell.as_deref(), login).await
}

/// The shell [`agents_report`] names and how asking it went.
async fn reported_login_path(
    env: &BoxEnv,
) -> (Option<std::path::PathBuf>, Result<LoginPath, String>) {
    let shell = match &env.login_shell {
        Some(s) => s.clone(),
        None => LoginShell::from_env(),
    };
    let login = env.baseline_login_path().await.unwrap_or_else(|| {
        Err(format!(
            "{BOX_PROBE_ENV} is off: the login shell is not asked"
        ))
    });
    (shell.path().map(std::path::Path::to_path_buf), login)
}

/// What `twr runner --exec <runtime>` runs (a remote attach):
/// [`twr_adapters::agents::exec_command`] with `env`'s baseline login `PATH`
/// ([`BoxEnv::baseline_login_path`]), asked only when the thread configures no `PATH`
/// (`thread_path`), as the box's daemon resolves a spawn of the thread with the same `env`.
/// `failed` gets why the login shell gave no `PATH`, when it was asked and gave none.
pub async fn exec_command(
    env: &BoxEnv,
    runtime: &str,
    args: &[String],
    thread_path: Option<&str>,
    thread_env: &[(String, Option<String>)],
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    failed: impl FnOnce(&str),
) -> anyhow::Result<std::process::Command> {
    let login = match thread_path {
        Some(_) => None,
        None => env.login_path_or(failed).await,
    };
    twr_adapters::agents::exec_command(runtime, args, thread_path, thread_env, login.as_ref(), var)
}

/// The box's login `PATH` and agents ([`crate::LocalBox`] holds one).
#[derive(Debug)]
pub(crate) struct BoxAgents {
    env: BoxEnv,
    /// The login shell's `PATH`, once asked (`None` inside: it gave none).
    login: tokio::sync::OnceCell<Option<Arc<LoginPath>>>,
    /// The last probe's result, with the number of that probe.
    known: Mutex<Option<(u64, Vec<AgentInfo>)>>,
    /// Held by the probe running (one at a time).
    probing: tokio::sync::Mutex<()>,
    /// The number of the last probe started (under `probing`).
    started: AtomicU64,
}

impl BoxAgents {
    pub(crate) fn new(env: BoxEnv) -> BoxAgents {
        BoxAgents {
            env,
            login: tokio::sync::OnceCell::new(),
            known: Mutex::new(None),
            probing: tokio::sync::Mutex::new(()),
            started: AtomicU64::new(0),
        }
    }

    /// The login shell's `PATH`: asked the first time (at most [`LOGIN_PATH_TIMEOUT`]; callers
    /// meanwhile wait for that answer), then the same answer. Logged once.
    pub(crate) async fn login_path(&self) -> Option<Arc<LoginPath>> {
        self.login
            .get_or_init(|| async {
                let shown = self
                    .env
                    .login_shell
                    .as_ref()
                    .and_then(LoginShell::path)
                    .map(|p| p.display().to_string());
                match self.env.baseline_login_path().await? {
                    Ok(p) => {
                        tracing::info!(
                            shell = ?shown,
                            entries = p.entries().len(),
                            "the login shell's PATH goes before the runner's for the runtimes"
                        );
                        Some(Arc::new(p))
                    }
                    Err(why) => {
                        tracing::warn!(
                            shell = ?shown,
                            %why,
                            "the login shell gave no PATH: the runtimes keep the runner's"
                        );
                        None
                    }
                }
            })
            .await
            .clone()
    }

    /// The agents as last probed (none before the first probe ends).
    pub(crate) fn known(&self) -> Vec<AgentInfo> {
        self.cache()
            .as_ref()
            .map(|(_, a)| a.clone())
            .unwrap_or_default()
    }

    /// Whether a probe has ended (or none is ever made).
    pub(crate) fn probed(&self) -> bool {
        !self.env.probe_agents || self.cache().is_some()
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Option<(u64, Vec<AgentInfo>)>> {
        self.known.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Probes the agents and keeps the result. A probe that started after this was asked and
    /// has ended (one runs at a time) is as fresh as one more: its result is returned
    /// instead. None without probing ([`BoxEnv::probe_agents`]).
    pub(crate) async fn probe(&self) -> Vec<AgentInfo> {
        if !self.env.probe_agents {
            return Vec::new();
        }
        let asked = self.started.load(Ordering::SeqCst);
        let _one = self.probing.lock().await;
        if let Some((n, agents)) = self.cache().as_ref().filter(|(n, _)| *n > asked) {
            tracing::debug!(probe = n, "an agent probe started since: using its result");
            return agents.clone();
        }
        let n = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let login = self.login_path().await;
        let mut env = Vec::new();
        twr_adapters::resolve::with_login_path(&mut env, login.as_deref());
        let var = |k: &str| match &self.env.bin_vars {
            None => std::env::var_os(k),
            Some(vars) => vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()),
        };
        let agents =
            twr_adapters::agents::probe_with(&env, var, twr_adapters::agents::COMMAND_TIMEOUT)
                .await;
        *self.cache() = Some((n, agents.clone()));
        agents
    }

    /// The background work of a running box: the login `PATH`, then a probe every
    /// [`AGENTS_EVERY`] (none without [`BoxEnv::probe_agents`]).
    pub(crate) async fn run(&self) {
        self.login_path().await;
        if !self.env.probe_agents {
            return;
        }
        loop {
            self.probe().await;
            tokio::time::sleep(AGENTS_EVERY).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `TWR_BOX_PROBE=off` turns the runner's login shell ask and agent probe off; anything
    /// else leaves them on.
    #[test]
    fn the_box_probe_can_be_turned_off() {
        let env = |probe: Option<&'static str>| {
            move |k: &str| match k {
                BOX_PROBE_ENV => probe.map(Into::into),
                "SHELL" => Some("/bin/bash".into()),
                _ => None,
            }
        };
        assert_eq!(BoxEnv::user_with(env(Some("off"))), BoxEnv::none());
        for on in [None, Some(""), Some("on"), Some("OFF ")] {
            let e = BoxEnv::user_with(env(on));
            assert!(e.probe_agents, "{on:?}");
            assert!(
                matches!(&e.login_shell, Some(LoginShell::Posix(p)) if p.ends_with("bash")),
                "{on:?}"
            );
        }
    }

    /// `twr runner --agents` says how the box's baseline login `PATH` was asked, as the daemon
    /// asks it: not at all with `TWR_BOX_PROBE=off`, which it names.
    #[tokio::test]
    async fn the_agents_report_takes_the_box_login_path_decision() {
        let off = BoxEnv::user_with(|k| (k == BOX_PROBE_ENV).then(|| "off".into()));
        let (_, login) = reported_login_path(&off).await;
        assert_eq!(
            login.unwrap_err(),
            "TWR_BOX_PROBE is off: the login shell is not asked"
        );
        let unset = BoxEnv::user_with(|_| None);
        let (shell, login) = reported_login_path(&unset).await;
        assert_eq!(shell, None);
        assert_eq!(login.unwrap_err(), "SHELL is not set");
        let csh = BoxEnv::user_with(|k| (k == "SHELL").then(|| "/bin/csh".into()));
        let (shell, login) = reported_login_path(&csh).await;
        assert_eq!(shell.as_deref(), Some(std::path::Path::new("/bin/csh")));
        assert!(login.unwrap_err().contains("is not asked"));
    }

    /// The probe reads `TWR_*_BIN` from [`BoxEnv::bin_vars`] when given, never from the
    /// environment then: what a test sets there is what it probes, whatever it inherits.
    #[tokio::test]
    async fn the_probe_reads_the_bin_vars_it_is_given() {
        let vars = ["TWR_CLAUDE_BIN", "TWR_CODEX_BIN", "TWR_GROK_BIN"]
            .iter()
            .map(|k| (k.to_string(), format!("/nonexistent/{k}").into()))
            .collect();
        let agents = BoxAgents::new(BoxEnv {
            login_shell: None,
            probe_agents: true,
            bin_vars: Some(vars),
        });
        let found = agents.probe().await;
        assert_eq!(found.len(), 3);
        for a in &found {
            assert!(!a.found, "{a:?}");
            assert!(
                a.note
                    .as_deref()
                    .is_some_and(|n| n.ends_with("names no executable file")),
                "{a:?}"
            );
        }
        assert_eq!(agents.known(), found);
    }
}

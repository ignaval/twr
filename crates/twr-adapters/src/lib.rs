//! Agent runtime adapters for twr.
//!
//! Each adapter lives in one file, implements [`twr_core::adapter::Adapter`] and is listed in
//! [`registry`]. Adding a runtime = one file + one line here.

use std::sync::Arc;

use twr_core::adapter::Adapter;

pub mod acp;
pub mod attn;
mod background;
pub mod claude_code;
pub mod codex;
pub mod fake;
pub mod grok_build;
pub mod process;
pub mod pty;
pub mod usage;

pub use acp::AcpAdapter;
pub use claude_code::ClaudeCodeAdapter;
pub use fake::FakeAdapter;
pub use grok_build::GrokBuildAdapter;
pub use pty::PtyAdapter;

/// Environment variable that adds the [`fake`] runtime to [`registry`] when set to `1`.
///
/// The fake runtime can write files and exists for tests and demos only, so production
/// builds do not offer it by default. Read at every [`registry`] / [`find`] call.
pub const ENABLE_FAKE_ENV: &str = "TWR_ENABLE_FAKE";

/// Whether [`ENABLE_FAKE_ENV`] is `1`.
pub fn fake_enabled() -> bool {
    std::env::var(ENABLE_FAKE_ENV).is_ok_and(|v| v.trim() == "1")
}

/// The [`SpawnConfig::env`](twr_core::adapter::SpawnConfig::env) keys an adapter's
/// `attach_command` reads: the runtime spec's attach template ([`acp::ATTACH_ENV`]). A spec's
/// agent command ([`acp::COMMAND_ENV`], [`pty::COMMAND_ENV`]) is not one: no adapter attaches
/// with it. A remote attach sends a box's runner only these keys, and the runner keeps only
/// these ([`retain_attach_env`]): nothing else of the hub's environment reaches it.
pub const ATTACH_SPEC_ENV: &[&str] = &[acp::ATTACH_ENV];

/// Whether `id` may be put on a command line as a session id (`claude --resume <id>`, `codex
/// resume <id>`, `grok --resume <id>`, an ACP attach template): 1 to 256 characters from
/// `[A-Za-z0-9._:-]`, not starting with `-` (it would read as an option:
/// `--dangerously-skip-permissions`). Every runtime's ids are UUIDs or the like; an id from
/// anywhere else (a runner's event, a corrupt log) is refused where it would reach a command
/// line.
pub use acp::translate::valid_session_id;

/// Keeps only the [`ATTACH_SPEC_ENV`] entries of `env`.
pub fn retain_attach_env(env: &mut Vec<(String, String)>) {
    env.retain(|(k, _)| ATTACH_SPEC_ENV.contains(&k.as_str()));
}

/// Whether runtime `runtime` (an adapter name) starts with permission mode `mode`: Ok, or why
/// not. Claude Code, Codex and Grok Build accept their own documented mode ids as written
/// (`claude_code::NATIVE_MODES`, `codex::NATIVE_MODES`, `grok_build::NATIVE_MODES`), other
/// spellings of those, and every mode twr knows ([`acp::translate::mode_level`], mapped by its
/// level, never wider); anything else is refused (their `spawn` fails the same way). A generic
/// ACP agent takes the agent's own vocabulary (checked against what the session reports), and
/// the pty and fake runtimes apply no mode: any mode is fine there.
pub fn check_mode(runtime: &str, mode: &str) -> Result<(), String> {
    let cfg = twr_core::adapter::SpawnConfig {
        mode: mode.to_string(),
        ..Default::default()
    };
    let res = match runtime {
        claude_code::NAME => claude_code::permission_mode(&cfg).map(drop),
        codex::NAME => codex::policy(&cfg.mode, None).map(drop),
        grok_build::NAME => grok_build::permission_mode(&cfg).map(drop),
        _ => Ok(()),
    };
    res.map_err(|e| e.to_string())
}

/// How much permission mode `mode` lets a thread of runtime `runtime` (an adapter name) run
/// without asking, as that runtime starts with it; None when twr cannot tell. Claude Code and
/// Grok Build classify the `--permission-mode` they pass (`""` is `default`, which asks), Codex
/// the policy it maps to ([`codex::mode_level`]; `""` is `manual`); a mode any of them refuses
/// is None. Anything else (a generic ACP agent, pty, fake) classifies the id itself
/// ([`acp::translate::mode_level`]): an ACP agent's own id in none of twr's tables, or no mode
/// (the agent's own default), is None, and pty and fake, which apply no mode, have the level
/// of the twr mode recorded for the thread.
pub fn mode_level_for(runtime: &str, mode: &str) -> Option<acp::translate::ModeLevel> {
    let cfg = twr_core::adapter::SpawnConfig {
        mode: mode.to_string(),
        ..Default::default()
    };
    match runtime {
        claude_code::NAME => claude_code::permission_mode(&cfg)
            .ok()
            .and_then(acp::translate::mode_level),
        grok_build::NAME => grok_build::permission_mode(&cfg)
            .ok()
            .and_then(acp::translate::mode_level),
        codex::NAME => codex::mode_level(mode),
        _ => acp::translate::mode_level(mode),
    }
}

/// Whether moving a thread from mode `from.1` of runtime `from.0` to mode `to.1` of runtime
/// `to.0` takes away anything its runtime may do without asking: what the runtime starts with,
/// not only the level ([`mode_level_for`]). Within Codex every capability counts on its own
/// ([`codex::mode_capabilities`]: approval policy, sandbox, network, extra roots), so
/// `on-request` -> `manual` and `auto` -> `never` narrow though their levels are equal.
/// Within Claude Code or Grok Build two modes the CLI is started with the same
/// `--permission-mode` for are the same (`manual` and `default`). Otherwise the levels
/// decide: a lower one narrows, and so does an equal one with another policy (another mode
/// id, another runtime), since twr cannot tell that nothing is lost. A mode twr cannot
/// classify (an ACP agent's own id, one a runtime refuses) narrows. The same runtime and mode
/// never does.
pub fn mode_narrows(from: (&str, &str), to: (&str, &str)) -> bool {
    if from == to {
        return false;
    }
    if from.0 == to.0 {
        let cfg = |mode: &str| twr_core::adapter::SpawnConfig {
            mode: mode.to_string(),
            ..Default::default()
        };
        match from.0 {
            codex::NAME => {
                return match (
                    codex::mode_capabilities(from.1),
                    codex::mode_capabilities(to.1),
                ) {
                    (Some(a), Some(b)) => a.iter().zip(b).any(|(a, b)| b < *a),
                    _ => true,
                }
            }
            claude_code::NAME => {
                let (a, b) = (
                    claude_code::permission_mode(&cfg(from.1)),
                    claude_code::permission_mode(&cfg(to.1)),
                );
                if matches!((&a, &b), (Ok(a), Ok(b)) if a == b) {
                    return false;
                }
            }
            grok_build::NAME => {
                let (a, b) = (
                    grok_build::permission_mode(&cfg(from.1)),
                    grok_build::permission_mode(&cfg(to.1)),
                );
                if matches!((&a, &b), (Ok(a), Ok(b)) if a == b) {
                    return false;
                }
            }
            _ => {}
        }
    }
    match (mode_level_for(from.0, from.1), mode_level_for(to.0, to.1)) {
        (Some(a), Some(b)) => b <= a,
        _ => true,
    }
}

/// The reasoning efforts `runtime` takes, in their canonical spelling: Claude Code's
/// (`claude_code::EFFORTS`), Codex's (`codex::EFFORTS`) and Grok Build's
/// (`grok_build::EFFORTS`); none for `pty` (`Some(&[])`: it runs a command line twr does not
/// configure). `None`: any value, as given (a generic ACP agent's own vocabulary, checked by
/// its session; the fake runtime).
pub fn efforts(runtime: &str) -> Option<&'static [&'static str]> {
    match runtime {
        claude_code::NAME => Some(claude_code::EFFORTS),
        codex::NAME => Some(codex::EFFORTS),
        grok_build::NAME => Some(grok_build::EFFORTS),
        pty::NAME => Some(&[]),
        _ => None,
    }
}

/// Which settings a runtime keeps from a session it resumes without them ([`resume_keeps`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumeKeeps {
    /// The session's model.
    pub model: bool,
    /// The session's reasoning effort.
    pub effort: bool,
}

/// Which settings `runtime` keeps from a session it resumes without them. For each one it
/// keeps, a thread with a session cannot go back to the runtime's default (twr resumes
/// without the setting, and the session runs on with its old value), and the hub refuses
/// that clear (`thread.set_model`). Checked 2026-10-03:
/// - Claude Code 2.1.289 keeps the model (a `haiku` session resumed without `--model` ran
///   `haiku`, though the settings named `opus`; `--model default` is the account's recommended
///   model, not the configured one, so twr cannot ask for the default either) but not the
///   effort (a session started with `--effort low` resumed without it ran `medium`, the
///   effort a new session gets);
/// - Codex 0.159.2 keeps both (a `gpt-6-luna` / `low` thread resumed with neither reported
///   `gpt-6-luna` / `low`, though `config.toml` says `gpt-6.1-sol` / `xhigh`);
/// - grok 1.0.44 keeps both (`session/resume` reported the `model` and `reasoning_effort`
///   set before; ACP config options carry no default to set back);
/// - a generic ACP agent: both, as far as twr can tell (it may restore them, and ACP says
///   nothing of a default).
///
/// Neither for anything else (`fake`; `pty` takes no settings).
pub fn resume_keeps(runtime: &str) -> ResumeKeeps {
    match runtime {
        claude_code::NAME => ResumeKeeps {
            model: true,
            effort: false,
        },
        codex::NAME | grok_build::NAME | acp::NAME => ResumeKeeps {
            model: true,
            effort: true,
        },
        _ => ResumeKeeps::default(),
    }
}

/// All built-in adapters (the fake one only with `TWR_ENABLE_FAKE=1`).
pub fn registry() -> Vec<Arc<dyn Adapter>> {
    registry_with(fake_enabled())
}

/// The built-in adapters, with or without the fake one.
pub fn registry_with(fake: bool) -> Vec<Arc<dyn Adapter>> {
    let mut v: Vec<Arc<dyn Adapter>> = Vec::new();
    if fake {
        v.push(Arc::new(FakeAdapter::new()));
    }
    v.push(Arc::new(ClaudeCodeAdapter::new()));
    v.push(Arc::new(codex::CodexAdapter::new()));
    v.push(Arc::new(GrokBuildAdapter::new()));
    v.push(Arc::new(AcpAdapter::new()));
    v.push(Arc::new(PtyAdapter::new()));
    v
}

/// Finds a built-in adapter by name (the fake one only with `TWR_ENABLE_FAKE=1`).
pub fn find(name: &str) -> Option<Arc<dyn Adapter>> {
    registry().into_iter().find(|a| a.info().name == name)
}

#[cfg(test)]
mod tests {
    /// A narrowing is any capability lost, not only a lower level: Codex's same-level modes
    /// with another approval policy, network or roots; another mode or runtime at one level;
    /// anything unknown. Equal effective policies and wider ones do not narrow.
    #[test]
    fn narrowing_compares_effective_policies() {
        use super::mode_narrows as n;
        let codex = |a: &'static str, b: &'static str| n(("codex", a), ("codex", b));
        assert!(codex("on-request", "manual"));
        assert!(codex("auto", "never"));
        assert!(codex("never", "auto"));
        assert!(codex("auto", "plan"));
        assert!(codex("auto", "dontAsk"));
        assert!(!codex("manual", "on-request"));
        assert!(!codex("manual", "acceptEdits"));
        assert!(!codex("acceptEdits", "manual"));
        assert!(!codex("manual", "default"));
        assert!(!codex("plan", "auto"));
        assert!(codex("auto", "nonsense"));
        // Full access has network and writes anywhere: a widening from auto, whatever it is
        // called; the other way round narrows.
        for full in ["bypassPermissions", "danger-full-access", "yolo"] {
            assert!(!codex("auto", full), "{full}");
            assert!(!codex("never", full), "{full}");
            assert!(codex(full, "auto"), "{full}");
        }
        let claude = |a: &'static str, b: &'static str| n(("claude-code", a), ("claude-code", b));
        assert!(claude("auto", "acceptEdits"));
        assert!(claude("plan", "dontAsk"));
        assert!(!claude("default", "acceptEdits"));
        assert!(!n(("grok-build", "manual"), ("grok-build", "default")));
        assert!(n(("grok-build", "auto"), ("grok-build", "default")));
        assert!(n(("codex", "auto"), ("claude-code", "auto")));
        assert!(!n(("codex", "manual"), ("claude-code", "acceptEdits")));
        assert!(n(("acp", "their-own"), ("acp", "another")));
        assert!(n(("acp", "default"), ("acp", "ask")));
        assert!(!n(("acp", "default"), ("acp", "default")));
    }

    use super::*;

    #[test]
    fn names_are_unique() {
        let mut names: Vec<_> = registry_with(true).iter().map(|a| a.info().name).collect();
        let n = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), n);
        assert!(find("nope").is_none());
    }

    #[test]
    fn fake_is_only_registered_on_request() {
        let names = |fake| -> Vec<&'static str> {
            registry_with(fake).iter().map(|a| a.info().name).collect()
        };
        assert!(!names(false).contains(&fake::NAME));
        assert!(names(true).contains(&fake::NAME));
        for real in [
            claude_code::NAME,
            codex::NAME,
            grok_build::NAME,
            acp::NAME,
            pty::NAME,
        ] {
            assert!(names(false).contains(&real), "{real}");
        }
    }

    /// The runtimes that can be the orchestrator: every headless one but the fake (a generic
    /// ACP agent included: its tool calls are permission prompts to the user).
    #[test]
    fn orchestrator_hosts() {
        let hosts: Vec<&str> = registry_with(true)
            .iter()
            .filter(|a| a.hosts_orchestrator())
            .map(|a| a.info().name)
            .collect();
        assert_eq!(
            hosts,
            [claude_code::NAME, codex::NAME, grok_build::NAME, acp::NAME]
        );
    }

    /// What each runtime keeps from a session it resumes without it (see [`resume_keeps`]).
    #[test]
    fn settings_runtimes_keep_on_resume() {
        let keeps = |model, effort| ResumeKeeps { model, effort };
        assert_eq!(resume_keeps(claude_code::NAME), keeps(true, false));
        for rt in [codex::NAME, grok_build::NAME, acp::NAME] {
            assert_eq!(resume_keeps(rt), keeps(true, true), "{rt}");
        }
        for rt in [fake::NAME, pty::NAME, "nope"] {
            assert_eq!(resume_keeps(rt), keeps(false, false), "{rt}");
        }
    }

    /// Which settings each runtime's attach command applies to the session it resumes
    /// (`Adapter::attach_applies`): Claude Code, Codex and Grok Build pass the thread's model
    /// and effort; a value left to the default is not passed, so it applies only where the
    /// runtime does not keep the session's own (Claude Code's effort). A generic ACP agent
    /// applies what its preset's attach template holds a placeholder for; `pty` has no attach.
    #[test]
    fn what_each_attach_applies() {
        use twr_core::adapter::{AttachApplies, SpawnConfig};
        let applies = |model, effort| AttachApplies { model, effort };
        let set = SpawnConfig {
            model: Some("m".into()),
            effort: Some("high".into()),
            ..Default::default()
        };
        let unset = SpawnConfig::default();
        for rt in [claude_code::NAME, codex::NAME, grok_build::NAME] {
            let a = registry_with(false)
                .into_iter()
                .find(|a| a.info().name == rt)
                .unwrap();
            assert_eq!(a.attach_applies(&set), applies(true, true), "{rt}");
            let keeps = resume_keeps(rt);
            assert_eq!(
                a.attach_applies(&unset),
                applies(!keeps.model, !keeps.effort),
                "{rt}"
            );
        }
        let acp = AcpAdapter::new();
        assert_eq!(acp.attach_applies(&set), applies(false, false));
        let mut templated = set.clone();
        templated.env.push((
            acp::ATTACH_ENV.into(),
            r#"["cli","{session}","--model={model}"]"#.into(),
        ));
        assert_eq!(acp.attach_applies(&templated), applies(true, false));
        assert_eq!(
            PtyAdapter::new().attach_applies(&set),
            applies(false, false)
        );
    }

    /// Each runtime accepts its documented ids, other
    /// spellings of them and every mode twr knows, and refuses what it does not know.
    #[test]
    fn modes_runtimes_start_with() {
        for rt in [claude_code::NAME, codex::NAME, grok_build::NAME] {
            for ok in [
                "",
                "manual",
                "plan",
                "read-only",
                "readOnly",
                "dont-ask",
                "auto",
            ] {
                assert!(check_mode(rt, ok).is_ok(), "{rt} {ok}");
            }
            let e = check_mode(rt, "made-up").unwrap_err();
            assert!(e.contains("unknown permission mode"), "{rt}: {e}");
        }
        assert!(check_mode(codex::NAME, "on-request").is_ok());
        assert!(check_mode(claude_code::NAME, "on-request").is_err());
        for rt in [acp::NAME, pty::NAME, fake::NAME] {
            assert!(check_mode(rt, "made-up").is_ok(), "{rt}");
        }
    }

    /// The level each runtime runs a mode at ([`mode_level_for`]): twr's modes and their
    /// spellings everywhere, each runtime's own ids (Codex's by their policy), no mode as the
    /// runtime's default, and None for what twr cannot classify.
    #[test]
    fn levels_runtimes_run_modes_at() {
        use acp::translate::ModeLevel::*;
        for rt in [
            claude_code::NAME,
            codex::NAME,
            grok_build::NAME,
            acp::NAME,
            pty::NAME,
            fake::NAME,
        ] {
            for (mode, level) in [
                ("plan", Restricted),
                ("dontAsk", Restricted),
                ("dont-ask", Restricted),
                ("read-only", Restricted),
                ("Read_Only", Restricted),
                ("manual", Ask),
                ("default", Ask),
                ("acceptEdits", AcceptEdits),
                ("auto", Auto),
                ("bypassPermissions", Bypass),
            ] {
                assert_eq!(mode_level_for(rt, mode), Some(level), "{rt} {mode}");
            }
            assert_eq!(mode_level_for(rt, "made-up"), None, "{rt}");
        }
        for (mode, level) in [
            ("untrusted", Ask),
            ("on-request", Ask),
            ("On_Request", Ask),
            ("never", Auto),
            ("danger-full-access", Bypass),
            ("yolo", Bypass),
            ("", Ask),
        ] {
            assert_eq!(mode_level_for(codex::NAME, mode), Some(level), "{mode}");
        }
        for mode in codex::NATIVE_MODES {
            assert!(codex::mode_level(mode).is_some(), "{mode}");
        }
        for rt in [claude_code::NAME, grok_build::NAME] {
            assert_eq!(mode_level_for(rt, ""), Some(Ask), "{rt}");
        }
        // An ACP agent's own default, or an id of its own: twr does not know what it allows.
        assert_eq!(mode_level_for(acp::NAME, ""), None);
        assert_eq!(mode_level_for(acp::NAME, "architect"), None);
        assert_eq!(mode_level_for(acp::NAME, "yolo"), Some(Bypass));
    }

    /// `find` / `registry` read `TWR_ENABLE_FAKE` on every call (the only test of this crate
    /// touching it, so no other test observes the change).
    #[test]
    fn fake_follows_the_env_at_lookup_time() {
        std::env::remove_var(ENABLE_FAKE_ENV);
        assert!(find(fake::NAME).is_none());
        std::env::set_var(ENABLE_FAKE_ENV, "0");
        assert!(find(fake::NAME).is_none());
        std::env::set_var(ENABLE_FAKE_ENV, "1");
        assert!(find(fake::NAME).is_some());
        assert!(registry().iter().any(|a| a.info().name == fake::NAME));
        std::env::remove_var(ENABLE_FAKE_ENV);
        assert!(find(fake::NAME).is_none());
    }
}

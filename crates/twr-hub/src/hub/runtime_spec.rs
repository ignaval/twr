//! Runtime specs (`thread.new` `runtime`, `orchestrator.use` `runtime`, `default_runtime`,
//! `[projects.P] runtime`, `[orchestrator] runtime`) and what they turn into.
//!
//! - `pty:<command line>`: the `pty` runtime; the command is split in-tree
//!   ([`twr_adapters::pty::split_command_line`], no expansion) and recorded as
//!   `thread.created` `ext.twr.pty_command`. A bare `pty` is refused.
//! - `acp:<command line>`: the generic `acp` runtime (any Agent Client Protocol agent), split
//!   the same way and recorded as `ext.twr.acp_command`. A bare `acp` is refused.
//! - `<name>` of a `[runtimes.<name>]` preset: the `acp` runtime with the preset's command
//!   (and `attach` template, `ext.twr.acp_attach`), resolved when the thread is created (so a
//!   later config change does not change an existing thread) and recorded with
//!   `ext.twr.acp_preset = <name>`.
//! - anything else: a runtime name, as is.
//!
//! Permission modes (`thread.new` `mode`, `threads.default_mode`) are canonicalized here
//! ([`canonical_mode`]): one of twr's own ids ([`SHARED_MODES`]) or the runtime's
//! (`<adapter>::NATIVE_MODES`), whole id, case / `-` / `_` / space insensitive, becomes its
//! spelling; a generic ACP agent's id is kept as given (its vocabulary), and anything else is
//! refused ([`twr_adapters::check_mode`] decides what a runtime starts with). The synonym
//! tables of [`acp::translate`] only recognize what an ACP agent reports.
//!
//! Reasoning efforts (`thread.new` / `thread.set_model` `effort`) are canonicalized here too
//! ([`canonical_effort`]): one of the runtime's own values ([`twr_adapters::efforts`]),
//! case-insensitively, or, for a generic ACP agent, its own value as given; blank or `default`
//! means the runtime's default ([`is_default`], the same for `thread.set_model` `model`).
//!
//! Each start (spawn or resume, local or on a runner box) passes an `acp` thread's argv as
//! `TWR_ACP_COMMAND` and its attach template as `TWR_ACP_ATTACH` ([`acp_env`]); `twr attach`
//! gets them too, for the preset's attach command (`{session}`, `{mode}`, `{model}` and
//! `{effort}` filled in; a thread in a restricted mode is not attached through a template
//! without `{mode}`, [`acp_attach_refusal`]; `{model}` / `{effort}` alone as an argument is
//! refused when the thread is created, [`acp::template_error`]). Commands are capped at
//! [`twr_adapters::acp::COMMAND_MAX`] bytes.

use serde_json::{json, Map, Value};
use twr_adapters::{acp, pty};
use twr_core::config::Config;
use twr_core::proto::{codes, RpcError};

use super::rpc;

/// Longest command line of a `pty:<command>` runtime, in bytes.
const PTY_COMMAND_MAX: usize = 4096;

/// What a runtime spec names.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct RuntimeSpec {
    /// The adapter (`thread.created` `runtime`).
    pub(super) runtime: String,
    /// A `pty` thread's argv.
    pub(super) pty_command: Option<Vec<String>>,
    /// An `acp` thread's agent.
    pub(super) acp: Option<AcpSpec>,
}

/// The agent of an `acp` thread.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct AcpSpec {
    /// The agent's argv.
    pub(super) command: Vec<String>,
    /// The attach command, `{session}` standing for the session id, `{mode}` for the
    /// thread's permission mode and `{model}` / `{effort}` for its model and effort.
    pub(super) attach: Option<Vec<String>>,
    /// The `[runtimes.<name>]` preset it came from.
    pub(super) preset: Option<String>,
}

impl RuntimeSpec {
    /// Adds the `thread.created` `ext.twr` entries of this spec to `twr`.
    pub(super) fn record(&self, twr: &mut Map<String, Value>) {
        if let Some(argv) = &self.pty_command {
            twr.insert("pty_command".into(), json!(argv));
        }
        if let Some(a) = &self.acp {
            twr.insert("acp_command".into(), json!(a.command));
            if let Some(t) = &a.attach {
                twr.insert("acp_attach".into(), json!(t));
            }
            if let Some(p) = &a.preset {
                twr.insert("acp_preset".into(), json!(p));
            }
        }
    }
}

/// Splits `line` (a `<kind>:` runtime's command) into argv.
fn split(kind: &str, line: &str, max: usize) -> Result<Vec<String>, RpcError> {
    if line.len() > max {
        return Err(rpc(
            codes::BAD_REQUEST,
            format!("the {kind} command is longer than {max} bytes"),
        ));
    }
    pty::split_command_line(line)
        .map_err(|e| rpc(codes::BAD_REQUEST, format!("{kind} command {line:?}: {e}")))
}

/// Resolves `spec` (see the module docs).
pub(super) fn resolve(spec: &str, config: &Config) -> Result<RuntimeSpec, RpcError> {
    if let Some(line) = spec.strip_prefix(pty::RUNTIME_PREFIX) {
        return Ok(RuntimeSpec {
            runtime: pty::NAME.to_string(),
            pty_command: Some(split("pty", line, PTY_COMMAND_MAX)?),
            acp: None,
        });
    }
    if spec == pty::NAME {
        return Err(rpc(
            codes::BAD_REQUEST,
            "the pty runtime needs the command to run: --runtime 'pty:<command line>'",
        ));
    }
    Ok(resolve_acp(spec, config)?.unwrap_or_else(|| RuntimeSpec {
        runtime: spec.to_string(),
        ..Default::default()
    }))
}

/// Resolves `spec` when it names an ACP agent (`acp:<command>` or a `[runtimes.<name>]`
/// preset); `None` for any other spec.
pub(super) fn resolve_acp(spec: &str, config: &Config) -> Result<Option<RuntimeSpec>, RpcError> {
    let acp_spec = |a: AcpSpec| RuntimeSpec {
        runtime: acp::NAME.to_string(),
        pty_command: None,
        acp: Some(a),
    };
    if let Some(line) = spec.strip_prefix(acp::RUNTIME_PREFIX) {
        return Ok(Some(acp_spec(AcpSpec {
            command: split("acp", line, acp::COMMAND_MAX)?,
            attach: None,
            preset: None,
        })));
    }
    if spec == acp::NAME {
        return Err(rpc(
            codes::BAD_REQUEST,
            "the acp runtime needs the agent's command: --runtime 'acp:<command line>', or a \
             `[runtimes.<name>] acp = \"<command line>\"` preset",
        ));
    }
    let Some(preset) = config.runtimes.get(spec) else {
        return Ok(None);
    };
    let command = split("acp", &preset.acp, acp::COMMAND_MAX).map_err(|e| {
        rpc(
            codes::BAD_REQUEST,
            format!("runtimes.{spec}: {}", e.message),
        )
    })?;
    let attach = match &preset.attach {
        Some(a) => {
            let argv = split("attach", a, acp::COMMAND_MAX).map_err(|e| {
                rpc(
                    codes::BAD_REQUEST,
                    format!("runtimes.{spec}: {}", e.message),
                )
            })?;
            if let Some(e) = acp::template_error(&argv) {
                return Err(rpc(codes::BAD_REQUEST, format!("runtimes.{spec}: {e}")));
            }
            Some(argv)
        }
        None => None,
    };
    Ok(Some(acp_spec(AcpSpec {
        command,
        attach,
        preset: Some(spec.to_string()),
    })))
}

/// The runtimes whose own mode ids ([`native_modes`]) a `threads.default_mode` may name.
const MODE_RUNTIMES: [&str; 3] = [
    twr_adapters::claude_code::NAME,
    twr_adapters::codex::NAME,
    twr_adapters::grok_build::NAME,
];

/// A runtime's own documented mode ids (`<adapter>::NATIVE_MODES`); none for the others.
fn native_modes(runtime: &str) -> &'static [&'static str] {
    match runtime {
        twr_adapters::claude_code::NAME => twr_adapters::claude_code::NATIVE_MODES,
        twr_adapters::codex::NAME => twr_adapters::codex::NATIVE_MODES,
        twr_adapters::grok_build::NAME => twr_adapters::grok_build::NATIVE_MODES,
        _ => &[],
    }
}

/// Whether `mode` is spelled exactly as one of twr's own mode ids ([`SHARED_MODES`]) or of
/// `runtime`'s documented ones; any other id (a generic ACP agent's own, kept as given) is
/// shown on a confirm card as a custom mode.
pub(crate) fn known_mode(mode: &str, runtime: &str) -> bool {
    SHARED_MODES.contains(&mode) || native_modes(runtime).contains(&mode)
}

/// A mode id compared with the tables: lowercase, without `-`, `_` and spaces (as
/// `acp::translate::mode_level` compares).
fn mode_key(mode: &str) -> String {
    mode.trim()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}

/// twr's own permission modes: what `--mode` and `threads.default_mode` take on every runtime,
/// each runtime mapping them to its own by level, never wider (docs/runtimes.md, "Permission
/// modes"). The synonym tables of `acp::translate` (`yolo`, `autoEdit`, `always-approve`, ...)
/// are not among them: they recognize what an ACP agent reports, and are no vocabulary to ask
/// for a mode in.
pub(crate) const SHARED_MODES: &[&str] = &[
    "manual",
    "default",
    "plan",
    "read-only",
    "dontAsk",
    "acceptEdits",
    "auto",
    "bypassPermissions",
];

/// The canonical spelling of permission mode `mode` for `runtime` (an adapter name; `None`:
/// `threads.default_mode`, for any runtime):
/// - one of [`SHARED_MODES`] or of the runtime's own ids (`<adapter>::NATIVE_MODES`; with no
///   runtime, Claude Code's, Codex's or Grok Build's), compared as a whole id with case, `-`,
///   `_` and spaces ignored, in its spelling (`READ_ONLY` → `read-only`, `On_Request` →
///   `on-request`);
/// - else, for a runtime that takes any vocabulary ([`twr_adapters::check_mode`]: a generic ACP
///   agent's own mode ids, checked against what its session reports; pty and fake apply no
///   mode), the id as given (trimmed);
/// - anything else, and an empty id: `Err` naming the ids `runtime` takes.
///
/// For a runtime, the result also passes [`twr_adapters::check_mode`] (what its `spawn`
/// checks).
pub(crate) fn canonical_mode(mode: &str, runtime: Option<&str>) -> Result<String, String> {
    let mode = mode.trim();
    if mode.is_empty() {
        return Err("the permission mode is empty".into());
    }
    let key = mode_key(mode);
    let runtimes: Vec<&str> = runtime.map_or(MODE_RUNTIMES.to_vec(), |r| vec![r]);
    let accepted: Vec<&str> = SHARED_MODES
        .iter()
        .chain(runtimes.iter().flat_map(|r| native_modes(r).iter()))
        .copied()
        .collect();
    let canonical = match accepted.iter().find(|id| mode_key(id) == key) {
        Some(id) => (*id).to_string(),
        None => {
            let unknown = || {
                let listed: std::collections::BTreeSet<&str> = accepted.iter().copied().collect();
                let target = runtime.map_or_else(String::new, |r| format!(" for runtime {r:?}"));
                format!(
                    "unknown permission mode {mode:?}{target}; use one of {}",
                    listed.into_iter().collect::<Vec<_>>().join(", ")
                )
            };
            match runtime {
                // A runtime with no vocabulary of its own takes any id it accepts.
                Some(r) if native_modes(r).is_empty() => {
                    twr_adapters::check_mode(r, mode).map_err(|_| unknown())?;
                    mode.to_string()
                }
                _ => return Err(unknown()),
            }
        }
    };
    if let Some(r) = runtime {
        twr_adapters::check_mode(r, &canonical)?;
    }
    Ok(canonical)
}

/// Whether a model or reasoning effort value `v` asks for the runtime's default: blank or
/// `default` (any case). The one convention of `thread.new` `effort` and `thread.set_model`.
pub(crate) fn is_default(v: &str) -> bool {
    let v = v.trim();
    v.is_empty() || v.eq_ignore_ascii_case("default")
}

/// Why model `model` cannot be recorded, if it cannot: over 128 bytes, or with control
/// characters (it is shown in thread lists).
pub(crate) fn model_error(model: &str) -> Option<&'static str> {
    if model.len() > 128 {
        Some("model is longer than 128 bytes")
    } else if model.chars().any(char::is_control) {
        Some("model contains control characters")
    } else {
        None
    }
}

/// The canonical spelling of reasoning effort `effort` for `runtime` (an adapter name), or
/// `None` for the runtime's default ([`is_default`]):
/// - for a runtime with a vocabulary of its own ([`twr_adapters::efforts`]: Claude Code,
///   Codex, Grok Build), one of its values, case-insensitively, in its spelling (`HIGH` →
///   `high`);
/// - for one that takes any value (a generic ACP agent's own, checked by its session; the
///   fake runtime), the value as given (trimmed), at most 64 bytes, no control characters;
/// - anything else, and any effort for `pty`: `Err` naming the values `runtime` takes.
pub(crate) fn canonical_effort(effort: &str, runtime: &str) -> Result<Option<String>, String> {
    if is_default(effort) {
        return Ok(None);
    }
    let effort = effort.trim();
    match twr_adapters::efforts(runtime) {
        Some([]) => Err(format!(
            "the {runtime} runtime takes no reasoning effort (it runs its own command line)"
        )),
        Some(list) => list
            .iter()
            .find(|e| e.eq_ignore_ascii_case(effort))
            .map(|e| Some((*e).to_string()))
            .ok_or_else(|| {
                format!(
                    "unknown reasoning effort {effort:?} for runtime {runtime:?}; use one of {} \
                     (or default for the runtime's own)",
                    list.join(", ")
                )
            }),
        None if effort.len() > 64 => Err("reasoning effort is longer than 64 bytes".into()),
        None if effort.chars().any(char::is_control) => {
            Err("reasoning effort contains control characters".into())
        }
        None => Ok(Some(effort.to_string())),
    }
}

/// Why thread `name` of `runtime`, whose next start resumes a session (`resumes`: it has one,
/// or a live runtime started with a value for `what` may still report one), cannot go from `from` back to the runtime's default `what` (`model` or `effort`):
/// `runtime` keeps a session's `what` on resume ([`twr_adapters::resume_keeps`]), so the
/// session would run on with `from` while the thread said the runtime's default. `None` when
/// nothing is cleared (`to` is a value, or `from` was the default already).
pub(crate) fn clear_refusal(
    runtime: &str,
    name: &str,
    resumes: bool,
    what: &str,
    from: &Option<String>,
    to: &Option<String>,
) -> Option<String> {
    let keeps = twr_adapters::resume_keeps(runtime);
    let kept = match what {
        "model" => keeps.model,
        _ => keeps.effort,
    };
    let from = from
        .as_deref()
        .filter(|_| resumes && kept && to.is_none())?;
    let choices = match twr_adapters::efforts(runtime) {
        Some(list) if what == "effort" && !list.is_empty() => format!(" ({})", list.join(", ")),
        _ => String::new(),
    };
    let a = if what == "effort" { "an" } else { "a" };
    Some(format!(
        "{runtime} keeps a session's {what} on resume, so {name} cannot go back to the \
         runtime's default {what} (its session would go on with {from}): pick {a} {what} \
         instead{choices}"
    ))
}

/// Why a thread of `runtime` on the hub's own box (`local`) cannot run with a reasoning
/// effort, as far as the hub knows without probing: the installed Claude Code does not list
/// `--effort` (`twr_adapters::claude_code::effort_refusal`, from what an earlier spawn read).
/// A remote box's binaries are not known here (a spawn there refuses it itself).
pub(crate) fn effort_refusal(runtime: &str, local: bool) -> Option<String> {
    (local && runtime == twr_adapters::claude_code::NAME)
        .then(twr_adapters::claude_code::effort_refusal)
        .flatten()
}

/// Why thread `name` (permission `mode`) of an `acp` runtime with environment `env` (its
/// [`acp_env`]) cannot be attached, when the reason is its preset's attach template: the
/// thread's mode is restricted and the template has no `{mode}`, so the interactive CLI would
/// run in its own default mode (`twr_adapters::acp::attach_argv` refuses it too).
pub(super) fn acp_attach_refusal(
    name: &str,
    mode: &str,
    env: &[(String, String)],
) -> Option<String> {
    use acp::translate::{mode_level, ModeLevel};
    let template: Vec<String> = env
        .iter()
        .find(|(k, _)| k == acp::ATTACH_ENV)
        .and_then(|(_, v)| serde_json::from_str(v).ok())?;
    let restricted = mode_level(mode) == Some(ModeLevel::Restricted);
    (restricted && !template.iter().any(|a| a.contains(acp::MODE_PLACEHOLDER))).then(|| {
        format!(
            "cannot attach {name}: its preset's attach template has no {}, and the thread's \
             mode {mode:?} is restricted; the interactive CLI would run in its own default \
             mode (put {} in the preset's `attach` where the mode goes)",
            acp::MODE_PLACEHOLDER,
            acp::MODE_PLACEHOLDER
        )
    })
}

/// The environment an `acp` thread's runtime gets from its `thread.created` `ext.twr`:
/// [`acp::COMMAND_ENV`] and, with an attach template, [`acp::ATTACH_ENV`] (JSON arrays).
pub(super) fn acp_env(twr: Option<&Map<String, Value>>) -> Vec<(String, String)> {
    let Some(twr) = twr else {
        return Vec::new();
    };
    let mut env = Vec::new();
    for (key, var) in [
        ("acp_command", acp::COMMAND_ENV),
        ("acp_attach", acp::ATTACH_ENV),
    ] {
        if let Some(v) = twr.get(key).filter(|v| v.is_array()) {
            env.push((var.to_string(), v.to_string()));
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config::from_toml_str(
            "[runtimes.gemini]\nacp = \"gemini --experimental-acp\"\nattach = \"gemini --resume {session}\"\n[runtimes.broken]\nacp = \"x 'unterminated\"",
        )
        .unwrap()
    }

    #[test]
    fn specs_resolve() {
        let c = config();
        let s = resolve("acp:opencode acp --port 0", &c).unwrap();
        assert_eq!(s.runtime, "acp");
        assert_eq!(
            s.acp.as_ref().unwrap().command,
            ["opencode", "acp", "--port", "0"]
        );
        let s = resolve("gemini", &c).unwrap();
        let a = s.acp.as_ref().unwrap();
        assert_eq!(a.command, ["gemini", "--experimental-acp"]);
        assert_eq!(
            a.attach.as_deref().unwrap(),
            ["gemini", "--resume", "{session}"]
        );
        assert_eq!(a.preset.as_deref(), Some("gemini"));
        let mut twr = Map::new();
        s.record(&mut twr);
        assert_eq!(
            acp_env(Some(&twr)),
            [
                (
                    acp::COMMAND_ENV.to_string(),
                    r#"["gemini","--experimental-acp"]"#.to_string()
                ),
                (
                    acp::ATTACH_ENV.to_string(),
                    r#"["gemini","--resume","{session}"]"#.to_string()
                ),
            ]
        );
        assert_eq!(twr["acp_preset"], "gemini");
        let s = resolve("pty:aider --yes", &c).unwrap();
        assert_eq!((s.runtime.as_str(), s.acp.is_none()), ("pty", true));
        assert_eq!(resolve("codex", &c).unwrap().runtime, "codex");
        assert!(resolve_acp("codex", &c).unwrap().is_none());
        assert!(resolve_acp("pty:x", &c).unwrap().is_none());
        for bad in ["acp", "acp:", "acp:   ", "acp:x 'y", "pty", "broken"] {
            assert!(resolve(bad, &c).is_err(), "{bad}");
        }
        assert!(resolve(&format!("acp:{}", "x".repeat(5000)), &c).is_err());
    }

    #[test]
    fn modes_are_canonicalized_or_refused() {
        for (given, want) in [
            ("manual", "manual"),
            ("Default", "default"),
            ("READ_ONLY", "read-only"),
            ("readOnly", "read-only"),
            (" dont-ask ", "dontAsk"),
            ("accept_edits", "acceptEdits"),
            ("Bypass-Permissions", "bypassPermissions"),
        ] {
            assert_eq!(canonical_mode(given, None).as_deref(), Ok(want), "{given}");
            for r in ["claude-code", "codex", "grok-build", "acp", "fake"] {
                assert_eq!(
                    canonical_mode(given, Some(r)).as_deref(),
                    Ok(want),
                    "{given} {r}"
                );
            }
        }
        // The synonym tables recognize what ACP agents report; they are not
        // ids to ask for. Refused where the runtime has a vocabulary of its own (listing only
        // what that runtime takes), kept as given for a generic ACP agent (its vocabulary).
        for synonym in ["dangerously-allow-all", "always-approve", "ask", "autoEdit"] {
            for r in ["claude-code", "codex", "grok-build"] {
                let e = canonical_mode(synonym, Some(r)).unwrap_err();
                assert!(e.contains("unknown permission mode"), "{synonym} {r}: {e}");
            }
            assert!(canonical_mode(synonym, None).is_err(), "{synonym}");
            assert_eq!(canonical_mode(synonym, Some("acp")).as_deref(), Ok(synonym));
        }
        assert_eq!(
            canonical_mode("bogus", Some("claude-code")).unwrap_err(),
            "unknown permission mode \"bogus\" for runtime \"claude-code\"; use one of \
             acceptEdits, auto, bypassPermissions, default, dontAsk, manual, plan, read-only"
        );
        assert_eq!(
            canonical_mode("bogus", Some("grok-build")).unwrap_err(),
            canonical_mode("bogus", Some("claude-code"))
                .unwrap_err()
                .replace("claude-code", "grok-build")
        );
        // `yolo` is Codex's own (twr maps it like bypassPermissions there), nobody else's.
        assert_eq!(canonical_mode("YOLO", Some("codex")).as_deref(), Ok("yolo"));
        assert_eq!(canonical_mode("YOLO", None).as_deref(), Ok("yolo"));
        assert!(canonical_mode("yolo", Some("claude-code")).is_err());
        assert_eq!(canonical_mode("YOLO", Some("acp")).as_deref(), Ok("YOLO"));
        // Codex's own ids, in its spelling: for codex (or the default mode), not for Claude.
        for (given, want) in [
            ("untrusted", "untrusted"),
            ("On_Request", "on-request"),
            ("NEVER", "never"),
        ] {
            assert_eq!(canonical_mode(given, Some("codex")).as_deref(), Ok(want));
            assert_eq!(canonical_mode(given, None).as_deref(), Ok(want));
            assert!(
                canonical_mode(given, Some("claude-code")).is_err(),
                "{given}"
            );
            assert!(
                canonical_mode(given, Some("grok-build")).is_err(),
                "{given}"
            );
        }
        // A generic ACP agent's own vocabulary is its (checked against its session).
        assert_eq!(
            canonical_mode("architect", Some("acp")).as_deref(),
            Ok("architect")
        );
        for bad in ["planning", "bypass", "mañual", "architect"] {
            for r in [Some("codex"), Some("claude-code"), None] {
                let e = canonical_mode(bad, r).unwrap_err();
                assert!(e.contains("unknown permission mode"), "{bad} {r:?}: {e}");
            }
        }
        assert!(canonical_mode("bogus", None)
            .unwrap_err()
            .contains("untrusted"));
        for empty in ["", "  "] {
            assert!(canonical_mode(empty, Some("acp")).is_err());
            assert!(canonical_mode(empty, None).is_err());
        }
    }

    /// Efforts: a runtime's own vocabulary in its spelling, the agent's own for a generic ACP
    /// agent, none for pty; blank or `default` is the runtime's default everywhere.
    #[test]
    fn efforts_are_canonicalized_or_refused() {
        for r in ["claude-code", "codex", "grok-build", "acp", "fake", "pty"] {
            for d in ["", "  ", "default", "Default", " DEFAULT "] {
                assert_eq!(canonical_effort(d, r), Ok(None), "{d:?} {r}");
            }
        }
        for r in ["claude-code", "codex", "grok-build"] {
            assert_eq!(
                canonical_effort(" HIGH ", r),
                Ok(Some("high".into())),
                "{r}"
            );
        }
        assert_eq!(
            canonical_effort("max", "claude-code"),
            Ok(Some("max".into()))
        );
        assert_eq!(
            canonical_effort("bogus", "claude-code").unwrap_err(),
            "unknown reasoning effort \"bogus\" for runtime \"claude-code\"; use one of low, \
             medium, high, xhigh, max (or default for the runtime's own)"
        );
        // Grok 1.0.44 has no `max`; Codex has `minimal` and `ultra`, Claude Code neither.
        let e = canonical_effort("max", "grok-build").unwrap_err();
        assert!(
            e.ends_with("use one of low, medium, high, xhigh (or default for the runtime's own)"),
            "{e}"
        );
        assert_eq!(canonical_effort("Ultra", "codex"), Ok(Some("ultra".into())));
        assert!(canonical_effort("minimal", "claude-code").is_err());
        assert_eq!(canonical_effort(" Deep ", "acp"), Ok(Some("Deep".into())));
        assert!(canonical_effort(&"x".repeat(65), "acp").is_err());
        assert!(canonical_effort("a\u{1b}b", "acp").is_err());
        assert!(canonical_effort("high", "pty")
            .unwrap_err()
            .contains("takes no reasoning effort"));
        assert!(is_default("default") && !is_default("defaults"));
        assert_eq!(
            model_error(&"m".repeat(129)),
            Some("model is longer than 128 bytes")
        );
        assert_eq!(
            model_error("a\nb"),
            Some("model contains control characters")
        );
        assert_eq!(model_error("claude-fable-5-1"), None);
        // Only a local Claude Code thread is checked against its binary.
        assert_eq!(effort_refusal("codex", true), None);
        assert_eq!(effort_refusal("claude-code", false), None);
    }

    /// A clear back to the runtime's default is refused only for a thread with a session whose
    /// runtime keeps that setting on resume (`twr_adapters::resume_keeps`).
    #[test]
    fn clears_a_resumed_session_would_ignore_are_refused() {
        let low = Some("low".to_string());
        let high = Some("high".to_string());
        assert_eq!(
            clear_refusal("codex", "api", true, "effort", &low, &None).as_deref(),
            Some(
                "codex keeps a session's effort on resume, so api cannot go back to the \
                 runtime's default effort (its session would go on with low): pick an effort \
                 instead (minimal, low, medium, high, xhigh, max, ultra)"
            )
        );
        let m = Some("fable".to_string());
        let e = clear_refusal("claude-code", "api", true, "model", &m, &None).unwrap();
        assert!(
            e.ends_with("would go on with fable): pick a model instead"),
            "{e}"
        );
        assert!(clear_refusal("acp", "api", true, "effort", &low, &None).is_some());
        assert!(clear_refusal("grok-build", "api", true, "model", &m, &None).is_some());
        // Claude Code resumes on the default effort when none is passed.
        assert_eq!(
            clear_refusal("claude-code", "api", true, "effort", &low, &None),
            None
        );
        // No session yet (the next start is a new one), a value, nothing to clear, the fake.
        assert_eq!(
            clear_refusal("codex", "api", false, "effort", &low, &None),
            None
        );
        assert_eq!(
            clear_refusal("codex", "api", true, "effort", &low, &high),
            None
        );
        assert_eq!(
            clear_refusal("codex", "api", true, "effort", &None, &None),
            None
        );
        assert_eq!(clear_refusal("fake", "api", true, "model", &m, &None), None);
    }

    /// The hub gates a mode call by comparing its canonical mode, exactly, with the mode
    /// a command names (`twr_decisions::code::command::mode_coverage`). That holds only while
    /// every mode id a command names is its own canonical form on every runtime
    /// that takes it, and an id kept as given by a runtime with no vocabulary of its own
    /// ("p'lan") is none of them.
    #[test]
    fn the_command_modes_are_their_own_canonical_form() {
        use twr_decisions::code::command::MODES;
        for (id, _) in MODES {
            assert!(SHARED_MODES.contains(id), "{id}");
            for r in ["claude-code", "codex", "grok-build", "acp", "fake", "pty"] {
                if let Ok(c) = canonical_mode(id, Some(r)) {
                    assert_eq!(c, *id, "{id} {r}");
                }
            }
            assert_eq!(canonical_mode(id, None).as_deref(), Ok(*id));
        }
        for (given, kept) in [("p'lan", "p'lan"), ("plan\u{200b}", "plan\u{200b}")] {
            assert_eq!(canonical_mode(given, Some("acp")).as_deref(), Ok(kept));
            assert!(!MODES.iter().any(|(id, _)| *id == kept));
        }
        for given in ["PLAN", "pl-an", "plan ", " Plan", "p_lan"] {
            assert_eq!(
                canonical_mode(given, Some("acp")).as_deref(),
                Ok("plan"),
                "{given:?}"
            );
        }
    }
}

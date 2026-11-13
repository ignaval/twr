//! Loop settings: what the setup card shows and `loop.settings_saved` keeps, the presets of
//! its editor, the checks of a saved set against what the hub found installed, and the
//! replacements the card shows for what is missing (docs/design/loops.md, "Loop settings").
//!
//! Nothing here reads the machine: the hub fills an [`Availability`] from the runtimes it
//! found and passes it in.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use super::config::{
    parse_implementer, ImplCommit, LoopsConfig, ModelChoice, PlanCheck, ReviewCommit,
    ReviewerChoice, SweepMode, WorkersChoice, REVIEWER_RUNTIMES,
};
use super::LoopKind;
use crate::event::{Event, EventBody, HUB_THREAD};

/// The settings a loop runs on: one set for both kinds of loop. Scope and task belong to one
/// run and are not here.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopSettings {
    /// The profile's name.
    pub profile: String,
    /// The implementer: `None` is the thread's own model (an existing thread keeps its own, a
    /// new one gets its project's); a named one is applied to the thread on the same runtime
    /// before the loop's first turn.
    pub implementer: Option<ModelChoice>,
    /// The implementer's subagents, where its runtime has them.
    pub workers: WorkersChoice,
    /// The reviewer.
    pub reviewer: ReviewerChoice,
    /// An implementation loop's plan check; `None`: the profile's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_check: Option<PlanCheck>,
    /// What an implementation loop commits.
    pub commit: ImplCommit,
    /// What a review loop commits.
    pub review_commit: ReviewCommit,
    /// Who audits the sweep's rules.
    pub sweep: SweepMode,
}

impl LoopSettings {
    /// What the first setup card shows: `[loops]` of the config file (its defaults when the
    /// file has none). A key that does not parse, which [`LoopsConfig::validate`] refuses at
    /// load, falls back to the built-in default.
    pub fn first_card(config: &LoopsConfig) -> LoopSettings {
        let defaults = LoopsConfig::default();
        LoopSettings {
            profile: if config.profile(&config.profile).is_some() {
                config.profile.clone()
            } else {
                defaults.profile.clone()
            },
            implementer: config
                .implementer()
                .or_else(|_| defaults.implementer())
                .unwrap_or(None),
            workers: config
                .workers()
                .or_else(|_| defaults.workers())
                .unwrap_or_default(),
            reviewer: config
                .reviewer()
                .or_else(|_| defaults.reviewer())
                .unwrap_or_default(),
            plan_check: None,
            commit: config.commit,
            review_commit: ReviewCommit::Match,
            sweep: config.sweep,
        }
    }

    /// These settings with preset `preset`'s implementer, workers and reviewer.
    pub fn with_preset(&self, preset: &Preset) -> LoopSettings {
        LoopSettings {
            implementer: Some(preset.implementer.clone()),
            workers: preset.workers.clone(),
            reviewer: preset.reviewer.clone(),
            ..self.clone()
        }
    }
}

/// Where the editor saves: for every project of the workspace, or for the thread's project
/// only (a project with settings of its own uses that whole set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveFor {
    /// Every project without settings of its own.
    #[default]
    All,
    /// The thread's project.
    Project,
}

/// Where `loop.settings_saved` came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedFrom {
    /// An answer to a setup card.
    #[default]
    SetupItem,
    /// The settings editor of a user's client.
    Editor,
}

/// Who saved settings: only ever the user (the hub refuses everyone else).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SavedBy {
    /// The user.
    #[default]
    User,
}

/// What `loop.started` ran on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsSource {
    /// The settings an answered setup card showed.
    #[default]
    SetupItem,
    /// Saved settings that still held.
    Saved,
}

// ---------------------------------------------------------------------------------------------
// Presets and model families
// ---------------------------------------------------------------------------------------------

/// A preset of the settings editor: an implementer, workers and a reviewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preset {
    /// Its id (`claude-code+codex`).
    pub id: String,
    /// What the editor calls it (`Claude Code + Codex`).
    pub label: String,
    /// The implementer.
    pub implementer: ModelChoice,
    /// The workers.
    pub workers: WorkersChoice,
    /// The reviewer.
    pub reviewer: ReviewerChoice,
}

/// The model (and effort) twr picks on `runtime` when it has to pick one: a preset's, or the
/// replacement for a runtime that is missing ([`propose`]). `None` for a runtime it has none
/// for.
pub fn default_model(runtime: &str) -> Option<(&'static str, &'static str)> {
    match runtime {
        "claude-code" => Some(("claude-opus-5-5", "high")),
        "codex" => Some(("gpt-6.1-sol", "high")),
        "grok-build" => Some(("grok-4.7", "high")),
        _ => None,
    }
}

/// The editor's four presets, the default first: **Claude Code + Codex**, **all Claude
/// Code**, **all Codex**, **all Grok**.
pub fn presets() -> Vec<Preset> {
    let named = |rt: &str| {
        let (model, effort) = default_model(rt).unwrap_or(("", "high"));
        ModelChoice {
            runtime: rt.into(),
            account: None,
            model: model.into(),
            effort: Some(effort.into()),
        }
    };
    let workers = |rt: &str| {
        let m = named(rt);
        WorkersChoice {
            model: m.model,
            effort: m.effort,
        }
    };
    let reviewer = |rt: &str| ReviewerChoice {
        runtime: rt.into(),
        account: None,
        model: named(rt).model,
    };
    let preset = |id: &str, label: &str, imp: &str, rev: &str| Preset {
        id: id.into(),
        label: label.into(),
        implementer: named(imp),
        workers: workers(imp),
        reviewer: reviewer(rev),
    };
    vec![
        preset(
            "claude-code+codex",
            "Claude Code + Codex",
            "claude-code",
            "codex",
        ),
        preset(
            "claude-code",
            "all Claude Code",
            "claude-code",
            "claude-code",
        ),
        preset("codex", "all Codex", "codex", "codex"),
        preset("grok-build", "all Grok", "grok-build", "grok-build"),
    ]
}

/// The family of model `model` (`claude`, `gpt`, `grok`), for the same-family warning:
/// Claude's full ids and Claude Code's aliases (`opus`, `sonnet`, `haiku`, `fable`,
/// `opusplan`), OpenAI's `gpt-*` and `codex-*`, xAI's `grok-*`. `None`: a model it does not
/// recognize (no warning).
pub fn model_family(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    let alias = ["opus", "sonnet", "haiku", "fable", "opusplan"];
    if m.contains("claude")
        || alias
            .iter()
            .any(|a| m == *a || m.starts_with(&format!("{a}-")))
    {
        Some("claude")
    } else if m.contains("grok") {
        Some("grok")
    } else if m.contains("gpt") || m.starts_with("codex") {
        Some("gpt")
    } else {
        None
    }
}

/// The family of runtime `runtime`'s own models, for a model left to the runtime's default.
pub fn runtime_family(runtime: &str) -> Option<&'static str> {
    match runtime {
        "claude-code" => Some("claude"),
        "codex" => Some("gpt"),
        "grok-build" => Some("grok"),
        _ => None,
    }
}

/// The same-family warning of the setup card.
pub const SAME_FAMILY_WARNING: &str =
    "reviewer and implementer are the same family: it tends to miss what the implementer missed";

// ---------------------------------------------------------------------------------------------
// Availability, validation and replacements
// ---------------------------------------------------------------------------------------------

/// What the hub found of one runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeAvailability {
    /// The runtime.
    pub runtime: String,
    /// Its CLI is installed.
    pub found: bool,
    /// It is logged in (its default account, or the one a setting names).
    pub login: bool,
    /// The hub probed its agent CLIs. `false`: it does not (`TWR_AGENT_PROBE=off`), so it
    /// does not know whether this one is installed or logged in, and counts it as both
    /// (`found` and `login` are true): no setting is refused or replaced for it, and a run
    /// whose CLI is missing fails as any failed run does.
    pub probed: bool,
    /// The models it reports; empty: it reports none, and no model is checked.
    pub models: Vec<String>,
    /// The efforts it takes; empty: none is checked.
    pub efforts: Vec<String>,
    /// The accounts twr knows on it; empty: no account is checked.
    pub accounts: Vec<String>,
    /// It runs subagents of its own (the workers apply).
    pub subagents: bool,
}

impl Default for RuntimeAvailability {
    fn default() -> RuntimeAvailability {
        RuntimeAvailability {
            runtime: String::new(),
            found: false,
            login: false,
            probed: true,
            models: Vec::new(),
            efforts: Vec::new(),
            accounts: Vec::new(),
            subagents: false,
        }
    }
}

/// The thread a loop would run in: its runtime and model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThreadModel {
    /// Its runtime.
    pub runtime: String,
    /// Its model; `None`: the runtime's default.
    pub model: Option<String>,
    /// Its effort; `None`: the runtime's default.
    pub effort: Option<String>,
}

/// What a set of settings is checked against: the runtimes the hub found (one it did not list
/// is not installed) and the thread the loop would run in (`None` outside a thread, as in the
/// settings editor).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Availability {
    /// The runtimes, as found.
    pub runtimes: Vec<RuntimeAvailability>,
    /// The thread.
    pub thread: Option<ThreadModel>,
}

impl Availability {
    /// What was found of `runtime`.
    pub fn runtime(&self, runtime: &str) -> Option<&RuntimeAvailability> {
        self.runtimes.iter().find(|r| r.runtime == runtime)
    }

    /// Whether `runtime` is installed and logged in.
    pub fn usable(&self, runtime: &str) -> bool {
        self.runtime(runtime).is_some_and(|r| r.found && r.login)
    }
}

/// The setting a [`Problem`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The profile.
    Profile,
    /// The implementer.
    Implementer,
    /// The workers.
    Workers,
    /// The reviewer.
    Reviewer,
}

/// What is wrong with a setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    /// The profile is gone from the config file.
    UnknownProfile,
    /// The runtime is not installed.
    NotInstalled,
    /// The runtime is not logged in.
    LoggedOut,
    /// The runtime no longer reports the model.
    UnknownModel,
    /// The runtime does not take the effort.
    UnknownEffort,
    /// The runtime has no such account.
    UnknownAccount,
    /// The runtime can't review ([`REVIEWER_RUNTIMES`]).
    NotAReviewer,
    /// The thread runs on another runtime than the named implementer (a loop never moves a
    /// thread to another runtime).
    OtherRuntime,
}

/// A saved setting that no longer holds: the card comes back for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// The setting.
    pub role: Role,
    /// What is wrong.
    pub kind: ProblemKind,
    /// The words for the card (`codex isn't installed`).
    pub message: String,
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// The runtime a model setting checks: its runtime's availability problem, if any.
fn runtime_problem(role: Role, runtime: &str, avail: &Availability) -> Option<Problem> {
    let (kind, message) = match avail.runtime(runtime) {
        None => (
            ProblemKind::NotInstalled,
            format!("{runtime} isn't installed"),
        ),
        Some(r) if !r.found => (
            ProblemKind::NotInstalled,
            format!("{runtime} isn't installed"),
        ),
        Some(r) if !r.login => (ProblemKind::LoggedOut, format!("{runtime} isn't logged in")),
        Some(_) => return None,
    };
    Some(Problem {
        role,
        kind,
        message,
    })
}

/// `model` on `runtime`, if the runtime reports models and not this one.
fn model_problem(role: Role, runtime: &str, model: &str, avail: &Availability) -> Option<Problem> {
    let r = avail.runtime(runtime)?;
    (!r.models.is_empty() && !r.models.iter().any(|m| m == model)).then(|| Problem {
        role,
        kind: ProblemKind::UnknownModel,
        message: format!("{runtime} no longer offers {model}"),
    })
}

/// `effort` on `runtime`, if the runtime lists efforts and not this one.
fn effort_problem(
    role: Role,
    runtime: &str,
    effort: &str,
    avail: &Availability,
) -> Option<Problem> {
    let r = avail.runtime(runtime)?;
    (!r.efforts.is_empty() && !r.efforts.iter().any(|e| e == effort)).then(|| Problem {
        role,
        kind: ProblemKind::UnknownEffort,
        message: format!(
            "{runtime} takes no effort {effort} ({})",
            r.efforts.join(", ")
        ),
    })
}

/// `account` on `runtime`, if twr knows the runtime's accounts and not this one.
fn account_problem(
    role: Role,
    runtime: &str,
    account: &str,
    avail: &Availability,
) -> Option<Problem> {
    let r = avail.runtime(runtime)?;
    (!r.accounts.is_empty() && !r.accounts.iter().any(|a| a == account)).then(|| Problem {
        role,
        kind: ProblemKind::UnknownAccount,
        message: format!("{runtime} has no account {account}"),
    })
}

/// The runtime the implementer runs on: the named one's, else the thread's.
fn implementer_runtime<'a>(s: &'a LoopSettings, avail: &'a Availability) -> Option<&'a str> {
    match &s.implementer {
        Some(m) => Some(m.runtime.as_str()),
        None => avail.thread.as_ref().map(|t| t.runtime.as_str()),
    }
}

/// What no longer holds of `s`, given what the hub found (`avail`) and the profiles of
/// `config`: a profile that is gone; a reviewer that can't review, isn't installed or logged
/// in, or whose model, account or the profile's efforts its runtime doesn't take; a named
/// implementer likewise, or on another runtime than the thread's; workers whose model or effort
/// the implementer's runtime doesn't take (checked only where it runs subagents). An empty list
/// means the settings hold and a loop may start on them without the card.
pub fn validate(s: &LoopSettings, avail: &Availability, config: &LoopsConfig) -> Vec<Problem> {
    let mut out = Vec::new();
    let profile = config.profile(&s.profile);
    if profile.is_none() {
        out.push(Problem {
            role: Role::Profile,
            kind: ProblemKind::UnknownProfile,
            message: format!("there is no profile {:?} any more", s.profile),
        });
    }
    let r = &s.reviewer;
    if !REVIEWER_RUNTIMES.contains(&r.runtime.as_str()) {
        out.push(Problem {
            role: Role::Reviewer,
            kind: ProblemKind::NotAReviewer,
            message: format!(
                "{} can't review ({} can)",
                r.runtime,
                REVIEWER_RUNTIMES.join(", ")
            ),
        });
    } else if let Some(p) = runtime_problem(Role::Reviewer, &r.runtime, avail) {
        out.push(p);
    } else {
        out.extend(model_problem(Role::Reviewer, &r.runtime, &r.model, avail));
        if let Some(a) = &r.account {
            out.extend(account_problem(Role::Reviewer, &r.runtime, a, avail));
        }
        if let Some(p) = &profile {
            let mut efforts: Vec<&str> = p.tiers.iter().map(|t| t.effort.as_str()).collect();
            efforts.push(&p.verify.effort);
            efforts.dedup();
            for e in efforts {
                if let Some(mut problem) = effort_problem(Role::Reviewer, &r.runtime, e, avail) {
                    problem
                        .message
                        .push_str(&format!(": profile {} asks for it", p.name));
                    out.push(problem);
                    break;
                }
            }
        }
    }
    if let Some(m) = &s.implementer {
        if let Some(p) = runtime_problem(Role::Implementer, &m.runtime, avail) {
            out.push(p);
        } else {
            out.extend(model_problem(
                Role::Implementer,
                &m.runtime,
                &m.model,
                avail,
            ));
            if let Some(e) = &m.effort {
                out.extend(effort_problem(Role::Implementer, &m.runtime, e, avail));
            }
            if let Some(a) = &m.account {
                out.extend(account_problem(Role::Implementer, &m.runtime, a, avail));
            }
        }
        if let Some(t) = &avail.thread {
            if t.runtime != m.runtime {
                out.push(Problem {
                    role: Role::Implementer,
                    kind: ProblemKind::OtherRuntime,
                    message: format!("the thread runs on {}, not {}", t.runtime, m.runtime),
                });
            }
        }
    }
    if let Some(rt) = implementer_runtime(s, avail) {
        if avail.runtime(rt).is_some_and(|r| r.subagents) {
            out.extend(model_problem(Role::Workers, rt, &s.workers.model, avail));
            if let Some(e) = &s.workers.effort {
                out.extend(effort_problem(Role::Workers, rt, e, avail));
            }
        }
    }
    out
}

/// What the setup card shows for a set of settings: the settings with what is missing
/// replaced, the card's warnings, and what is still wrong (which the user has to change
/// before the loop can start).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    /// The settings the card shows.
    pub settings: LoopSettings,
    /// Its warnings, in order: each replacement, then the same-family warning.
    pub warnings: Vec<String>,
    /// What [`validate`] still finds in [`Proposal::settings`].
    pub problems: Vec<Problem>,
}

/// The model to pick on `runtime`: its default ([`default_model`]) if the runtime offers it
/// (or reports no models), else the first model it reports.
fn pick_model(runtime: &str, avail: &Availability) -> Option<(String, Option<String>)> {
    let reported = avail
        .runtime(runtime)
        .map(|r| r.models.as_slice())
        .unwrap_or(&[]);
    match default_model(runtime) {
        Some((m, e)) if reported.is_empty() || reported.iter().any(|r| r == m) => {
            Some((m.to_string(), Some(e.to_string())))
        }
        _ => reported.first().map(|m| (m.clone(), None)),
    }
}

/// The first runtime of the reviewer runtimes (Claude Code first) other than `not` that is
/// installed and logged in.
fn next_runtime(not: &str, avail: &Availability) -> Option<&'static str> {
    REVIEWER_RUNTIMES
        .iter()
        .copied()
        .find(|rt| *rt != not && avail.usable(rt))
}

/// The settings the setup card shows for `s` (saved settings, the first card's, or a request's
/// overrides applied to them): each role whose runtime isn't installed or logged in moved to
/// the next runtime that is, Claude Code first, on its default model, with the warning
/// `codex isn't installed: reviewing with claude-code · claude-opus-5-5 instead`; a model the
/// runtime no longer offers replaced by its default; an unknown account dropped; a named
/// implementer on another runtime than the thread's replaced by the thread's own model; a
/// profile that is gone replaced by `[loops] profile`. Then the same-family warning
/// ([`SAME_FAMILY_WARNING`]) if the reviewer is the implementer's family. Nothing is swapped
/// without a warning saying so.
pub fn propose(s: &LoopSettings, avail: &Availability, config: &LoopsConfig) -> Proposal {
    let mut out = s.clone();
    let mut warnings = Vec::new();
    for p in validate(s, avail, config) {
        match (p.role, p.kind) {
            (Role::Profile, _) => {
                let fallback = LoopSettings::first_card(config).profile;
                warnings.push(format!("{}: {fallback} instead", p.message));
                out.profile = fallback;
            }
            (
                Role::Reviewer,
                ProblemKind::NotInstalled | ProblemKind::LoggedOut | ProblemKind::NotAReviewer,
            ) => {
                if let Some(rt) = next_runtime(&s.reviewer.runtime, avail) {
                    if let Some((model, _)) = pick_model(rt, avail) {
                        warnings.push(format!(
                            "{}: reviewing with {rt} · {model} instead",
                            p.message
                        ));
                        out.reviewer = ReviewerChoice {
                            runtime: rt.into(),
                            account: None,
                            model,
                        };
                    }
                }
            }
            (Role::Reviewer, ProblemKind::UnknownModel) => {
                if let Some((model, _)) = pick_model(&s.reviewer.runtime, avail) {
                    warnings.push(format!(
                        "{}: reviewing with {} · {model} instead",
                        p.message, s.reviewer.runtime
                    ));
                    out.reviewer.model = model;
                }
            }
            (Role::Reviewer | Role::Implementer, ProblemKind::UnknownAccount) => {
                warnings.push(format!("{}: using its default account", p.message));
                match p.role {
                    Role::Reviewer => out.reviewer.account = None,
                    _ => {
                        if let Some(m) = &mut out.implementer {
                            m.account = None;
                        }
                    }
                }
            }
            (Role::Implementer, ProblemKind::OtherRuntime) => {
                warnings.push(format!(
                    "{}: implementing with the thread's own model",
                    p.message
                ));
                out.implementer = None;
            }
            (Role::Implementer, ProblemKind::NotInstalled | ProblemKind::LoggedOut) => {
                // (A thread on another runtime gets its own model, above.)
                let other = avail
                    .thread
                    .as_ref()
                    .zip(s.implementer.as_ref())
                    .is_some_and(|(t, m)| t.runtime != m.runtime);
                let from = s.implementer.as_ref().map_or("", |m| m.runtime.as_str());
                if let (false, Some(rt)) = (other, next_runtime(from, avail)) {
                    if let Some((model, effort)) = pick_model(rt, avail) {
                        warnings.push(format!(
                            "{}: implementing with {rt} · {model} instead",
                            p.message
                        ));
                        out.implementer = Some(ModelChoice {
                            runtime: rt.into(),
                            account: None,
                            model,
                            effort,
                        });
                    }
                }
            }
            (Role::Implementer, ProblemKind::UnknownModel) => {
                if let Some(m) = &mut out.implementer {
                    if let Some((model, _)) = pick_model(&m.runtime, avail) {
                        warnings.push(format!(
                            "{}: implementing with {} · {model} instead",
                            p.message, m.runtime
                        ));
                        m.model = model;
                    }
                }
            }
            (Role::Implementer, ProblemKind::UnknownEffort) => {
                warnings.push(format!(
                    "{}: the runtime's default effort instead",
                    p.message
                ));
                if let Some(m) = &mut out.implementer {
                    m.effort = None;
                }
            }
            (Role::Workers, ProblemKind::UnknownModel) => {
                if let Some(rt) = implementer_runtime(s, avail) {
                    if let Some((model, _)) = pick_model(rt, avail) {
                        warnings.push(format!("{}: workers on {model} instead", p.message));
                        out.workers.model = model;
                    }
                }
            }
            (Role::Workers, ProblemKind::UnknownEffort) => {
                warnings.push(format!(
                    "{}: the runtime's default effort instead",
                    p.message
                ));
                out.workers.effort = None;
            }
            // (A reviewer effort is the profile's: the user changes the profile or the
            // reviewer.)
            _ => {}
        }
    }
    if same_family(&out, avail) {
        warnings.push(SAME_FAMILY_WARNING.to_string());
    }
    let problems = validate(&out, avail, config);
    Proposal {
        settings: out,
        warnings,
        problems,
    }
}

/// Whether the reviewer of `s` is the implementer's model family ([`model_family`]): the
/// named implementer's, or the thread's model (or its runtime's family when it runs the
/// runtime's default).
pub fn same_family(s: &LoopSettings, avail: &Availability) -> bool {
    let family = |model: Option<&str>, runtime: &str| {
        model
            .and_then(model_family)
            .or_else(|| runtime_family(runtime))
    };
    let implementer = match &s.implementer {
        Some(m) => family(Some(&m.model), &m.runtime),
        None => avail
            .thread
            .as_ref()
            .and_then(|t| family(t.model.as_deref(), &t.runtime)),
    };
    let reviewer = family(Some(&s.reviewer.model), &s.reviewer.runtime);
    implementer.is_some() && implementer == reviewer
}

// ---------------------------------------------------------------------------------------------
// Overrides and scope
// ---------------------------------------------------------------------------------------------

/// What a request changes for one run (`/review --profile low`), as the user typed it. Applied
/// to saved settings it leaves them alone; before the first save it prefills the card.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    /// A profile name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// `thread` or `runtime[:account]/model[:effort]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implementer: Option<String>,
    /// `model[:effort]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<String>,
    /// `runtime[:account]/model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<String>,
    /// The loop kind's commit: `end`, `per-round` or `none` (implementation), `match` or `none`
    /// (review).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl Overrides {
    /// Whether it changes nothing.
    pub fn is_empty(&self) -> bool {
        *self == Overrides::default()
    }

    /// `s` with these overrides for a loop of `kind`, or why one does not parse (the profile
    /// is checked by [`validate`], not here).
    pub fn apply(&self, s: &LoopSettings, kind: LoopKind) -> Result<LoopSettings, String> {
        let mut out = s.clone();
        if let Some(p) = &self.profile {
            super::config::check_profile_name(p)?;
            out.profile = p.clone();
        }
        if let Some(i) = &self.implementer {
            out.implementer = parse_implementer(i).map_err(|e| format!("implementer: {e}"))?;
        }
        if let Some(w) = &self.workers {
            out.workers = WorkersChoice::parse(w).map_err(|e| format!("workers: {e}"))?;
        }
        if let Some(r) = &self.reviewer {
            out.reviewer = ReviewerChoice::parse(r).map_err(|e| format!("reviewer: {e}"))?;
        }
        if let Some(c) = &self.commit {
            match kind {
                LoopKind::Implement => out.commit = ImplCommit::parse(c)?,
                LoopKind::Review => out.review_commit = ReviewCommit::parse(c)?,
            }
        }
        Ok(out)
    }
}

/// Most commits `last <n>` may go back.
pub const MAX_LAST: u32 = 1000;

/// What a review loop reviews: the base its diffs start from, plus the working tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Scope {
    /// From the merge-base of the thread's branch and its base ref.
    Branch,
    /// From `HEAD`.
    Uncommitted,
    /// From a ref.
    Since(String),
    /// From `HEAD~n`.
    Last(u32),
}

impl Scope {
    /// The scope a review loop gets when the request names none: `branch` when the thread has
    /// commits beyond its base, else `uncommitted`.
    pub fn default_for(has_own_commits: bool) -> Scope {
        if has_own_commits {
            Scope::Branch
        } else {
            Scope::Uncommitted
        }
    }
}

impl fmt::Display for Scope {
    /// `branch`, `uncommitted`, `since <ref>`, `last <n>`: what [`Scope::from_str`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Branch => f.write_str("branch"),
            Scope::Uncommitted => f.write_str("uncommitted"),
            Scope::Since(r) => write!(f, "since {r}"),
            Scope::Last(n) => write!(f, "last {n}"),
        }
    }
}

/// Checks a ref as `since <ref>` takes it: a name git can't read as an option or a range.
fn check_ref(r: &str) -> Result<(), String> {
    if r.is_empty()
        || r.len() > 256
        || r.starts_with('-')
        || r.contains("..")
        || r.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("{r:?} is not a ref"));
    }
    Ok(())
}

impl FromStr for Scope {
    type Err = String;

    /// `branch`, `uncommitted`, `since <ref>` (or `since=<ref>`), `last <n>` (1 to
    /// [`MAX_LAST`]).
    fn from_str(s: &str) -> Result<Scope, String> {
        let s = s.trim();
        let (word, rest) = match s.split_once([' ', '=']) {
            Some((w, r)) => (w, Some(r.trim())),
            None => (s, None),
        };
        match (word, rest) {
            ("branch", None) => Ok(Scope::Branch),
            ("uncommitted", None) => Ok(Scope::Uncommitted),
            ("since", Some(r)) => {
                check_ref(r)?;
                Ok(Scope::Since(r.to_string()))
            }
            ("last", Some(n)) => match n.parse::<u32>() {
                Ok(n) if (1..=MAX_LAST).contains(&n) => Ok(Scope::Last(n)),
                _ => Err(format!(
                    "last {n}: give a number of commits, 1 to {MAX_LAST}"
                )),
            },
            _ => Err(format!(
                "{s:?} is not a scope (branch, uncommitted, since <ref>, last <n>)"
            )),
        }
    }
}

impl Serialize for Scope {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Scope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Scope, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------------------------
// The saved settings, folded
// ---------------------------------------------------------------------------------------------

/// One saved set.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    /// The `loop.settings_saved` id (what `loop.started` names as `settings_from`).
    pub id: String,
    /// The settings.
    pub settings: LoopSettings,
}

/// Whose settings a loop runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsOf {
    /// The project's own.
    Project,
    /// The workspace's.
    Workspace,
}

/// The saved settings, folded from `loop.settings_saved` and `loop.settings_reset` on the
/// hub's own thread: the workspace's and each project's own.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSettings {
    /// The workspace's: every project without settings of its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<Saved>,
    /// Projects' own, by project name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, Saved>,
}

impl SavedSettings {
    /// Folds `ev`: a save replaces the set it is for; a reset with a project forgets that
    /// project's own, and one without forgets every saved set (the next loop shows the
    /// first-run card). Events on other threads are not settings.
    pub fn apply(&mut self, ev: &Event) {
        if ev.thread != HUB_THREAD {
            return;
        }
        match &ev.body {
            EventBody::LoopSettingsSaved(p) => {
                let saved = Saved {
                    id: p.id.clone(),
                    settings: p.settings.clone(),
                };
                match &p.project {
                    Some(project) => {
                        self.projects.insert(project.clone(), saved);
                    }
                    None => self.workspace = Some(saved),
                }
            }
            EventBody::LoopSettingsReset(p) => match &p.project {
                Some(project) => {
                    self.projects.remove(project);
                }
                None => *self = SavedSettings::default(),
            },
            _ => {}
        }
    }

    /// The settings a loop in project `project` runs on: the project's own if it has them,
    /// else the workspace's; `None` before the first save.
    pub fn for_project(&self, project: Option<&str>) -> Option<(&Saved, SettingsOf)> {
        project
            .and_then(|p| self.projects.get(p))
            .map(|s| (s, SettingsOf::Project))
            .or_else(|| self.workspace.as_ref().map(|s| (s, SettingsOf::Workspace)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{LoopSettingsReset, LoopSettingsSaved, Source};

    fn rt(name: &str, models: &[&str]) -> RuntimeAvailability {
        RuntimeAvailability {
            runtime: name.into(),
            found: true,
            login: true,
            models: models.iter().map(|s| s.to_string()).collect(),
            efforts: ["low", "medium", "high", "xhigh"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            accounts: vec![],
            subagents: name == "claude-code",
            ..Default::default()
        }
    }

    fn all_found() -> Availability {
        Availability {
            runtimes: vec![
                rt("claude-code", &["claude-opus-5-5", "claude-sonnet-5-5"]),
                rt("codex", &["gpt-6.1-sol", "gpt-6-astra"]),
                rt("grok-build", &["grok-4.7"]),
            ],
            thread: Some(ThreadModel {
                runtime: "claude-code".into(),
                model: Some("claude-opus-5-5".into()),
                effort: Some("high".into()),
            }),
        }
    }

    fn first() -> LoopSettings {
        LoopSettings::first_card(&LoopsConfig::default())
    }

    #[test]
    fn the_first_card_and_the_presets() {
        let s = first();
        assert_eq!(s.profile, "high");
        assert_eq!(s.implementer, None);
        assert_eq!(s.reviewer.to_string(), "codex/gpt-6.1-sol");
        assert_eq!(s.workers.to_string(), "claude-opus-5-5:high");
        assert_eq!(s.commit, ImplCommit::End);
        assert_eq!(s.review_commit, ReviewCommit::Match);
        let p = presets();
        let ids: Vec<&str> = p.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-code+codex", "claude-code", "codex", "grok-build"]
        );
        assert_eq!(
            p[0].implementer.to_string(),
            "claude-code/claude-opus-5-5:high"
        );
        assert_eq!(p[0].reviewer.to_string(), "codex/gpt-6.1-sol");
        assert_eq!(p[3].reviewer.to_string(), "grok-build/grok-4.7");
        let applied = s.with_preset(&p[2]);
        assert_eq!(applied.profile, "high");
        assert_eq!(applied.implementer.unwrap().runtime, "codex");
        // Every preset's implementer and reviewer are of one family when the runtime is.
        assert!(!same_family(&s.with_preset(&p[0]), &all_found()));
        assert!(same_family(&s.with_preset(&p[1]), &all_found()));
        assert!(same_family(&s.with_preset(&p[3]), &all_found()));
    }

    #[test]
    fn families() {
        for (m, f) in [
            ("claude-opus-5-5", Some("claude")),
            ("opus", Some("claude")),
            ("anthropic.claude-sonnet-5-5-v1:0", Some("claude")),
            ("gpt-6.1-sol", Some("gpt")),
            ("codex-mini", Some("gpt")),
            ("grok-4.7-build-fast", Some("grok")),
            ("gemini-3", None),
        ] {
            assert_eq!(model_family(m), f, "{m}");
        }
    }

    #[test]
    fn settings_that_hold_have_no_problems() {
        let c = LoopsConfig::default();
        assert_eq!(validate(&first(), &all_found(), &c), vec![]);
        let p = propose(&first(), &all_found(), &c);
        assert_eq!(p.settings, first());
        assert_eq!(p.warnings, Vec::<String>::new());
        assert!(p.problems.is_empty());
    }

    #[test]
    fn each_problem_is_found() {
        let c = LoopsConfig::default();
        let kinds = |s: &LoopSettings, a: &Availability| -> Vec<(Role, ProblemKind)> {
            validate(s, a, &c)
                .iter()
                .map(|p| (p.role, p.kind))
                .collect()
        };
        let mut a = all_found();
        a.runtimes.retain(|r| r.runtime != "codex");
        assert_eq!(
            kinds(&first(), &a),
            [(Role::Reviewer, ProblemKind::NotInstalled)]
        );
        let mut a = all_found();
        a.runtimes[1].login = false;
        assert_eq!(
            kinds(&first(), &a),
            [(Role::Reviewer, ProblemKind::LoggedOut)]
        );
        let mut a = all_found();
        a.runtimes[1].models = vec!["gpt-7".into()];
        assert_eq!(
            kinds(&first(), &a),
            [(Role::Reviewer, ProblemKind::UnknownModel)]
        );
        let mut a = all_found();
        a.runtimes[1].efforts = vec!["low".into(), "medium".into()];
        let v = validate(&first(), &a, &c);
        assert_eq!(v[0].kind, ProblemKind::UnknownEffort);
        assert_eq!(
            v[0].message,
            "codex takes no effort xhigh (low, medium): profile high asks for it"
        );
        let mut s = first();
        s.profile = "gone".into();
        assert_eq!(
            kinds(&s, &all_found()),
            [(Role::Profile, ProblemKind::UnknownProfile)]
        );
        let mut s = first();
        s.reviewer.account = Some("team".into());
        let mut a = all_found();
        a.runtimes[1].accounts = vec!["default".into()];
        assert_eq!(
            kinds(&s, &a),
            [(Role::Reviewer, ProblemKind::UnknownAccount)]
        );
        let mut s = first();
        s.implementer = Some(ModelChoice::parse("codex/gpt-6.1-sol:high").unwrap());
        assert_eq!(
            kinds(&s, &all_found()),
            [(Role::Implementer, ProblemKind::OtherRuntime)]
        );
        let mut s = first();
        s.workers = WorkersChoice::parse("claude-haiku-9:high").unwrap();
        assert_eq!(
            kinds(&s, &all_found()),
            [(Role::Workers, ProblemKind::UnknownModel)]
        );
        // Workers are not checked on a runtime without subagents.
        let mut a = all_found();
        a.thread.as_mut().unwrap().runtime = "codex".into();
        assert_eq!(kinds(&s, &a), []);
        let mut s = first();
        s.reviewer = ReviewerChoice {
            runtime: "gemini".into(),
            account: None,
            model: "g".into(),
        };
        assert_eq!(
            kinds(&s, &all_found()),
            [(Role::Reviewer, ProblemKind::NotAReviewer)]
        );
    }

    #[test]
    fn missing_runtimes_are_replaced_claude_code_first() {
        let c = LoopsConfig::default();
        let mut a = all_found();
        a.runtimes.retain(|r| r.runtime != "codex");
        let p = propose(&first(), &a, &c);
        assert_eq!(
            p.settings.reviewer.to_string(),
            "claude-code/claude-opus-5-5"
        );
        assert_eq!(
            p.warnings,
            [
                "codex isn't installed: reviewing with claude-code · claude-opus-5-5 instead",
                SAME_FAMILY_WARNING
            ]
        );
        assert!(p.problems.is_empty());
        // Claude Code missing too: Grok.
        a.runtimes.retain(|r| r.runtime != "claude-code");
        a.thread = None;
        let p = propose(&first(), &a, &c);
        assert_eq!(p.settings.reviewer.to_string(), "grok-build/grok-4.7");
        // Nothing to review with: the problem stays.
        let none = Availability::default();
        let p = propose(&first(), &none, &c);
        assert_eq!(p.settings.reviewer, first().reviewer);
        assert_eq!(p.problems[0].kind, ProblemKind::NotInstalled);
        // A logged-out implementer on a new thread.
        let mut s = first();
        s.implementer = Some(ModelChoice::parse("claude-code/claude-opus-5-5:high").unwrap());
        let mut a = all_found();
        a.thread = None;
        a.runtimes[0].login = false;
        let p = propose(&s, &a, &c);
        assert_eq!(
            p.settings.implementer.as_ref().unwrap().to_string(),
            "codex/gpt-6.1-sol:high"
        );
        assert_eq!(
            p.warnings[0],
            "claude-code isn't logged in: implementing with codex · gpt-6.1-sol instead"
        );
        assert_eq!(p.warnings[1], SAME_FAMILY_WARNING);
        // A named implementer on another runtime than the thread's: the thread's own.
        let mut s = first();
        s.implementer = Some(ModelChoice::parse("codex/gpt-6.1-sol:high").unwrap());
        let p = propose(&s, &all_found(), &c);
        assert_eq!(p.settings.implementer, None);
        assert_eq!(
            p.warnings,
            ["the thread runs on claude-code, not codex: implementing with the thread's own model"]
        );
        // A model the runtime no longer offers, and a profile that is gone.
        let mut s = first();
        s.profile = "gone".into();
        let mut a = all_found();
        a.runtimes[1].models = vec!["gpt-7".into()];
        let p = propose(&s, &a, &c);
        assert_eq!(p.settings.profile, "high");
        assert_eq!(p.settings.reviewer.model, "gpt-7");
        assert_eq!(
            p.warnings,
            [
                "there is no profile \"gone\" any more: high instead",
                "codex no longer offers gpt-6.1-sol: reviewing with codex · gpt-7 instead"
            ]
        );
    }

    #[test]
    fn overrides_apply_per_kind() {
        let s = first();
        let o = Overrides {
            profile: Some("low".into()),
            reviewer: Some("claude-code/claude-opus-5-5".into()),
            implementer: Some("thread".into()),
            workers: Some("claude-sonnet-5-5".into()),
            commit: Some("none".into()),
        };
        let r = o.apply(&s, LoopKind::Review).unwrap();
        assert_eq!(r.profile, "low");
        assert_eq!(r.review_commit, ReviewCommit::None);
        assert_eq!(r.commit, ImplCommit::End);
        let i = o.apply(&s, LoopKind::Implement).unwrap();
        assert_eq!(i.commit, ImplCommit::None);
        assert!(Overrides::default().is_empty());
        let bad = Overrides {
            commit: Some("per-round".into()),
            ..Default::default()
        };
        assert!(bad
            .apply(&s, LoopKind::Review)
            .unwrap_err()
            .contains("match or none"));
        assert!(bad.apply(&s, LoopKind::Implement).is_ok());
        let bad = Overrides {
            reviewer: Some("x".into()),
            ..Default::default()
        };
        assert!(bad
            .apply(&s, LoopKind::Review)
            .unwrap_err()
            .starts_with("reviewer: "));
    }

    #[test]
    fn scopes_parse_print_and_default() {
        for (text, scope) in [
            ("branch", Scope::Branch),
            ("uncommitted", Scope::Uncommitted),
            ("since main", Scope::Since("main".into())),
            ("since=origin/dev", Scope::Since("origin/dev".into())),
            ("last 3", Scope::Last(3)),
        ] {
            assert_eq!(text.parse::<Scope>().unwrap(), scope, "{text}");
        }
        assert_eq!(Scope::Since("v1.2".into()).to_string(), "since v1.2");
        for bad in [
            "",
            "all",
            "since",
            "since -x",
            "since a..b",
            "last 0",
            "last x",
            "branch x",
        ] {
            assert!(bad.parse::<Scope>().is_err(), "{bad}");
        }
        assert_eq!(Scope::default_for(true), Scope::Branch);
        assert_eq!(Scope::default_for(false), Scope::Uncommitted);
        let j = serde_json::to_value(Scope::Last(2)).unwrap();
        assert_eq!(j, "last 2");
        assert_eq!(serde_json::from_value::<Scope>(j).unwrap(), Scope::Last(2));
    }

    #[test]
    fn saved_settings_fold() {
        let ev = |thread: &str, body| Event::new(thread, 1, None, Source::Hub, body);
        let mut f = SavedSettings::default();
        assert_eq!(f.for_project(Some("api")), None);
        let save = |id: &str, project: Option<&str>, profile: &str| {
            EventBody::LoopSettingsSaved(Box::new(LoopSettingsSaved {
                id: id.into(),
                project: project.map(str::to_string),
                settings: LoopSettings {
                    profile: profile.into(),
                    ..first()
                },
                ..Default::default()
            }))
        };
        f.apply(&ev(HUB_THREAD, save("s1", None, "high")));
        f.apply(&ev(HUB_THREAD, save("s2", Some("api"), "low")));
        // Not on the hub's thread: not settings.
        f.apply(&ev("t1", save("s3", None, "medium")));
        let (s, of) = f.for_project(Some("api")).unwrap();
        assert_eq!((s.id.as_str(), of), ("s2", SettingsOf::Project));
        let (s, of) = f.for_project(Some("web")).unwrap();
        assert_eq!((s.id.as_str(), of), ("s1", SettingsOf::Workspace));
        f.apply(&ev(
            HUB_THREAD,
            EventBody::LoopSettingsReset(LoopSettingsReset {
                project: Some("api".into()),
                ..Default::default()
            }),
        ));
        assert_eq!(f.for_project(Some("api")).unwrap().0.id, "s1");
        f.apply(&ev(
            HUB_THREAD,
            EventBody::LoopSettingsReset(LoopSettingsReset::default()),
        ));
        assert_eq!(f, SavedSettings::default());
    }
}

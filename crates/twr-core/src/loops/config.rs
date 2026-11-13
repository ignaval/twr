//! `[loops]` in the config file: what the first setup card shows, the profiles and the limits
//! (docs/design/loops.md, "Config"). It is never the saved settings and never starts a loop.
//!
//! The model keys are written as strings and checked by [`LoopsConfig::validate`] (which
//! [`crate::config::Config::validate`] calls, so its errors read `loops.<key>: …`); the typed
//! values come from [`LoopsConfig::implementer`], [`LoopsConfig::reviewer`] and
//! [`LoopsConfig::workers`], which parse with [`ModelChoice::parse`], [`ReviewerChoice::parse`]
//! and [`WorkersChoice::parse`].

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::account::{check_name, split_runtime};
use crate::config::parse_span;

/// The built-in runtimes a review run can use (Codex, Claude Code, Grok Build): those whose
/// CLI has a one-shot, read-only, schema-constrained mode. Besides them, an ACP preset whose
/// `[runtimes.<name>.review]` names its agent's read-only mode can review
/// ([`crate::config::PresetReview`], [`crate::loops::settings::RuntimeAvailability::reviews`]).
pub const REVIEWER_RUNTIMES: &[&str] = &["claude-code", "codex", "grok-build"];

/// The built-in profiles, from the skills' table. A `[loops.profiles.<name>]` of the same name
/// overrides the keys it gives ([`LoopsConfig::profile`]).
pub const BUILTIN_PROFILES: &[&str] = &["low", "medium", "high"];

/// Longest model id a loop key takes, in bytes.
pub const MAX_MODEL_LEN: usize = 128;
/// Longest effort name, in bytes.
pub const MAX_EFFORT_LEN: usize = 16;
/// Longest profile name, in bytes.
pub const MAX_PROFILE_NAME_LEN: usize = 32;
/// Most tiers a profile's ladder may have.
pub const MAX_TIERS: usize = 6;
/// Most rounds a tier, `max_rounds_per_tier` or a profile's verification may run.
pub const MAX_ROUNDS: u32 = 100;
/// Most audit runs `max_audit_runs` may run at once.
pub const MAX_AUDIT_RUNS: u32 = 32;
/// The shortest `round_timeout`.
pub const MIN_ROUND_TIMEOUT: Duration = Duration::from_secs(60);
/// `round_timeout` when the configured one does not parse ([`LoopsConfig::round_timeout`]).
pub const DEFAULT_ROUND_TIMEOUT: Duration = Duration::from_secs(3600);

/// `[loops]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopsConfig {
    /// The profile the first card shows: a built-in one ([`BUILTIN_PROFILES`]) or a
    /// `[loops.profiles.<name>]`.
    pub profile: String,
    /// The implementer the first card shows: `thread` (the thread's own model) or
    /// `runtime[:account]/model[:effort]` ([`LoopsConfig::implementer`]).
    pub implementer: String,
    /// The reviewer the first card shows, `runtime[:account]/model` ([`LoopsConfig::reviewer`]).
    pub reviewer: String,
    /// The implementer's subagents, `model[:effort]`, where its runtime has them
    /// ([`LoopsConfig::workers`]).
    pub workers: String,
    /// What an implementation loop commits, on the first card ([`ImplCommit`]).
    pub commit: ImplCommit,
    /// Who audits the sweep's rules ([`SweepMode`]).
    pub sweep: SweepMode,
    /// With `sweep = "runs"`: the most audit runs at once.
    pub max_audit_runs: u32,
    /// How long one review run may take, as a span (`1h`, the default;
    /// [`LoopsConfig::round_timeout`]): one that takes longer is a failed run.
    #[serde(rename = "round_timeout")]
    pub round_timeout_span: String,
    /// The backstop: a ladder tier this many rounds long ends the ladder (25).
    pub max_rounds_per_tier: u32,
    /// The apps' **Review loop** button on a finished turn with changes.
    pub offer_review: bool,
    /// Profiles by name: overrides of the built-in ones and profiles of your own.
    pub profiles: BTreeMap<String, ProfileConfig>,
}

impl Default for LoopsConfig {
    fn default() -> Self {
        LoopsConfig {
            profile: "high".into(),
            implementer: "thread".into(),
            reviewer: "codex/gpt-6.1-sol".into(),
            workers: "claude-opus-5-5:high".into(),
            commit: ImplCommit::End,
            sweep: SweepMode::Thread,
            max_audit_runs: 4,
            round_timeout_span: "1h".into(),
            max_rounds_per_tier: 25,
            offer_review: true,
            profiles: BTreeMap::new(),
        }
    }
}

impl LoopsConfig {
    /// `round_timeout` as a duration ([`DEFAULT_ROUND_TIMEOUT`] if it does not parse, which
    /// [`LoopsConfig::validate`] refuses).
    pub fn round_timeout(&self) -> Duration {
        parse_span(&self.round_timeout_span).unwrap_or(DEFAULT_ROUND_TIMEOUT)
    }

    /// `implementer`, parsed.
    pub fn implementer(&self) -> Result<Option<ModelChoice>, String> {
        parse_implementer(&self.implementer)
    }

    /// `reviewer`, parsed.
    pub fn reviewer(&self) -> Result<ReviewerChoice, String> {
        ReviewerChoice::parse(&self.reviewer)
    }

    /// `workers`, parsed.
    pub fn workers(&self) -> Result<WorkersChoice, String> {
        WorkersChoice::parse(&self.workers)
    }

    /// The names of every profile: the built-in ones, then those of the config file, sorted.
    pub fn profile_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_PROFILES.iter().map(|s| s.to_string()).collect();
        for n in self.profiles.keys() {
            if !names.contains(n) {
                names.push(n.clone());
            }
        }
        names
    }

    /// Profile `name`, resolved: a built-in one with the keys a table of its name gives in
    /// place of its own, or a profile of the config file (its `plan_check` defaults to
    /// `contracts` and its `sweep` to `true`, the skills' defaults; `tiers` and `verify` it must
    /// give, [`LoopsConfig::validate`]). `None`: no such profile, or one missing a key.
    pub fn profile(&self, name: &str) -> Option<Profile> {
        let base = Profile::builtin(name);
        let Some(table) = self.profiles.get(name) else {
            return base;
        };
        let (tiers, verify) = match &base {
            Some(b) => (
                table
                    .tiers
                    .as_ref()
                    .map(|t| t.iter().map(TierConfig::tier).collect())
                    .unwrap_or_else(|| b.tiers.clone()),
                table
                    .verify
                    .as_ref()
                    .map(VerifyConfig::verify)
                    .unwrap_or_else(|| b.verify.clone()),
            ),
            None => (
                table.tiers.as_ref()?.iter().map(TierConfig::tier).collect(),
                table.verify.as_ref()?.verify(),
            ),
        };
        Some(Profile {
            name: name.to_string(),
            plan_check: table
                .plan_check
                .or(base.as_ref().map(|b| b.plan_check))
                .unwrap_or(PlanCheck::Contracts),
            tiers,
            sweep: table
                .sweep
                .or(base.as_ref().map(|b| b.sweep))
                .unwrap_or(true),
            verify,
        })
    }

    /// Checks every key; errors read `loops.<key>: …`.
    pub fn validate(&self) -> anyhow::Result<()> {
        let err = |key: &str, why: String| anyhow::anyhow!("loops.{key}: {why}");
        if self.profile(&self.profile).is_none() && !self.profiles.contains_key(&self.profile) {
            return Err(err(
                "profile",
                format!(
                    "{:?} is not a profile ({})",
                    self.profile,
                    self.profile_names().join(", ")
                ),
            ));
        }
        self.implementer().map_err(|e| err("implementer", e))?;
        self.reviewer().map_err(|e| err("reviewer", e))?;
        self.workers().map_err(|e| err("workers", e))?;
        if !(1..=MAX_AUDIT_RUNS).contains(&self.max_audit_runs) {
            return Err(err(
                "max_audit_runs",
                format!("must be 1 to {MAX_AUDIT_RUNS}"),
            ));
        }
        match parse_span(&self.round_timeout_span) {
            Some(d) if d >= MIN_ROUND_TIMEOUT => {}
            _ => {
                return Err(err(
                    "round_timeout",
                    format!(
                        "{:?} is not a span of time of a minute or more (30m, 1h)",
                        self.round_timeout_span
                    ),
                ))
            }
        }
        if !(1..=MAX_ROUNDS).contains(&self.max_rounds_per_tier) {
            return Err(err(
                "max_rounds_per_tier",
                format!("must be 1 to {MAX_ROUNDS}"),
            ));
        }
        for (name, p) in &self.profiles {
            let key = format!("profiles.{name}");
            check_profile_name(name).map_err(|e| err(&key, e))?;
            let builtin = BUILTIN_PROFILES.contains(&name.as_str());
            if !builtin && p.tiers.is_none() {
                return Err(err(
                    &key,
                    "a profile of your own gives `tiers` (and `verify`)".into(),
                ));
            }
            if !builtin && p.verify.is_none() {
                return Err(err(&key, "a profile of your own gives `verify`".into()));
            }
            if let Some(tiers) = &p.tiers {
                if tiers.is_empty() || tiers.len() > MAX_TIERS {
                    return Err(err(
                        &format!("{key}.tiers"),
                        format!("a ladder has 1 to {MAX_TIERS} tiers"),
                    ));
                }
                for t in tiers {
                    let t = t.tier();
                    check_effort(&t.effort).map_err(|e| err(&format!("{key}.tiers"), e))?;
                    if let Some(n) = t.max_rounds {
                        if !(1..=MAX_ROUNDS).contains(&n) {
                            return Err(err(
                                &format!("{key}.tiers"),
                                format!("max_rounds must be 1 to {MAX_ROUNDS}"),
                            ));
                        }
                    }
                }
            }
            if let Some(v) = &p.verify {
                if !(1..=MAX_ROUNDS).contains(&v.rounds) {
                    return Err(err(
                        &format!("{key}.verify"),
                        format!("rounds must be 1 to {MAX_ROUNDS}"),
                    ));
                }
                check_effort(&v.effort).map_err(|e| err(&format!("{key}.verify"), e))?;
            }
        }
        Ok(())
    }
}

/// Checks a profile name: 1 to [`MAX_PROFILE_NAME_LEN`] lowercase letters, digits, `-` or
/// `_`, the first a letter.
pub fn check_profile_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_PROFILE_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not a profile name (lowercase letters, digits, - and _, at most \
             {MAX_PROFILE_NAME_LEN})"
        ))
    }
}

/// Checks an effort name: 1 to [`MAX_EFFORT_LEN`] lowercase letters (`medium`, `xhigh`). Which
/// efforts a runtime takes is checked against what it reports
/// ([`crate::loops::settings::validate`]).
pub fn check_effort(effort: &str) -> Result<(), String> {
    if !effort.is_empty()
        && effort.len() <= MAX_EFFORT_LEN
        && effort.chars().all(|c| c.is_ascii_lowercase())
    {
        Ok(())
    } else {
        Err(format!(
            "{effort:?} is not an effort (lowercase letters: low, medium, high, xhigh…)"
        ))
    }
}

/// Checks a model id: 1 to [`MAX_MODEL_LEN`] bytes, no whitespace or control characters.
pub fn check_model(model: &str) -> Result<(), String> {
    if model.is_empty() {
        return Err("the model is empty".into());
    }
    if model.len() > MAX_MODEL_LEN {
        return Err(format!("the model is longer than {MAX_MODEL_LEN} bytes"));
    }
    if model.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "{model:?} is not a model id (it has a space or a control character)"
        ));
    }
    Ok(())
}

/// Checks a runtime name as a loop key gives it: a plain name (letters, digits, `-`, `_`,
/// `.`), not a command line (`acp:…`, `pty:…`).
fn check_runtime(runtime: &str) -> Result<(), String> {
    let plain = !runtime.is_empty()
        && runtime.len() <= 64
        && runtime
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain || runtime == "acp" || runtime == "pty" {
        return Err(format!(
            "{runtime:?} is not a runtime name (claude-code, codex, grok-build or a [runtimes] name)"
        ));
    }
    Ok(())
}

/// `runtime[:account]`, checked.
fn parse_runtime_account(s: &str) -> Result<(String, Option<String>), String> {
    let (runtime, account) = split_runtime(s);
    check_runtime(runtime)?;
    if let Some(a) = account {
        check_name(a).map_err(|e| format!("account {a:?}: {e}"))?;
    }
    Ok((runtime.to_string(), account.map(str::to_string)))
}

/// `model[:effort]`: the effort is what follows the last `:` when that is a word of lowercase
/// letters (a model id such as `vendor.model-v1:0` keeps its `:0`).
fn split_effort(s: &str) -> (&str, Option<&str>) {
    match s.rsplit_once(':') {
        Some((model, effort))
            if !effort.is_empty() && effort.chars().all(|c| c.is_ascii_lowercase()) =>
        {
            (model, Some(effort))
        }
        _ => (s, None),
    }
}

/// A runtime, an optional account, a model and an optional effort: a named implementer
/// (`runtime[:account]/model[:effort]`, `claude-code:work/claude-opus-5-5:high`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelChoice {
    /// The runtime (`claude-code`, `codex`, `grok-build`).
    pub runtime: String,
    /// The account of the runtime; `None`: the account the thread or the policy gives.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The model id, as the runtime takes it.
    pub model: String,
    /// The reasoning effort; `None`: the runtime's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl ModelChoice {
    /// Parses `runtime[:account]/model[:effort]`.
    pub fn parse(s: &str) -> Result<ModelChoice, String> {
        let s = s.trim();
        let Some((left, right)) = s.split_once('/') else {
            return Err(format!(
                "{s:?} is not runtime[:account]/model[:effort] (claude-code/claude-opus-5-5:high)"
            ));
        };
        let (runtime, account) = parse_runtime_account(left)?;
        let (model, effort) = split_effort(right);
        check_model(model)?;
        if let Some(e) = effort {
            check_effort(e)?;
        }
        Ok(ModelChoice {
            runtime,
            account,
            model: model.to_string(),
            effort: effort.map(str::to_string),
        })
    }
}

impl fmt::Display for ModelChoice {
    /// `runtime[:account]/model[:effort]`, what [`ModelChoice::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.runtime)?;
        if let Some(a) = &self.account {
            write!(f, ":{a}")?;
        }
        write!(f, "/{}", self.model)?;
        if let Some(e) = &self.effort {
            write!(f, ":{e}")?;
        }
        Ok(())
    }
}

/// Parses an implementer: `thread` (`None`: the thread's own model) or
/// `runtime[:account]/model[:effort]`.
pub fn parse_implementer(s: &str) -> Result<Option<ModelChoice>, String> {
    if s.trim() == "thread" {
        return Ok(None);
    }
    ModelChoice::parse(s)
        .map(Some)
        .map_err(|e| format!("{e}, or \"thread\""))
}

/// The reviewer: a runtime, an optional account and a model (`runtime[:account]/model`). Its
/// effort is each round's, from the profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewerChoice {
    /// The runtime: one of [`REVIEWER_RUNTIMES`], or an ACP preset's name.
    pub runtime: String,
    /// The account of the runtime; `None`: the runtime's default account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The model id.
    pub model: String,
}

impl ReviewerChoice {
    /// Parses `runtime[:account]/model`. The runtime is a plain runtime name: whether it can
    /// review (one of [`REVIEWER_RUNTIMES`], or an ACP preset that names a read-only mode) is
    /// what the config ([`crate::config::Config::validate`]) and the settings
    /// ([`crate::loops::settings::validate`]) check, since only they know the presets.
    pub fn parse(s: &str) -> Result<ReviewerChoice, String> {
        let s = s.trim();
        let Some((left, model)) = s.split_once('/') else {
            return Err(format!(
                "{s:?} is not runtime[:account]/model (codex/gpt-6.1-sol)"
            ));
        };
        let (runtime, account) = parse_runtime_account(left)?;
        if split_effort(model).1.is_some() {
            return Err(format!(
                "{s:?} gives an effort: a reviewer's effort is each round's, from the profile"
            ));
        }
        check_model(model)?;
        Ok(ReviewerChoice {
            runtime,
            account,
            model: model.to_string(),
        })
    }
}

impl fmt::Display for ReviewerChoice {
    /// `runtime[:account]/model`, what [`ReviewerChoice::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.runtime)?;
        if let Some(a) = &self.account {
            write!(f, ":{a}")?;
        }
        write!(f, "/{}", self.model)
    }
}

/// The implementer's subagents: a model and an optional effort (`model[:effort]`), on the
/// implementer's runtime.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkersChoice {
    /// The model id.
    pub model: String,
    /// The reasoning effort; `None`: the runtime's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

impl WorkersChoice {
    /// Parses `model[:effort]`.
    pub fn parse(s: &str) -> Result<WorkersChoice, String> {
        let s = s.trim();
        if s.contains('/') {
            return Err(format!(
                "{s:?} is not model[:effort] (claude-opus-5-5:high): workers run on the \
                 implementer's runtime"
            ));
        }
        let (model, effort) = split_effort(s);
        check_model(model)?;
        if let Some(e) = effort {
            check_effort(e)?;
        }
        Ok(WorkersChoice {
            model: model.to_string(),
            effort: effort.map(str::to_string),
        })
    }
}

impl fmt::Display for WorkersChoice {
    /// `model[:effort]`, what [`WorkersChoice::parse`] reads.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.model)?;
        if let Some(e) = &self.effort {
            write!(f, ":{e}")?;
        }
        Ok(())
    }
}

/// When an implementation loop asks for a plan check (one review run on the plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCheck {
    /// Never.
    Never,
    /// When the plan says `contract-risk: yes`.
    #[default]
    Contracts,
    /// Always.
    Always,
}

impl PlanCheck {
    /// As written in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            PlanCheck::Never => "never",
            PlanCheck::Contracts => "contracts",
            PlanCheck::Always => "always",
        }
    }

    /// Parses `never`, `contracts` or `always`.
    pub fn parse(s: &str) -> Result<PlanCheck, String> {
        match s.trim() {
            "never" => Ok(PlanCheck::Never),
            "contracts" => Ok(PlanCheck::Contracts),
            "always" => Ok(PlanCheck::Always),
            other => Err(format!(
                "{other:?} is not a plan check (contracts, always or never)"
            )),
        }
    }

    /// Whether the plan check runs on a plan that said `contract_risk`: always with `always`,
    /// never with `never`, and with `contracts` when the plan touches a contract.
    pub fn runs(self, contract_risk: bool) -> bool {
        match self {
            PlanCheck::Never => false,
            PlanCheck::Contracts => contract_risk,
            PlanCheck::Always => true,
        }
    }
}

/// What an implementation loop commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImplCommit {
    /// One commit of the loop's work at the end (not after `stopped` or `failed`).
    #[default]
    End,
    /// A commit per round.
    PerRound,
    /// No commit.
    None,
}

impl ImplCommit {
    /// As written in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            ImplCommit::End => "end",
            ImplCommit::PerRound => "per-round",
            ImplCommit::None => "none",
        }
    }

    /// Parses `end`, `per-round` or `none`.
    pub fn parse(s: &str) -> Result<ImplCommit, String> {
        match s.trim() {
            "end" => Ok(ImplCommit::End),
            "per-round" => Ok(ImplCommit::PerRound),
            "none" => Ok(ImplCommit::None),
            other => Err(format!(
                "{other:?} is not an implementation loop's commit (end, per-round or none)"
            )),
        }
    }
}

/// What a review loop commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewCommit {
    /// Follow the thread: commit each round's fixes if its work was being committed.
    #[default]
    Match,
    /// Leave fixes uncommitted.
    None,
}

impl ReviewCommit {
    /// As written.
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewCommit::Match => "match",
            ReviewCommit::None => "none",
        }
    }

    /// Parses `match` or `none`.
    pub fn parse(s: &str) -> Result<ReviewCommit, String> {
        match s.trim() {
            "match" => Ok(ReviewCommit::Match),
            "none" => Ok(ReviewCommit::None),
            other => Err(format!(
                "{other:?} is not a review loop's commit (match or none)"
            )),
        }
    }

    /// Whether a review loop commits its fixes, as the hub decides it when the loop starts
    /// (`loop.started.commit_fixes`): under `match`, when the thread had been committing its
    /// work, which is when the scope has commits of the thread's own and the worktree had no
    /// uncommitted changes; under `none`, never. The report turn then asks for one commit of
    /// the fixes.
    pub fn commits_fixes(self, own_commits: bool, clean: bool) -> bool {
        match self {
            ReviewCommit::Match => own_commits && clean,
            ReviewCommit::None => false,
        }
    }
}

/// Who audits the sweep's rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepMode {
    /// The thread, with its own read-only subagents where its runtime has them.
    #[default]
    Thread,
    /// The hub, one read-only audit run per rule on the reviewer's model, up to
    /// `max_audit_runs` at once; the thread verifies their reports (`twr_core::loops`, "The
    /// sweep's audit runs").
    Runs,
}

/// `[loops.profiles.<name>]`. Every key is optional: a table named after a built-in profile
/// overrides only the keys it gives ([`LoopsConfig::profile`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileConfig {
    /// When an implementation loop checks its plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_check: Option<PlanCheck>,
    /// The ladder's tiers, in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<TierConfig>>,
    /// Whether the sweep runs (when the ladder found something valid).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sweep: Option<bool>,
    /// The capped verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifyConfig>,
}

/// One tier of `tiers`: an effort (`"medium"`) or `{effort, max_rounds}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TierConfig {
    /// An effort alone.
    Effort(String),
    /// An effort and the most rounds the tier runs.
    Full {
        /// The reviewer's effort in this tier.
        effort: String,
        /// The most rounds; `None`: until it converges (or the backstop).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_rounds: Option<u32>,
    },
}

impl TierConfig {
    /// The tier it describes.
    pub fn tier(&self) -> Tier {
        match self {
            TierConfig::Effort(e) => Tier {
                effort: e.clone(),
                max_rounds: None,
            },
            TierConfig::Full { effort, max_rounds } => Tier {
                effort: effort.clone(),
                max_rounds: *max_rounds,
            },
        }
    }
}

/// `verify = {rounds, effort}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyConfig {
    /// The most verification rounds.
    pub rounds: u32,
    /// The reviewer's effort in them.
    pub effort: String,
}

impl VerifyConfig {
    /// The verification it describes.
    pub fn verify(&self) -> Verify {
        Verify {
            rounds: self.rounds,
            effort: self.effort.clone(),
        }
    }
}

/// A ladder tier of a resolved profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tier {
    /// The reviewer's effort.
    pub effort: String,
    /// The most rounds the tier runs before the ladder moves on (`None`: until it converges,
    /// the transition rule or the backstop end it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
}

/// The capped verification of a resolved profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Verify {
    /// The most rounds.
    pub rounds: u32,
    /// The reviewer's effort.
    pub effort: String,
}

/// A profile, resolved: what `loop.started` records, so a loop runs on the profile it started
/// with whatever the config file says later.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Its name.
    pub name: String,
    /// When an implementation loop checks its plan.
    pub plan_check: PlanCheck,
    /// The ladder's tiers, in order.
    pub tiers: Vec<Tier>,
    /// Whether the sweep runs (when the ladder found something valid).
    pub sweep: bool,
    /// The capped verification.
    pub verify: Verify,
}

impl Profile {
    /// Built-in profile `name` ([`BUILTIN_PROFILES`]), the skills' table: `high` is a medium
    /// tier then an xhigh tier, the sweep and up to 5 verification rounds at xhigh; `medium`
    /// the medium tier only, the sweep and 2 at xhigh; `low` one medium round, no sweep and 1
    /// at medium.
    pub fn builtin(name: &str) -> Option<Profile> {
        let tier = |effort: &str, max_rounds| Tier {
            effort: effort.into(),
            max_rounds,
        };
        let verify = |rounds, effort: &str| Verify {
            rounds,
            effort: effort.into(),
        };
        let (plan_check, tiers, sweep, verify) = match name {
            "high" => (
                PlanCheck::Contracts,
                vec![tier("medium", None), tier("xhigh", None)],
                true,
                verify(5, "xhigh"),
            ),
            "medium" => (
                PlanCheck::Contracts,
                vec![tier("medium", None)],
                true,
                verify(2, "xhigh"),
            ),
            "low" => (
                PlanCheck::Never,
                vec![tier("medium", Some(1))],
                false,
                verify(1, "medium"),
            ),
            _ => return None,
        };
        Some(Profile {
            name: name.into(),
            plan_check,
            tiers,
            sweep,
            verify,
        })
    }

    /// What the setup card says of it: `high · medium tier, then xhigh · sweep · 5 verify`.
    pub fn summary(&self) -> String {
        let tiers: Vec<String> = self
            .tiers
            .iter()
            .map(|t| match t.max_rounds {
                Some(1) => format!("one {} round", t.effort),
                Some(n) => format!("{} tier ({n} rounds at most)", t.effort),
                None => format!("{} tier", t.effort),
            })
            .collect();
        let mut out = format!("{} · {}", self.name, tiers.join(", then "));
        if self.sweep {
            out.push_str(" · sweep");
        }
        out.push_str(&format!(" · {} verify", self.verify.rounds));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> anyhow::Result<LoopsConfig> {
        let c: LoopsConfig = toml::from_str(toml)?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn defaults_are_the_first_card_of_the_design() {
        let c = LoopsConfig::default();
        c.validate().unwrap();
        assert_eq!(c.implementer().unwrap(), None);
        assert_eq!(
            c.reviewer().unwrap(),
            ReviewerChoice {
                runtime: "codex".into(),
                account: None,
                model: "gpt-6.1-sol".into()
            }
        );
        assert_eq!(
            c.workers().unwrap(),
            WorkersChoice {
                model: "claude-opus-5-5".into(),
                effort: Some("high".into())
            }
        );
        assert_eq!(c.round_timeout(), Duration::from_secs(3600));
        assert_eq!(c.profile_names(), ["low", "medium", "high"]);
    }

    #[test]
    fn specs_parse_and_print_back() {
        let m = ModelChoice::parse("claude-code:work/claude-opus-5-5:high").unwrap();
        assert_eq!(m.runtime, "claude-code");
        assert_eq!(m.account.as_deref(), Some("work"));
        assert_eq!(m.model, "claude-opus-5-5");
        assert_eq!(m.effort.as_deref(), Some("high"));
        assert_eq!(m.to_string(), "claude-code:work/claude-opus-5-5:high");
        // A model id with a `:` of its own keeps it.
        let m = ModelChoice::parse("claude-code/vendor.model-v1:0").unwrap();
        assert_eq!((m.model.as_str(), m.effort), ("vendor.model-v1:0", None));
        assert_eq!(parse_implementer("thread").unwrap(), None);
        let r = ReviewerChoice::parse("codex:team/gpt-6.1-sol").unwrap();
        assert_eq!(r.to_string(), "codex:team/gpt-6.1-sol");
        let w = WorkersChoice::parse("claude-opus-5-5").unwrap();
        assert_eq!((w.model.as_str(), w.effort), ("claude-opus-5-5", None));

        for (bad, why) in [
            ("claude-opus-5-5", "is not runtime[:account]/model"),
            ("acp:x/m", "is not a runtime name"),
            ("claude-code:bad name/m", "account"),
            ("claude-code/", "the model is empty"),
            ("claude-code/a b", "not a model id"),
        ] {
            let e = ModelChoice::parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(parse_implementer("nope")
            .unwrap_err()
            .contains("or \"thread\""));
        // An ACP preset's name parses; whether it can review is the settings' to say.
        assert_eq!(ReviewerChoice::parse("gemini/x").unwrap().runtime, "gemini");
        for (bad, why) in [
            ("acp:gemini/x", "is not a runtime name"),
            ("codex/gpt-6.1-sol:high", "gives an effort"),
            ("codex", "is not runtime[:account]/model"),
        ] {
            let e = ReviewerChoice::parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        assert!(WorkersChoice::parse("claude-code/opus")
            .unwrap_err()
            .contains("implementer's runtime"));
        assert!(WorkersChoice::parse("opus high").is_err());
        // Not an effort (not lowercase letters): part of the model id.
        assert_eq!(
            WorkersChoice::parse("opus:HIGH").unwrap().model,
            "opus:HIGH"
        );
    }

    #[test]
    fn builtin_profiles_and_overrides_resolve() {
        let c = parse(
            r#"
profile = "quick"
[profiles.high]
verify = { rounds = 3, effort = "high" }

[profiles.quick]
tiers = [{ effort = "low", max_rounds = 2 }, "medium"]
verify = { rounds = 1, effort = "medium" }
"#,
        )
        .unwrap();
        let high = c.profile("high").unwrap();
        assert_eq!(high.tiers.len(), 2);
        assert!(high.sweep);
        assert_eq!(
            high.verify,
            Verify {
                rounds: 3,
                effort: "high".into()
            }
        );
        let quick = c.profile("quick").unwrap();
        assert_eq!(quick.plan_check, PlanCheck::Contracts);
        assert!(quick.sweep);
        assert_eq!(
            quick.tiers,
            [
                Tier {
                    effort: "low".into(),
                    max_rounds: Some(2)
                },
                Tier {
                    effort: "medium".into(),
                    max_rounds: None
                }
            ]
        );
        let low = c.profile("low").unwrap();
        assert_eq!(low.tiers[0].max_rounds, Some(1));
        assert!(!low.sweep);
        assert_eq!(c.profile("nope"), None);
        assert_eq!(c.profile_names(), ["low", "medium", "high", "quick"]);
        assert_eq!(
            Profile::builtin("high").unwrap().summary(),
            "high · medium tier, then xhigh tier · sweep · 5 verify"
        );
        assert_eq!(
            Profile::builtin("low").unwrap().summary(),
            "low · one medium round · 1 verify"
        );
    }

    #[test]
    fn plan_checks_parse_and_run_by_the_plans_risk() {
        for (text, v) in [
            ("never", PlanCheck::Never),
            ("contracts", PlanCheck::Contracts),
            (" always ", PlanCheck::Always),
        ] {
            assert_eq!(PlanCheck::parse(text).unwrap(), v);
            assert_eq!(PlanCheck::parse(v.as_str()).unwrap(), v);
        }
        assert!(PlanCheck::parse("yes")
            .unwrap_err()
            .contains("contracts, always or never"));
        assert!(!PlanCheck::Never.runs(true));
        assert!(PlanCheck::Contracts.runs(true));
        assert!(!PlanCheck::Contracts.runs(false));
        assert!(PlanCheck::Always.runs(false));
    }

    #[test]
    fn validation_names_the_key() {
        for (toml, key) in [
            ("profile = \"nope\"", "loops.profile: "),
            ("implementer = \"x\"", "loops.implementer: "),
            ("reviewer = \"acp:x/y\"", "loops.reviewer: "),
            ("workers = \"a/b\"", "loops.workers: "),
            ("max_audit_runs = 0", "loops.max_audit_runs: "),
            ("round_timeout = \"30s\"", "loops.round_timeout: "),
            ("round_timeout = \"soon\"", "loops.round_timeout: "),
            ("max_rounds_per_tier = 0", "loops.max_rounds_per_tier: "),
            ("[profiles.Bad]\ntiers = [\"medium\"]\nverify = { rounds = 1, effort = \"medium\" }", "loops.profiles.Bad: "),
            ("[profiles.mine]\nverify = { rounds = 1, effort = \"medium\" }", "loops.profiles.mine: "),
            ("[profiles.mine]\ntiers = [\"medium\"]", "loops.profiles.mine: "),
            ("[profiles.high]\ntiers = []", "loops.profiles.high.tiers: "),
            ("[profiles.high]\ntiers = [\"Medium\"]", "loops.profiles.high.tiers: "),
            ("[profiles.high]\ntiers = [{ effort = \"medium\", max_rounds = 0 }]", "loops.profiles.high.tiers: "),
            ("[profiles.high]\nverify = { rounds = 0, effort = \"xhigh\" }", "loops.profiles.high.verify: "),
        ] {
            let e = parse(toml).unwrap_err();
            assert!(format!("{e:#}").starts_with(key), "{toml}: {e:#}");
        }
        // Unknown keys are refused by the parser.
        assert!(toml::from_str::<LoopsConfig>("nope = 1").is_err());
        assert!(toml::from_str::<LoopsConfig>("[profiles.high]\nnope = 1").is_err());
    }
}

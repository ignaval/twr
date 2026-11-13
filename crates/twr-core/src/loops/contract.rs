//! What a loop reads from models: the reviewer's findings, and the fenced blocks the thread
//! ends its loop turns with (`twr-goal`, `twr-triage`, `twr-sweep`); plus the ledger the hub
//! builds from them (docs/design/loops.md, "Contracts").
//!
//! Everything here is parsed from another model's output, so every parser is strict about
//! shape and bounded in size, and none panics: a reviewer's answer that does not parse is a
//! failed review run, and a block that does not validate is the list of problems the hub puts
//! in its one correction prompt.
//!
//! # Finding ids
//!
//! A finding is named `<tag><round>.<n>`: `m3.2` is the second finding of the third round of
//! the tier tagged `m`. Round numbers count within their tier (or within the verification),
//! from 1, so the tag keeps ids unique within a loop. The tags ([`tier_tags`]):
//!
//! - a ladder tier is tagged by its effort: `n` minimal, `l` low, `m` medium, `h` high, `x`
//!   xhigh, `z` max, `u` ultra;
//! - a tier whose effort is none of those, or whose letter an earlier tier of the profile
//!   already took (two `medium` tiers), is tagged `t` and its place in the ladder as a letter:
//!   `ta` for the first tier, `tb` for the second (`tb2.1`);
//! - `v` is the verification's rounds (`v2.1`), and `p` is reserved for the plan check of an
//!   implementation loop; no tier takes either.
//!
//! A tag is lowercase letters only and a round number digits only, so an id parses back
//! ([`FindingId::parse`]).

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The JSON schema a review run's answer is constrained to: the skills' `findings.schema.json`,
/// in meaning unchanged. An empty `findings` list means the reviewer found nothing.
pub const FINDINGS_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["findings"],
  "properties": {
    "findings": {
      "type": "array",
      "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["severity", "location", "problem", "fix"],
        "properties": {
          "severity": { "type": "string", "enum": ["blocker", "major", "minor"] },
          "location": { "type": "string" },
          "problem": { "type": "string" },
          "fix": { "type": "string" }
        }
      }
    }
  }
}
"#;

/// Largest reviewer answer the hub reads, in bytes.
pub const MAX_ANSWER_BYTES: usize = 256 * 1024;
/// Most findings one answer may hold.
pub const MAX_FINDINGS: usize = 100;
/// Longest `location`, in bytes.
pub const MAX_LOCATION_BYTES: usize = 1024;
/// Longest `problem` or `fix`, in bytes.
pub const MAX_FINDING_TEXT_BYTES: usize = 16 * 1024;
/// Largest fenced block the hub reads from a thread's message, in bytes.
pub const MAX_BLOCK_BYTES: usize = 64 * 1024;
/// Longest GOAL paragraph, in bytes.
pub const MAX_GOAL_BYTES: usize = 4096;
/// Longest reason (a dismissal's, an accepted residual's), in bytes.
pub const MAX_REASON_BYTES: usize = 2048;
/// Longest rule name, in bytes.
pub const MAX_RULE_BYTES: usize = 80;
/// Most gate commands a block may list.
pub const MAX_GATES: usize = 32;
/// Longest gate command, in bytes.
pub const MAX_GATE_BYTES: usize = 512;
/// Most rules a sweep block may report.
pub const MAX_SWEEP_RULES: usize = 32;

/// Fence info string of the scope turn's block.
pub const GOAL_BLOCK: &str = "twr-goal";
/// Fence info string of a triage turn's block.
pub const TRIAGE_BLOCK: &str = "twr-triage";
/// Fence info string of the sweep turn's block.
pub const SWEEP_BLOCK: &str = "twr-sweep";

// ---------------------------------------------------------------------------------------------
// Findings
// ---------------------------------------------------------------------------------------------

/// How bad a finding is, as the reviewer says.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Must be fixed.
    Blocker,
    /// Should be fixed.
    Major,
    /// Worth fixing.
    #[default]
    Minor,
}

impl Severity {
    /// As the schema writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocker => "blocker",
            Severity::Major => "major",
            Severity::Minor => "minor",
        }
    }
}

/// One finding of a review run, numbered by the loop.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Finding {
    /// Its id (`m3.2`, module docs), given by the loop; empty in a reviewer's answer.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// How bad it is.
    pub severity: Severity,
    /// Where (`file:line`).
    pub location: String,
    /// What is wrong.
    pub problem: String,
    /// What the reviewer would do.
    pub fix: String,
}

/// The answer exactly as the schema allows it: no other key, every key given.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAnswer {
    findings: Vec<RawFinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFinding {
    severity: Severity,
    location: String,
    problem: String,
    fix: String,
}

/// Why a reviewer's answer was not read: a failed review run, never a finding.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnswerError {
    /// Larger than [`MAX_ANSWER_BYTES`].
    #[error("the answer is larger than {MAX_ANSWER_BYTES} bytes")]
    TooLarge,
    /// Not JSON of the schema's shape (an unknown severity, a missing or unknown key).
    #[error("the answer does not match the findings schema: {0}")]
    Shape(String),
    /// More than [`MAX_FINDINGS`] findings.
    #[error("the answer has more than {MAX_FINDINGS} findings")]
    TooMany,
    /// A field longer than its bound.
    #[error("finding {index}: its {field} is longer than {max} bytes")]
    FieldTooLong {
        /// 1-based place of the finding.
        index: usize,
        /// The field.
        field: &'static str,
        /// Its bound.
        max: usize,
    },
}

/// Reads a reviewer's answer file: `{"findings": [{severity, location, problem, fix}]}` and
/// nothing else, within the size bounds. The findings come back without ids
/// ([`number_findings`] gives them).
pub fn parse_findings(text: &str) -> Result<Vec<Finding>, AnswerError> {
    if text.len() > MAX_ANSWER_BYTES {
        return Err(AnswerError::TooLarge);
    }
    let raw: RawAnswer =
        serde_json::from_str(text.trim()).map_err(|e| AnswerError::Shape(e.to_string()))?;
    if raw.findings.len() > MAX_FINDINGS {
        return Err(AnswerError::TooMany);
    }
    let mut out = Vec::with_capacity(raw.findings.len());
    for (i, f) in raw.findings.into_iter().enumerate() {
        for (field, value, max) in [
            ("location", &f.location, MAX_LOCATION_BYTES),
            ("problem", &f.problem, MAX_FINDING_TEXT_BYTES),
            ("fix", &f.fix, MAX_FINDING_TEXT_BYTES),
        ] {
            if value.len() > max {
                return Err(AnswerError::FieldTooLong {
                    index: i + 1,
                    field,
                    max,
                });
            }
        }
        out.push(Finding {
            id: String::new(),
            severity: f.severity,
            location: f.location,
            problem: f.problem,
            fix: f.fix,
        });
    }
    Ok(out)
}

/// `findings` numbered as round `round` (`m3`) names them: `m3.1`, `m3.2`, …
pub fn number_findings(round: &str, findings: Vec<Finding>) -> Vec<Finding> {
    findings
        .into_iter()
        .enumerate()
        .map(|(i, f)| Finding {
            id: format!("{round}.{}", i + 1),
            ..f
        })
        .collect()
}

/// The tag of the verification's rounds (module docs).
pub const VERIFY_TAG: &str = "v";
/// The tag reserved for an implementation loop's plan check.
pub const PLAN_CHECK_TAG: &str = "p";

/// The tag of each tier of a ladder whose tiers have `efforts`, in order (module docs).
pub fn tier_tags<S: AsRef<str>>(efforts: &[S]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::with_capacity(efforts.len());
    for (i, effort) in efforts.iter().enumerate() {
        let letter = match effort.as_ref() {
            "minimal" => Some("n"),
            "low" => Some("l"),
            "medium" => Some("m"),
            "high" => Some("h"),
            "xhigh" => Some("x"),
            "max" => Some("z"),
            "ultra" => Some("u"),
            _ => None,
        };
        let tag = match letter {
            Some(l) if !tags.iter().any(|t| t == l) => l.to_string(),
            _ => {
                let place = u8::try_from(i).ok().filter(|i| *i < 26).unwrap_or(25);
                format!("t{}", char::from(b'a' + place))
            }
        };
        tags.push(tag);
    }
    tags
}

/// A finding id taken apart: `m3.2` is tag `m`, round 3, finding 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingId {
    /// The tier's tag, [`VERIFY_TAG`] or [`PLAN_CHECK_TAG`].
    pub tag: String,
    /// The round within its tier, from 1.
    pub round: u32,
    /// The finding within its round, from 1.
    pub n: u32,
}

impl FindingId {
    /// Parses `<tag><round>.<n>`; `None` for anything else.
    pub fn parse(s: &str) -> Option<FindingId> {
        let (round_id, n) = s.split_once('.')?;
        let digits = round_id.find(|c: char| c.is_ascii_digit())?;
        let (tag, round) = round_id.split_at(digits);
        let plain_digits = |d: &str| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit());
        if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_lowercase()) {
            return None;
        }
        if !plain_digits(round) || !plain_digits(n) {
            return None;
        }
        Some(FindingId {
            tag: tag.to_string(),
            round: round.parse().ok()?,
            n: n.parse().ok()?,
        })
    }

    /// The id of its round (`m3`).
    pub fn round_id(&self) -> String {
        format!("{}{}", self.tag, self.round)
    }
}

impl fmt::Display for FindingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}.{}", self.tag, self.round, self.n)
    }
}

// ---------------------------------------------------------------------------------------------
// Fenced blocks
// ---------------------------------------------------------------------------------------------

/// The body of the last fenced block with info string `info` in `text` (a line of three or
/// more backticks followed by `info`, up to a line of as many backticks or more and nothing
/// else; up to three spaces of indentation on either). `None`: no such block, or the last one
/// is never closed.
pub fn last_block<'a>(text: &'a str, info: &str) -> Option<&'a str> {
    let mut found = None;
    let mut open: Option<(usize, usize)> = None; // (fence length, body start)
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let bare = line.trim_end_matches(['\n', '\r']);
        let indent = bare.len() - bare.trim_start_matches(' ').len();
        if indent > 3 {
            continue;
        }
        let rest = &bare[indent..];
        let ticks = rest.len() - rest.trim_start_matches('`').len();
        if ticks < 3 {
            continue;
        }
        let after = rest[ticks..].trim();
        match open {
            Some((len, body)) => {
                if ticks >= len && after.is_empty() {
                    found = Some(&text[body..start]);
                    open = None;
                }
            }
            None if after == info => open = Some((ticks, pos)),
            None => {}
        }
    }
    found
}

/// Why a block a thread ended its turn with was refused: what the correction prompt says.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlockProblem {
    /// No closed block of that kind.
    #[error("no ```{0} block: end your reply with one")]
    Missing(&'static str),
    /// Larger than [`MAX_BLOCK_BYTES`].
    #[error("the ```{0} block is larger than {MAX_BLOCK_BYTES} bytes")]
    TooLarge(&'static str),
    /// Not JSON, or not an object.
    #[error("the ```{block} block is not a JSON object: {why}")]
    NotJson {
        /// The block's kind.
        block: &'static str,
        /// The parser's words.
        why: String,
    },
    /// Empty where it must say something.
    #[error("the ```{0} block is empty")]
    Empty(&'static str),
    /// A required key is missing or of the wrong type.
    #[error("{0}")]
    Field(String),
    /// A finding of the round has no item.
    #[error("finding {0} has no item: give it a verdict")]
    NoItem(String),
    /// An item names no finding of the round.
    #[error("item {0} is not a finding of this round")]
    UnknownFinding(String),
    /// Two items for one finding.
    #[error("finding {0} has more than one item")]
    Repeated(String),
    /// A `dismissed` item without a reason.
    #[error("{0} is dismissed without a reason: say why it is not a defect")]
    NoReason(String),
    /// A `duplicate` item that names no ledger entry.
    #[error("{0} is a duplicate without `of`: name the ledger entry it repeats (L1, L2…)")]
    NoOf(String),
    /// A `duplicate` of an entry the ledger does not have.
    #[error("{id} is a duplicate of {of}, which is not in the ledger")]
    UnknownEntry {
        /// The finding.
        id: String,
        /// The entry it named.
        of: String,
    },
    /// A `fixed` item without a class.
    #[error("{0} is fixed without a class: new-class, known-site or follow-on")]
    NoClass(String),
    /// The gates were red.
    #[error("gates.result is fail: gates must be green before the loop goes on")]
    GatesFailed,
}

/// A block's JSON object, or why not.
fn block_object(
    text: &str,
    info: &'static str,
) -> Result<serde_json::Map<String, Value>, BlockProblem> {
    let body = last_block(text, info).ok_or(BlockProblem::Missing(info))?;
    if body.len() > MAX_BLOCK_BYTES {
        return Err(BlockProblem::TooLarge(info));
    }
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(BlockProblem::NotJson {
            block: info,
            why: "it is not an object".into(),
        }),
        Err(e) => Err(BlockProblem::NotJson {
            block: info,
            why: e.to_string(),
        }),
    }
}

/// `m[key]` as a string of at most `max` bytes, trimmed; `Ok(None)` when absent or null.
fn opt_str(
    m: &serde_json::Map<String, Value>,
    key: &str,
    what: &str,
    max: usize,
) -> Result<Option<String>, BlockProblem> {
    match m.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= max => Ok(Some(s.trim().to_string())),
        Some(Value::String(_)) => Err(BlockProblem::Field(format!(
            "{what}: `{key}` is longer than {max} bytes"
        ))),
        Some(_) => Err(BlockProblem::Field(format!(
            "{what}: `{key}` is not a string"
        ))),
    }
}

/// The GOAL paragraph of the scope turn's ```` ```twr-goal ```` block: plain text, trimmed,
/// at most [`MAX_GOAL_BYTES`].
pub fn parse_goal(text: &str) -> Result<String, Vec<BlockProblem>> {
    let body = last_block(text, GOAL_BLOCK).ok_or(vec![BlockProblem::Missing(GOAL_BLOCK)])?;
    let goal = body.trim();
    if goal.is_empty() {
        return Err(vec![BlockProblem::Empty(GOAL_BLOCK)]);
    }
    if goal.len() > MAX_GOAL_BYTES {
        return Err(vec![BlockProblem::Field(format!(
            "the GOAL paragraph is longer than {MAX_GOAL_BYTES} bytes: one paragraph"
        ))]);
    }
    Ok(goal.to_string())
}

// ---------------------------------------------------------------------------------------------
// Gates
// ---------------------------------------------------------------------------------------------

/// Whether the gates passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateResult {
    /// Green.
    #[default]
    Pass,
    /// Red.
    Fail,
}

/// The gates a turn ran: `{"ran": [commands], "result": "pass" | "fail"}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gates {
    /// The commands, as the thread reports them.
    pub ran: Vec<String>,
    /// Whether they passed.
    pub result: GateResult,
}

/// `m["gates"]`, read: `Ok(None)` when absent.
fn parse_gates(m: &serde_json::Map<String, Value>) -> Result<Option<Gates>, BlockProblem> {
    let Some(g) = m.get("gates").filter(|g| !g.is_null()) else {
        return Ok(None);
    };
    let Some(g) = g.as_object() else {
        return Err(BlockProblem::Field("`gates` is not an object".into()));
    };
    let ran = match g.get("ran") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) if a.len() <= MAX_GATES => {
            let mut ran = Vec::with_capacity(a.len());
            for c in a {
                match c.as_str() {
                    Some(s) if s.len() <= MAX_GATE_BYTES => ran.push(s.to_string()),
                    _ => {
                        return Err(BlockProblem::Field(format!(
                            "`gates.ran` holds something that is not a command of at most \
                             {MAX_GATE_BYTES} bytes"
                        )))
                    }
                }
            }
            ran
        }
        Some(_) => {
            return Err(BlockProblem::Field(format!(
                "`gates.ran` is not a list of at most {MAX_GATES} commands"
            )))
        }
    };
    let result = match g.get("result").and_then(Value::as_str) {
        Some("pass") => GateResult::Pass,
        Some("fail") => GateResult::Fail,
        _ => {
            return Err(BlockProblem::Field(
                "`gates.result` is not \"pass\" or \"fail\"".into(),
            ))
        }
    };
    Ok(Some(Gates { ran, result }))
}

// ---------------------------------------------------------------------------------------------
// Triage
// ---------------------------------------------------------------------------------------------

/// What the thread made of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Valid, and fixed.
    #[default]
    Fixed,
    /// Not a defect, or intended: it goes into the ledger with its reason.
    Dismissed,
    /// Already in the ledger (`of`).
    Duplicate,
}

/// The class of a fixed finding (the skills' NEW-CLASS / KNOWN-SITE / FOLLOW-ON).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingClass {
    /// A rule the loop had not met yet.
    #[default]
    NewClass,
    /// Another site of a rule already met.
    KnownSite,
    /// A defect in one of the loop's own fixes.
    FollowOn,
}

impl FindingClass {
    /// As the triage block writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            FindingClass::NewClass => "new-class",
            FindingClass::KnownSite => "known-site",
            FindingClass::FollowOn => "follow-on",
        }
    }
}

/// One item of a triage block.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TriageItem {
    /// The finding (`m3.1`).
    pub id: String,
    /// What the thread made of it.
    pub verdict: Verdict,
    /// A `fixed` finding's class.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub class: Option<FindingClass>,
    /// A `fixed` finding's rule, a short name the sweep may audit (`identity-gating`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// A `dismissed` finding's reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// A `duplicate` finding's ledger entry (`L4`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
}

/// A triage block, validated.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Triage {
    /// One item per finding of the round, in the round's order.
    pub items: Vec<TriageItem>,
    /// The gates the turn ran (green).
    pub gates: Gates,
}

/// Reads and checks the ```` ```twr-triage ```` block `text` ends with, for a round whose
/// findings have ids `round` and a ledger whose entries have ids `ledger` (docs/design,
/// "The triage block"): every finding has exactly one item and no item names another; a
/// `dismissed` item has a non-empty reason; a `duplicate` names an existing ledger entry in
/// `of`; a `fixed` item has a class (and may have a rule); `gates.result` is `pass`. Every
/// problem found is listed, in the block's order, for the one correction prompt. Keys it does
/// not know are ignored.
pub fn parse_triage(
    text: &str,
    round: &[String],
    ledger: &[String],
) -> Result<Triage, Vec<BlockProblem>> {
    let m = block_object(text, TRIAGE_BLOCK).map_err(|p| vec![p])?;
    let mut problems = Vec::new();
    let items = match m.get("items") {
        Some(Value::Array(a)) => a.as_slice(),
        _ => {
            problems.push(BlockProblem::Field("`items` is not a list".into()));
            &[]
        }
    };
    let mut by_id: Vec<Option<TriageItem>> = vec![None; round.len()];
    for (i, item) in items.iter().enumerate() {
        let what = format!("item {}", i + 1);
        let Some(o) = item.as_object() else {
            problems.push(BlockProblem::Field(format!("{what} is not an object")));
            continue;
        };
        let id = match o.get("id").and_then(Value::as_str) {
            Some(id) => id.trim().to_string(),
            None => {
                problems.push(BlockProblem::Field(format!("{what} has no `id`")));
                continue;
            }
        };
        let Some(place) = round.iter().position(|r| *r == id) else {
            problems.push(BlockProblem::UnknownFinding(clip(&id)));
            continue;
        };
        if by_id[place].is_some() {
            problems.push(BlockProblem::Repeated(id));
            continue;
        }
        let verdict = match o.get("verdict").and_then(Value::as_str) {
            Some("fixed") => Verdict::Fixed,
            Some("dismissed") => Verdict::Dismissed,
            Some("duplicate") => Verdict::Duplicate,
            _ => {
                problems.push(BlockProblem::Field(format!(
                    "{id}: `verdict` is not fixed, dismissed or duplicate"
                )));
                by_id[place] = Some(TriageItem::default());
                continue;
            }
        };
        let fields = (|| -> Result<TriageItem, BlockProblem> {
            let mut t = TriageItem {
                id: id.clone(),
                verdict,
                ..Default::default()
            };
            match verdict {
                Verdict::Fixed => {
                    t.class = match o.get("class").and_then(Value::as_str) {
                        Some("new-class") => Some(FindingClass::NewClass),
                        Some("known-site") => Some(FindingClass::KnownSite),
                        Some("follow-on") => Some(FindingClass::FollowOn),
                        None if o.get("class").is_none_or(Value::is_null) => {
                            return Err(BlockProblem::NoClass(id.clone()))
                        }
                        _ => {
                            return Err(BlockProblem::Field(format!(
                                "{id}: `class` is not new-class, known-site or follow-on"
                            )))
                        }
                    };
                    t.rule = opt_str(o, "rule", &id, MAX_RULE_BYTES)?.filter(|r| !r.is_empty());
                }
                Verdict::Dismissed => {
                    t.reason =
                        opt_str(o, "reason", &id, MAX_REASON_BYTES)?.filter(|r| !r.is_empty());
                    if t.reason.is_none() {
                        return Err(BlockProblem::NoReason(id.clone()));
                    }
                }
                Verdict::Duplicate => {
                    let of = opt_str(o, "of", &id, 32)?.filter(|r| !r.is_empty());
                    let Some(of) = of else {
                        return Err(BlockProblem::NoOf(id.clone()));
                    };
                    if !ledger.contains(&of) {
                        return Err(BlockProblem::UnknownEntry { id: id.clone(), of });
                    }
                    t.of = Some(of);
                }
            }
            Ok(t)
        })();
        match fields {
            Ok(t) => by_id[place] = Some(t),
            Err(p) => {
                problems.push(p);
                // (Counted as present: its problem is named, not "no item" as well.)
                by_id[place] = Some(TriageItem::default());
            }
        }
    }
    for (id, item) in round.iter().zip(&by_id) {
        if item.is_none() {
            problems.push(BlockProblem::NoItem(id.clone()));
        }
    }
    let gates = match parse_gates(&m) {
        Ok(Some(g)) => {
            if g.result == GateResult::Fail {
                problems.push(BlockProblem::GatesFailed);
            }
            g
        }
        Ok(None) => {
            problems.push(BlockProblem::Field(
                "no `gates`: run the gates and report them, {\"ran\": [...], \"result\": \"pass\"}"
                    .into(),
            ));
            Gates::default()
        }
        Err(p) => {
            problems.push(p);
            Gates::default()
        }
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Triage {
        items: by_id.into_iter().flatten().collect(),
        gates,
    })
}

/// `s`, at most 64 bytes of it (an id another model made up, quoted back to it).
fn clip(s: &str) -> String {
    let mut end = s.len().min(64);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ---------------------------------------------------------------------------------------------
// Sweep
// ---------------------------------------------------------------------------------------------

/// What the sweep found of a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepResult {
    /// Sites were found and fixed.
    #[default]
    Fixed,
    /// Sites were found and left, with a reason: they go into the ledger.
    AcceptedResidual,
    /// No site survived verification.
    CheckedClean,
}

impl SweepResult {
    /// As the report writes it.
    pub fn as_str(self) -> &'static str {
        match self {
            SweepResult::Fixed => "fixed",
            SweepResult::AcceptedResidual => "accepted residual",
            SweepResult::CheckedClean => "checked clean",
        }
    }
}

/// One rule of a sweep block.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SweepItem {
    /// The rule's name.
    pub rule: String,
    /// What the sweep found of it.
    pub result: SweepResult,
    /// Why an accepted residual is left (required), or a note on the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Where an accepted residual is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

/// A sweep block, validated.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Sweep {
    /// One item per rule audited.
    pub rules: Vec<SweepItem>,
    /// The gates the turn ran, if it reported them (green).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<Gates>,
}

/// Reads and checks the ```` ```twr-sweep ```` block `text` ends with:
/// `{"rules": [{"rule", "result": "fixed" | "accepted residual" | "checked clean", "reason"?,
/// "location"?}], "gates"?}`. Every rule is named once; an accepted residual has a reason;
/// `gates`, when given, passed. (`accepted_residual`, `accepted-residual` and the like are
/// read as `accepted residual`.)
pub fn parse_sweep(text: &str) -> Result<Sweep, Vec<BlockProblem>> {
    let m = block_object(text, SWEEP_BLOCK).map_err(|p| vec![p])?;
    let mut problems = Vec::new();
    let mut rules: Vec<SweepItem> = Vec::new();
    match m.get("rules") {
        Some(Value::Array(a)) if a.len() <= MAX_SWEEP_RULES => {
            for (i, r) in a.iter().enumerate() {
                let what = format!("rule {}", i + 1);
                let Some(o) = r.as_object() else {
                    problems.push(BlockProblem::Field(format!("{what} is not an object")));
                    continue;
                };
                let item = (|| -> Result<SweepItem, BlockProblem> {
                    let rule = opt_str(o, "rule", &what, MAX_RULE_BYTES)?
                        .filter(|r| !r.is_empty())
                        .ok_or_else(|| BlockProblem::Field(format!("{what} has no `rule`")))?;
                    let normal = o
                        .get("result")
                        .and_then(Value::as_str)
                        .map(|s| s.trim().to_ascii_lowercase().replace(['_', '-'], " "));
                    let result = match normal.as_deref() {
                        Some("fixed") => SweepResult::Fixed,
                        Some("accepted residual") => SweepResult::AcceptedResidual,
                        Some("checked clean") => SweepResult::CheckedClean,
                        _ => {
                            return Err(BlockProblem::Field(format!(
                                "{rule}: `result` is not fixed, accepted residual or checked clean"
                            )))
                        }
                    };
                    let reason =
                        opt_str(o, "reason", &rule, MAX_REASON_BYTES)?.filter(|r| !r.is_empty());
                    if result == SweepResult::AcceptedResidual && reason.is_none() {
                        return Err(BlockProblem::Field(format!(
                            "{rule} is an accepted residual without a reason: say why it stays"
                        )));
                    }
                    let location = opt_str(o, "location", &rule, MAX_LOCATION_BYTES)?
                        .filter(|r| !r.is_empty());
                    Ok(SweepItem {
                        rule,
                        result,
                        reason,
                        location,
                    })
                })();
                match item {
                    Ok(item) if rules.iter().any(|r| r.rule == item.rule) => problems.push(
                        BlockProblem::Field(format!("rule {} is reported twice", item.rule)),
                    ),
                    Ok(item) => rules.push(item),
                    Err(p) => problems.push(p),
                }
            }
        }
        _ => problems.push(BlockProblem::Field(format!(
            "`rules` is not a list of at most {MAX_SWEEP_RULES} rules"
        ))),
    }
    let gates = match parse_gates(&m) {
        Ok(g) => {
            if g.as_ref().is_some_and(|g| g.result == GateResult::Fail) {
                problems.push(BlockProblem::GatesFailed);
            }
            g
        }
        Err(p) => {
            problems.push(p);
            None
        }
    };
    if !problems.is_empty() {
        return Err(problems);
    }
    Ok(Sweep { rules, gates })
}

// ---------------------------------------------------------------------------------------------
// Ledger
// ---------------------------------------------------------------------------------------------

/// Why an entry is in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerKind {
    /// A finding the thread dismissed: not a defect, or intended.
    #[default]
    Dismissed,
    /// A site the sweep found and left on purpose.
    AcceptedResidual,
}

/// One entry of the ledger every reviewer prompt names ("do not re-raise").
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LedgerEntry {
    /// `L1`, `L2`, … in the order entries were added ([`ledger_id`]).
    pub id: String,
    /// Why it is there.
    pub kind: LedgerKind,
    /// What it came from: a finding id (`m3.2`) or, for an accepted residual, its rule.
    pub from: String,
    /// Where, as the finding or the sweep said.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// The finding's problem, or the rule's name for an accepted residual.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub problem: String,
    /// Why it is not changed.
    pub reason: String,
}

/// The id of the `n`th ledger entry, from 1: `L1`.
pub fn ledger_id(n: usize) -> String {
    format!("L{n}")
}

/// `ledger.md`: the text the reviewer reads by path, its title and the reviewer's
/// instruction over [`render_ledger_entries`].
pub fn render_ledger(entries: &[LedgerEntry]) -> String {
    let mut out = String::from(
        "# Ledger\n\nPREVIOUSLY REVIEWED — intentionally NOT changed, and ACCEPTED RESIDUALS. \
         Do NOT re-raise anything listed here.\n\n",
    );
    if entries.is_empty() {
        out.push_str("none yet\n");
    } else {
        out.push_str(&render_ledger_entries(entries));
    }
    out
}

/// The ledger's entries, one item each (empty: none): the body of `ledger.md`, and what the
/// loop report shows under its own heading. Lines of `entries` are kept to one line each
/// (newlines become spaces), so an entry can't pose as a heading.
pub fn render_ledger_entries(entries: &[LedgerEntry]) -> String {
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for e in entries {
        let kind = match e.kind {
            LedgerKind::Dismissed => "dismissed",
            LedgerKind::AcceptedResidual => "accepted residual",
        };
        out.push_str(&format!("- {} ({kind}, {})", e.id, one_line(&e.from)));
        if !e.location.is_empty() {
            out.push_str(&format!(" `{}`", one_line(&e.location).replace('`', "'")));
        }
        if !e.problem.is_empty() {
            out.push_str(&format!(": {}", one_line(&e.problem)));
        }
        out.push_str(&format!("\n  Reason: {}\n", one_line(&e.reason)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_schema_is_the_skills_schema() {
        let ours: Value = serde_json::from_str(FINDINGS_SCHEMA).unwrap();
        let skills = serde_json::json!({
          "type": "object",
          "additionalProperties": false,
          "required": ["findings"],
          "properties": {
            "findings": {
              "type": "array",
              "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["severity", "location", "problem", "fix"],
                "properties": {
                  "severity": { "type": "string", "enum": ["blocker", "major", "minor"] },
                  "location": { "type": "string" },
                  "problem": { "type": "string" },
                  "fix": { "type": "string" }
                }
              }
            }
          }
        });
        assert_eq!(ours, skills);
    }

    #[test]
    fn findings_parse_and_number() {
        let text = r#"{"findings":[{"severity":"major","location":"a.rs:3","problem":"p","fix":"f"},
            {"severity":"blocker","location":"b.rs:1","problem":"q","fix":"g"}]}"#;
        let f = number_findings("m3", parse_findings(text).unwrap());
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].id, "m3.1");
        assert_eq!(f[1].id, "m3.2");
        assert_eq!(f[1].severity, Severity::Blocker);
        assert_eq!(parse_findings("{\"findings\": []}\n").unwrap(), vec![]);
    }

    #[test]
    fn findings_refuse_what_the_schema_does() {
        let one = |extra: &str| {
            format!(
                r#"{{"findings":[{{"severity":"major","location":"a","problem":"p"{extra}}}]}}"#
            )
        };
        for (bad, why) in [
            ("not json".to_string(), "does not match"),
            ("[]".to_string(), "does not match"),
            ("{}".to_string(), "does not match"),
            (r#"{"findings":[],"x":1}"#.to_string(), "does not match"),
            (one(""), "does not match"),
            (one(r#","fix":"f","id":"m1.1""#), "does not match"),
            (
                r#"{"findings":[{"severity":"critical","location":"a","problem":"p","fix":"f"}]}"#
                    .to_string(),
                "does not match",
            ),
            (
                r#"{"findings":[{"severity":"major","location":1,"problem":"p","fix":"f"}]}"#
                    .to_string(),
                "does not match",
            ),
        ] {
            let e = parse_findings(&bad).unwrap_err();
            assert!(e.to_string().contains(why), "{bad}: {e}");
        }
        let huge = " ".repeat(MAX_ANSWER_BYTES + 1);
        assert_eq!(parse_findings(&huge), Err(AnswerError::TooLarge));
        let many = format!(
            "{{\"findings\":[{}]}}",
            vec![
                r#"{"severity":"minor","location":"a","problem":"p","fix":"f"}"#;
                MAX_FINDINGS + 1
            ]
            .join(",")
        );
        assert_eq!(parse_findings(&many), Err(AnswerError::TooMany));
        let long = format!(
            r#"{{"findings":[{{"severity":"minor","location":"{}","problem":"p","fix":"f"}}]}}"#,
            "a".repeat(MAX_LOCATION_BYTES + 1)
        );
        assert_eq!(
            parse_findings(&long),
            Err(AnswerError::FieldTooLong {
                index: 1,
                field: "location",
                max: MAX_LOCATION_BYTES
            })
        );
    }

    #[test]
    fn tier_tags_and_ids() {
        assert_eq!(tier_tags(&["medium", "xhigh"]), ["m", "x"]);
        assert_eq!(tier_tags(&["low", "high", "max"]), ["l", "h", "z"]);
        assert_eq!(tier_tags(&["medium", "medium", "turbo"]), ["m", "tb", "tc"]);
        for tag in tier_tags(&["minimal", "low", "medium", "high", "xhigh", "max", "ultra"]) {
            assert!(tag != VERIFY_TAG && tag != PLAN_CHECK_TAG);
        }
        let id = FindingId::parse("m3.2").unwrap();
        assert_eq!((id.tag.as_str(), id.round, id.n), ("m", 3, 2));
        assert_eq!(id.round_id(), "m3");
        assert_eq!(id.to_string(), "m3.2");
        assert_eq!(FindingId::parse("tb12.10").unwrap().to_string(), "tb12.10");
        for bad in ["m3", "3.2", "M3.2", "m.2", "m3.", "m3.x", "m-3.2", ""] {
            assert_eq!(FindingId::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn last_block_takes_the_last_closed_fence() {
        let text = "x\n```twr-triage\n{\"a\":1}\n```\ntext\n  ````twr-triage\n{\"a\":2}\n````\n";
        assert_eq!(last_block(text, "twr-triage"), Some("{\"a\":2}\n"));
        assert_eq!(last_block("```twr-triage\nopen", "twr-triage"), None);
        assert_eq!(last_block("```json\n{}\n```", "twr-triage"), None);
        // A shorter fence inside does not close it.
        let nested = "```twr-goal\nsee:\n```` no\n```\n";
        assert_eq!(last_block(nested, "twr-goal"), Some("see:\n```` no\n"));
        assert_eq!(
            parse_goal("```twr-goal\n  Make it fast.\n```").unwrap(),
            "Make it fast."
        );
        assert_eq!(
            parse_goal("```twr-goal\n\n```").unwrap_err(),
            vec![BlockProblem::Empty(GOAL_BLOCK)]
        );
        assert_eq!(
            parse_goal("no block").unwrap_err(),
            vec![BlockProblem::Missing(GOAL_BLOCK)]
        );
    }

    fn triage(body: &str) -> String {
        format!("I fixed things.\n\n```twr-triage\n{body}\n```\n")
    }

    #[test]
    fn a_valid_triage_block_reads() {
        let t = parse_triage(
            &triage(
                r#"{"items": [
  {"id": "m3.1", "verdict": "fixed", "class": "new-class", "rule": "identity-gating"},
  {"id": "m3.2", "verdict": "dismissed", "reason": "the retry is idempotent"},
  {"id": "m3.3", "verdict": "duplicate", "of": "L4", "note": "ignored"}
 ],
 "gates": {"ran": ["cargo test --workspace"], "result": "pass"}}"#,
            ),
            &ids(&["m3.1", "m3.2", "m3.3"]),
            &ids(&["L1", "L2", "L3", "L4"]),
        )
        .unwrap();
        assert_eq!(t.items.len(), 3);
        assert_eq!(t.items[0].class, Some(FindingClass::NewClass));
        assert_eq!(t.items[0].rule.as_deref(), Some("identity-gating"));
        assert_eq!(
            t.items[1].reason.as_deref(),
            Some("the retry is idempotent")
        );
        assert_eq!(t.items[2].of.as_deref(), Some("L4"));
        assert_eq!(t.gates.ran, ["cargo test --workspace"]);
    }

    #[test]
    fn each_triage_rule_names_its_problem() {
        let round = ids(&["m1.1", "m1.2"]);
        let ledger = ids(&["L1"]);
        let gates = r#""gates": {"ran": [], "result": "pass"}"#;
        let check = |body: String, want: Vec<BlockProblem>| {
            let got = parse_triage(&triage(&body), &round, &ledger).unwrap_err();
            assert_eq!(got, want, "{body}");
        };
        let fixed2 = r#"{"id": "m1.2", "verdict": "fixed", "class": "known-site"}"#;
        check(
            format!(r#"{{"items": [{fixed2}], {gates}}}"#),
            vec![BlockProblem::NoItem("m1.1".into())],
        );
        check(
            format!(
                r#"{{"items": [{fixed2}, {fixed2}, {{"id": "m1.1", "verdict": "fixed", "class": "follow-on"}}], {gates}}}"#
            ),
            vec![BlockProblem::Repeated("m1.2".into())],
        );
        check(
            format!(
                r#"{{"items": [{fixed2}, {{"id": "m9.1", "verdict": "fixed"}}, {{"id": "m1.1", "verdict": "dismissed", "reason": "  "}}], {gates}}}"#
            ),
            vec![
                BlockProblem::UnknownFinding("m9.1".into()),
                BlockProblem::NoReason("m1.1".into()),
            ],
        );
        check(
            format!(
                r#"{{"items": [{fixed2}, {{"id": "m1.1", "verdict": "duplicate"}}], {gates}}}"#
            ),
            vec![BlockProblem::NoOf("m1.1".into())],
        );
        check(
            format!(
                r#"{{"items": [{fixed2}, {{"id": "m1.1", "verdict": "duplicate", "of": "L7"}}], {gates}}}"#
            ),
            vec![BlockProblem::UnknownEntry {
                id: "m1.1".into(),
                of: "L7".into(),
            }],
        );
        check(
            format!(r#"{{"items": [{fixed2}, {{"id": "m1.1", "verdict": "fixed"}}], {gates}}}"#),
            vec![BlockProblem::NoClass("m1.1".into())],
        );
        check(
            format!(
                r#"{{"items": [{fixed2}, {{"id": "m1.1", "verdict": "fixed", "class": "big"}}], {gates}}}"#
            ),
            vec![BlockProblem::Field(
                "m1.1: `class` is not new-class, known-site or follow-on".into(),
            )],
        );
        check(
            format!(r#"{{"items": [{fixed2}, {{"id": "m1.1", "verdict": "maybe"}}], {gates}}}"#),
            vec![BlockProblem::Field(
                "m1.1: `verdict` is not fixed, dismissed or duplicate".into(),
            )],
        );
        let both =
            format!(r#"{fixed2}, {{"id": "m1.1", "verdict": "fixed", "class": "new-class"}}"#);
        check(
            format!(r#"{{"items": [{both}], "gates": {{"ran": ["make"], "result": "fail"}}}}"#),
            vec![BlockProblem::GatesFailed],
        );
        check(
            format!(r#"{{"items": [{both}]}}"#),
            vec![BlockProblem::Field(
                "no `gates`: run the gates and report them, {\"ran\": [...], \"result\": \"pass\"}"
                    .into(),
            )],
        );
        check(
            format!(r#"{{"items": [{both}], "gates": {{"result": "green"}}}}"#),
            vec![BlockProblem::Field(
                "`gates.result` is not \"pass\" or \"fail\"".into(),
            )],
        );
        assert_eq!(
            parse_triage("no block here", &round, &ledger).unwrap_err(),
            vec![BlockProblem::Missing(TRIAGE_BLOCK)]
        );
        let e = parse_triage(&triage("{oops"), &round, &ledger).unwrap_err();
        assert!(matches!(e[..], [BlockProblem::NotJson { .. }]), "{e:?}");
        let big = triage(&" ".repeat(MAX_BLOCK_BYTES + 1));
        assert_eq!(
            parse_triage(&big, &round, &ledger).unwrap_err(),
            vec![BlockProblem::TooLarge(TRIAGE_BLOCK)]
        );
        // The messages the correction prompt gets.
        assert_eq!(
            BlockProblem::NoOf("m1.1".into()).to_string(),
            "m1.1 is a duplicate without `of`: name the ledger entry it repeats (L1, L2…)"
        );
        assert_eq!(
            BlockProblem::GatesFailed.to_string(),
            "gates.result is fail: gates must be green before the loop goes on"
        );
    }

    #[test]
    fn sweep_blocks_read_and_refuse() {
        let s = parse_sweep(
            "```twr-sweep\n{\"rules\": [{\"rule\": \"identity-gating\", \"result\": \"fixed\"},\
             {\"rule\": \"retries\", \"result\": \"accepted_residual\", \"reason\": \"bounded\", \
             \"location\": \"a.rs:9\"}, {\"rule\": \"locks\", \"result\": \"checked clean\"}]}\n```",
        )
        .unwrap();
        assert_eq!(s.rules.len(), 3);
        assert_eq!(s.rules[1].result, SweepResult::AcceptedResidual);
        assert_eq!(s.gates, None);
        for (body, want) in [
            (
                r#"{"rules": [{"rule": "r", "result": "accepted residual"}]}"#,
                "accepted residual without a reason",
            ),
            (r#"{"rules": [{"result": "fixed"}]}"#, "has no `rule`"),
            (
                r#"{"rules": [{"rule": "r", "result": "done"}]}"#,
                "`result` is not",
            ),
            (
                r#"{"rules": [{"rule": "r", "result": "fixed"}, {"rule": "r", "result": "fixed"}]}"#,
                "reported twice",
            ),
            (r#"{"rules": 1}"#, "`rules` is not a list"),
            (
                r#"{"rules": [], "gates": {"ran": [], "result": "fail"}}"#,
                "gates must be green",
            ),
        ] {
            let e = parse_sweep(&format!("```twr-sweep\n{body}\n```")).unwrap_err();
            assert!(
                e.iter().any(|p| p.to_string().contains(want)),
                "{body}: {e:?}"
            );
        }
    }

    #[test]
    fn the_ledger_renders_one_line_per_entry() {
        assert!(render_ledger(&[]).ends_with("none yet\n"));
        let text = render_ledger(&[
            LedgerEntry {
                id: ledger_id(1),
                kind: LedgerKind::Dismissed,
                from: "m3.2".into(),
                location: "src/a.rs:10".into(),
                problem: "retry is\nnot idempotent".into(),
                reason: "it is: upsert on (user_id, key)".into(),
            },
            LedgerEntry {
                id: ledger_id(2),
                kind: LedgerKind::AcceptedResidual,
                from: "identity-gating".into(),
                reason: "\n# heading\n".into(),
                ..Default::default()
            },
        ]);
        assert!(text.contains(
            "- L1 (dismissed, m3.2) `src/a.rs:10`: retry is not idempotent\n  Reason: it is: upsert on (user_id, key)\n"
        ), "{text}");
        assert!(text.contains("- L2 (accepted residual, identity-gating)\n  Reason: # heading\n"));
        assert!(!text.contains("\n# heading"));
    }
}

//! Subscription usage: how much of each plan window (Claude's 5-hour and weekly limits, Codex's
//! primary and secondary windows, Grok Build's credit period) the logged-in account has used.
//!
//! Adapters report what their runtime says as `runtime.limits` ([`crate::event::RuntimeLimits`]):
//! passively from the runtime's own limit events, and from a read that costs no model turn when
//! a session starts and at the end of a turn once the data is [`REFRESH_AFTER`] old. The hub
//! keeps the latest windows per runtime and account ([`merge_windows`]) and records them as
//! `account.usage` on the hub thread ([`crate::event::AccountUsage`]) when they change
//! ([`changed`]) or the last record is [`REFRESH_AFTER`] old. [`UsageView`] folds those events
//! (the hub on replay, every client from the control stream), and [`UsageView::shown`] is what
//! the clients show: the windows that have not reset yet, per runtime, with their age.
//!
//! Stored percentages are the share of the window **used** (0 to 100, more when a runtime
//! lets usage run past the cap), as every runtime reports it. Human-facing text shows the
//! share left ([`remaining_pct`]), clamped to 0–100. Accounts: `account` is `None` for the
//! runtime's own login (the only one today); a runtime with entries for more than one account
//! shows the account in its label ([`UsageView::shown`]), one with a single entry never does.

use std::collections::BTreeMap;

use chrono::{DateTime, TimeDelta, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::event::{Event, EventBody, HUB_THREAD};

/// Window id of a 5-hour window (Claude's session limit, Codex's 300-minute window).
pub const FIVE_HOUR: &str = "five_hour";
/// Window id of a 7-day window (Claude's weekly limit, Codex's 10080-minute window, a Grok
/// Build weekly credit period).
pub const SEVEN_DAY: &str = "seven_day";
/// Window id of a monthly window (a Grok Build monthly credit period).
pub const MONTH: &str = "month";

/// Data older than this is shown with its age, and is read again at the next turn end.
pub const REFRESH_AFTER: TimeDelta = TimeDelta::minutes(10);
/// Remaining share at or below which a window is drawn as getting low.
pub const WARN_PCT: f64 = 25.0;
/// Remaining share at or below which a window is low: drawn as such, with its reset time.
pub const LOW_PCT: f64 = 10.0;

/// The share left in a window, in percent, clamped to 0–100.
pub fn remaining_pct(used_pct: f64) -> f64 {
    (100.0 - used_pct).clamp(0.0, 100.0)
}

/// One usage window of a plan.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UsageWindow {
    /// Window id: [`FIVE_HOUR`], [`SEVEN_DAY`], [`MONTH`], or `<minutes>m` for another length.
    pub window: String,
    /// Share of the window used, in percent.
    pub used_pct: f64,
    /// When the window resets, if the runtime says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
    /// The window's length in minutes, when the runtime gives it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u64>,
    /// When the runtime reported this window (the hub stamps it; a record keeps each window's
    /// newest report, [`merge_windows`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

impl UsageWindow {
    /// A window of length `minutes` (Codex's `windowDurationMins`): [`FIVE_HOUR`] and
    /// [`SEVEN_DAY`] by their lengths, else `<minutes>m`.
    pub fn of_minutes(minutes: u64, used_pct: f64, resets_at: Option<DateTime<Utc>>) -> Self {
        let window = match minutes {
            300 => FIVE_HOUR.to_string(),
            10080 => SEVEN_DAY.to_string(),
            m => format!("{m}m"),
        };
        UsageWindow {
            window,
            used_pct,
            resets_at,
            minutes: Some(minutes),
            observed_at: None,
        }
    }

    /// The window's length in minutes, known or implied by its id.
    pub fn length(&self) -> Option<u64> {
        self.minutes.or(match self.window.as_str() {
            FIVE_HOUR => Some(300),
            SEVEN_DAY => Some(10080),
            MONTH => Some(43200),
            _ => None,
        })
    }

    /// Short label: `5h`, `wk`, `mo`, else the length (`90m`, `8h`, `2d`), else the id.
    pub fn label(&self) -> String {
        match self.window.as_str() {
            FIVE_HOUR => return "5h".into(),
            SEVEN_DAY => return "wk".into(),
            MONTH => return "mo".into(),
            _ => {}
        }
        match self.minutes {
            Some(m) if m % 1440 == 0 => format!("{}d", m / 1440),
            Some(m) if m % 60 == 0 => format!("{}h", m / 60),
            Some(m) => format!("{m}m"),
            None => self.window.clone(),
        }
    }

    /// Whether the window has reset since it was read (what it said no longer holds).
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_some_and(|r| r <= now)
    }

    /// The used share for change detection: a whole percent, never below 0.
    pub fn pct(&self) -> i64 {
        // A runtime's percentage is a small number; `as` saturates anything else.
        self.used_pct.round().max(0.0) as i64
    }

    /// The remaining share as drawn: a whole percent, clamped to 0–100.
    pub fn left_pct(&self) -> i64 {
        remaining_pct(self.used_pct).round() as i64
    }

    /// How low the remaining share is ([`WARN_PCT`], [`LOW_PCT`]), before rounding.
    pub fn level(&self) -> Level {
        let left = remaining_pct(self.used_pct);
        if left <= LOW_PCT {
            Level::Low
        } else if left <= WARN_PCT {
            Level::Warn
        } else {
            Level::Normal
        }
    }
}

/// How low a window's remaining share is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Above [`WARN_PCT`] left.
    Normal,
    /// At or below [`WARN_PCT`] left, above [`LOW_PCT`].
    Warn,
    /// At or below [`LOW_PCT`] left.
    Low,
}

/// The windows known after `new` was reported on top of `old`: for each window, the newest
/// report of it (a window of `new` replaces the same window of `old` unless that one was
/// observed later: a runner's replayed report may be older than a local one), plus the windows
/// of `old` that `new` does not mention (a runtime's passive report may carry only the window
/// that moved); all but those that have reset by `now`. Shortest window first.
pub fn merge_windows(
    old: &[UsageWindow],
    new: &[UsageWindow],
    now: DateTime<Utc>,
) -> Vec<UsageWindow> {
    let newer = |a: &UsageWindow, b: &UsageWindow| match (a.observed_at, b.observed_at) {
        (Some(x), Some(y)) => x > y,
        _ => false,
    };
    let mut out: Vec<UsageWindow> = Vec::new();
    for w in new.iter().filter(|w| !w.expired(now)) {
        match old.iter().find(|o| o.window == w.window) {
            Some(o) if newer(o, w) => out.push(o.clone()),
            _ => out.push(w.clone()),
        }
    }
    for w in old {
        if !w.expired(now) && !out.iter().any(|n| n.window == w.window) {
            out.push(w.clone());
        }
    }
    out.sort_by(|a, b| {
        (a.length().unwrap_or(u64::MAX), &a.window)
            .cmp(&(b.length().unwrap_or(u64::MAX), &b.window))
    });
    out
}

/// Whether `new` says something `old` did not: another set of windows, a whole percent that
/// moved, or a reset time that moved by a minute or more (runtimes report the same reset with
/// varying sub-second parts).
pub fn changed(old: &[UsageWindow], new: &[UsageWindow]) -> bool {
    old.len() != new.len()
        || old.iter().zip(new).any(|(a, b)| {
            a.window != b.window
                || a.pct() != b.pct()
                || match (a.resets_at, b.resets_at) {
                    (Some(x), Some(y)) => (x - y).num_seconds().abs() >= 60,
                    (x, y) => x.is_some() != y.is_some(),
                }
        })
}

/// The latest known usage of one account of one runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEntry {
    /// Runtime name (`claude-code`, `codex`, `grok-build`, ...).
    pub runtime: String,
    /// Account name; `None` for the runtime's own login.
    pub account: Option<String>,
    /// Its windows, shortest first.
    pub windows: Vec<UsageWindow>,
    /// When it was last observed.
    pub observed_at: DateTime<Utc>,
    /// How: `event` (the runtime's own limit report) or `read` (twr asked).
    pub source: String,
    /// The plan, when the runtime says (`max`, `pro`, ...).
    pub plan: Option<String>,
}

/// One runtime's usage as clients show it ([`UsageView::shown`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    /// Runtime name.
    pub runtime: String,
    /// Account name (`None`: the runtime's own login).
    pub account: Option<String>,
    /// `Claude`, `GPT`, `Grok` or the runtime's name; `Claude:work` when the runtime has
    /// several accounts.
    pub label: String,
    /// `C`, `G`, `Gk` or the runtime's first letter; `C:work` likewise.
    pub short: String,
    /// The windows that have not reset yet, shortest first (never empty).
    pub windows: Vec<UsageWindow>,
    /// When the data was observed: the oldest report of its windows.
    pub observed_at: DateTime<Utc>,
    /// Its age.
    pub age: TimeDelta,
    /// Older than [`REFRESH_AFTER`].
    pub stale: bool,
    /// How it was observed (`event` or `read`).
    pub source: String,
    /// The plan, when known.
    pub plan: Option<String>,
}

/// Long and short display names of a runtime.
pub fn runtime_labels(runtime: &str) -> (String, String) {
    match runtime {
        "claude-code" => ("Claude".into(), "C".into()),
        "codex" => ("GPT".into(), "G".into()),
        "grok-build" => ("Grok".into(), "Gk".into()),
        other => {
            let short = other
                .chars()
                .next()
                .map(|c| c.to_uppercase().to_string())
                .unwrap_or_default();
            (other.to_string(), short)
        }
    }
}

/// Order of runtimes in a usage line: Claude, Codex, Grok, then the rest by name.
fn runtime_rank(runtime: &str) -> (u8, &str) {
    match runtime {
        "claude-code" => (0, ""),
        "codex" => (1, ""),
        "grok-build" => (2, ""),
        other => (3, other),
    }
}

/// The latest usage per runtime and account, folded from `account.usage` events. Serialized
/// (in the hub's checkpoints of its folded state) as its entries, in order: each one's key is
/// its runtime and account.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(from = "Vec<UsageEntry>", into = "Vec<UsageEntry>")]
pub struct UsageView {
    entries: BTreeMap<(String, Option<String>), UsageEntry>,
}

impl From<Vec<UsageEntry>> for UsageView {
    fn from(entries: Vec<UsageEntry>) -> Self {
        let entries = entries
            .into_iter()
            .map(|e| ((e.runtime.clone(), e.account.clone()), e))
            .collect();
        UsageView { entries }
    }
}

impl From<UsageView> for Vec<UsageEntry> {
    fn from(v: UsageView) -> Self {
        v.entries.into_values().collect()
    }
}

impl UsageView {
    /// Folds one event: an `account.usage` on the hub thread replaces its entry.
    pub fn apply(&mut self, ev: &Event) {
        if ev.thread != HUB_THREAD {
            return;
        }
        if let EventBody::AccountUsage(p) = &ev.body {
            if p.runtime.is_empty() {
                return;
            }
            self.entries.insert(
                (p.runtime.clone(), p.account.clone()),
                UsageEntry {
                    runtime: p.runtime.clone(),
                    account: p.account.clone(),
                    windows: p.windows.clone(),
                    observed_at: p.observed_at.unwrap_or(ev.ts),
                    source: p.source.clone(),
                    plan: p.plan.clone(),
                },
            );
        }
    }

    /// The entry of `runtime`'s `account`.
    pub fn get(&self, runtime: &str, account: Option<&str>) -> Option<&UsageEntry> {
        self.entries
            .get(&(runtime.to_string(), account.map(str::to_string)))
    }

    /// Every entry.
    pub fn entries(&self) -> impl Iterator<Item = &UsageEntry> {
        self.entries.values()
    }

    /// What clients show at `now`: per runtime and account, the windows that have not reset
    /// (an entry left with none is not shown), Claude first, then Codex, Grok and the rest by
    /// name. A runtime with more than one entry shows each account in its label, also while
    /// some of them have nothing to show.
    pub fn shown(&self, now: DateTime<Utc>) -> Vec<Shown> {
        let mut out: Vec<Shown> = Vec::new();
        for e in self.entries.values() {
            let windows: Vec<UsageWindow> = e
                .windows
                .iter()
                .filter(|w| !w.expired(now))
                .cloned()
                .collect();
            if windows.is_empty() {
                continue;
            }
            let observed_at = windows
                .iter()
                .filter_map(|w| w.observed_at)
                .min()
                .unwrap_or(e.observed_at);
            let age = (now - observed_at).max(TimeDelta::zero());
            let (label, short) = runtime_labels(&e.runtime);
            out.push(Shown {
                runtime: e.runtime.clone(),
                account: e.account.clone(),
                label,
                short,
                windows,
                observed_at,
                age,
                stale: age >= REFRESH_AFTER,
                source: e.source.clone(),
                plan: e.plan.clone(),
            });
        }
        let several = |runtime: &str| {
            self.entries
                .keys()
                .filter(|(r, _)| r.as_str() == runtime)
                .count()
                > 1
        };
        for s in &mut out {
            if several(&s.runtime) {
                let account = s.account.as_deref().unwrap_or("default");
                s.label = format!("{}:{account}", s.label);
                s.short = format!("{}:{account}", s.short);
            }
        }
        out.sort_by(|a, b| {
            (runtime_rank(&a.runtime), &a.account).cmp(&(runtime_rank(&b.runtime), &b.account))
        });
        out
    }
}

/// How much a usage line says ([`segment`]), widest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Form {
    /// `left: Claude 5h 8% resets 14:30 · wk 56% | GPT wk 58% as of 12m ago`
    Full,
    /// `left: Claude 5h 8% · wk 56% | GPT wk 58%`
    Brief,
    /// `C 8%/56% | G 58%`
    Compact,
}

impl Form {
    /// Every form, widest first.
    pub const ALL: [Form; 3] = [Form::Full, Form::Brief, Form::Compact];
}

/// What a piece of a usage line is, for styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// `left:`, separators and runtime labels.
    Plain,
    /// A window's label and remaining percentage, with its level and whether its data is stale.
    Window(Level, bool),
    /// A reset time.
    Reset,
    /// The data's age.
    Age,
}

/// A usage line in `form`: pieces of text with what each is ([`Part`]). Reset times are in
/// `tz`, with the weekday when not within a day of `now`. Empty when nothing is shown.
pub fn segment<Tz: TimeZone>(
    shown: &[Shown],
    form: Form,
    now: DateTime<Utc>,
    tz: &Tz,
) -> Vec<(String, Part)>
where
    Tz::Offset: std::fmt::Display,
{
    let mut out: Vec<(String, Part)> = Vec::new();
    if shown.is_empty() {
        return out;
    }
    if form != Form::Compact {
        out.push(("left: ".into(), Part::Plain));
    }
    for (i, s) in shown.iter().enumerate() {
        if i > 0 {
            out.push((" | ".into(), Part::Plain));
        }
        if form == Form::Compact {
            out.push((format!("{} ", s.short), Part::Plain));
            for (j, w) in s.windows.iter().enumerate() {
                if j > 0 {
                    out.push(("/".into(), Part::Plain));
                }
                out.push((
                    format!("{}%", w.left_pct()),
                    Part::Window(w.level(), s.stale),
                ));
            }
            continue;
        }
        out.push((format!("{} ", s.label), Part::Plain));
        for (j, w) in s.windows.iter().enumerate() {
            if j > 0 {
                out.push((" · ".into(), Part::Plain));
            }
            out.push((
                format!("{} {}%", w.label(), w.left_pct()),
                Part::Window(w.level(), s.stale),
            ));
            if form == Form::Full && w.level() == Level::Low {
                if let Some(r) = w.resets_at {
                    out.push((format!(" resets {}", reset_time(r, now, tz)), Part::Reset));
                }
            }
        }
        if form == Form::Full && s.stale {
            out.push((format!(" as of {} ago", age(s.age)), Part::Age));
        }
    }
    out
}

/// The text of [`segment`].
pub fn segment_text<Tz: TimeZone>(
    shown: &[Shown],
    form: Form,
    now: DateTime<Utc>,
    tz: &Tz,
) -> String
where
    Tz::Offset: std::fmt::Display,
{
    segment(shown, form, now, tz)
        .into_iter()
        .map(|(t, _)| t)
        .collect()
}

/// A reset time in `tz`: `14:30` within a day of `now`, else `Thu 05:00`.
pub fn reset_time<Tz: TimeZone>(at: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let local = at.with_timezone(tz);
    if at - now < TimeDelta::hours(24) {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %H:%M").to_string()
    }
}

/// An age or a wait in its largest whole unit: `40s`, `12m`, `3h`, `2d`.
pub fn age(d: TimeDelta) -> String {
    let s = d.num_seconds().max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// [`age`] of `secs` seconds (`usage.get`'s `age_secs`).
pub fn age_secs(secs: i64) -> String {
    age(TimeDelta::seconds(secs))
}

/// What a thread's turns spent: the `usage` of its `turn.ended` events, summed
/// ([`crate::event::Usage`]: input tokens, cache reads and writes included, output tokens, and
/// the cost when the runtime reports one). The thread's view keeps it
/// ([`crate::reduce::ThreadView::spent`]); `usage.threads` sums it over a time window from the
/// log. Claude Code reports a cost on every plan: the API price of the tokens, a measure of
/// size, not a bill on a subscription. Codex, Grok Build and ACP agents report none.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Spent {
    /// Input tokens (cache reads and writes included).
    pub input: u64,
    /// Output tokens.
    pub output: u64,
    /// The cost of the turns that reported one, in USD. Read from `null` as 0, as
    /// `usage.threads` writes a row whose turns reported none.
    #[serde(deserialize_with = "null_as_zero")]
    pub cost: f64,
    /// Turns that reported usage.
    pub turns: u64,
    /// Of those, the turns that reported a cost.
    pub costed_turns: u64,
}

impl Spent {
    /// Adds one turn's usage. A cost that is negative or not a number counts as unreported.
    pub fn add(&mut self, u: &crate::event::Usage) {
        self.input = self.input.saturating_add(u.input);
        self.output = self.output.saturating_add(u.output);
        self.turns += 1;
        if let Some(c) = u.cost.filter(|c| c.is_finite() && *c >= 0.0) {
            self.cost += c;
            self.costed_turns += 1;
        }
    }

    /// Adds another total (several threads together).
    pub fn merge(&mut self, other: &Spent) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cost += other.cost;
        self.turns += other.turns;
        self.costed_turns += other.costed_turns;
    }

    /// Nothing spent yet (no turn reported usage).
    pub fn is_empty(&self) -> bool {
        self.turns == 0
    }

    /// Input and output tokens together.
    pub fn tokens(&self) -> u64 {
        self.input.saturating_add(self.output)
    }

    /// The cost, when at least one turn reported one.
    pub fn cost(&self) -> Option<f64> {
        (self.costed_turns > 0).then_some(self.cost)
    }

    /// Short text for cards and tables: `1.2M tok $3.40`, `85k tok` (no cost reported), or
    /// `None` when nothing was spent.
    pub fn short(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let tokens = format!("{} tok", tokens_text(self.tokens()));
        Some(match self.cost() {
            Some(c) => format!("{tokens} {}", cost_text(c)),
            None => tokens,
        })
    }
}

/// A number that may be `null` (or absent, with `#[serde(default)]`), as 0.
fn null_as_zero<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.unwrap_or(0.0))
}

/// A token count for people: `950`, `12k`, `1.2M`, `3.4B`.
pub fn tokens_text(n: u64) -> String {
    let f = n as f64;
    // Each unit up to where its rounding would read as the next one (`1000k`).
    if n < 1_000 {
        n.to_string()
    } else if n < 9_950 {
        format!("{:.1}k", f / 1e3)
    } else if n < 999_500 {
        format!("{:.0}k", f / 1e3)
    } else if n < 999_950_000 {
        format!("{:.1}M", f / 1e6)
    } else {
        format!("{:.1}B", f / 1e9)
    }
}

/// A token count with a decimal more than [`tokens_text`] below 100k: `850`, `34.9k`, `127k`,
/// `1.2M` (what a compaction line says, where `35k → 13k` would hide the change).
pub fn tokens_text_fine(n: u64) -> String {
    if (1_000..99_950).contains(&n) {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        tokens_text(n)
    }
}

/// A cost in USD for people: `$0.42`, `$12.30`, `$1234`.
pub fn cost_text(c: f64) -> String {
    if c >= 1000.0 {
        format!("${c:.0}")
    } else {
        format!("${c:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AccountUsage, Source};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn win(id: &str, pct: f64, resets: Option<&str>) -> UsageWindow {
        UsageWindow {
            window: id.into(),
            used_pct: pct,
            resets_at: resets.map(at),
            minutes: None,
            observed_at: None,
        }
    }

    fn usage_event(
        runtime: &str,
        account: Option<&str>,
        windows: Vec<UsageWindow>,
        ts: &str,
    ) -> Event {
        let mut ev = Event::new(
            HUB_THREAD,
            1,
            None,
            Source::Hub,
            EventBody::AccountUsage(AccountUsage {
                runtime: runtime.into(),
                account: account.map(str::to_string),
                windows,
                source: "event".into(),
                ..Default::default()
            }),
        );
        ev.ts = at(ts);
        ev
    }

    const NOW: &str = "2026-10-09T12:00:00Z";

    fn view() -> UsageView {
        let mut v = UsageView::default();
        v.apply(&usage_event(
            "codex",
            None,
            vec![UsageWindow::of_minutes(
                10080,
                42.0,
                Some(at("2026-10-14T05:00:00Z")),
            )],
            "2026-10-09T11:48:00Z",
        ));
        v.apply(&usage_event(
            "claude-code",
            None,
            vec![
                win(FIVE_HOUR, 22.4, Some("2026-10-09T15:20:00Z")),
                win(SEVEN_DAY, 44.0, Some("2026-10-14T05:00:00Z")),
            ],
            "2026-10-09T11:59:00Z",
        ));
        v
    }

    #[test]
    fn window_labels_follow_their_length() {
        assert_eq!(UsageWindow::of_minutes(300, 1.0, None).window, FIVE_HOUR);
        assert_eq!(UsageWindow::of_minutes(10080, 1.0, None).label(), "wk");
        assert_eq!(UsageWindow::of_minutes(300, 1.0, None).label(), "5h");
        assert_eq!(UsageWindow::of_minutes(480, 1.0, None).label(), "8h");
        assert_eq!(UsageWindow::of_minutes(2880, 1.0, None).label(), "2d");
        assert_eq!(UsageWindow::of_minutes(90, 1.0, None).label(), "90m");
        assert_eq!(win(MONTH, 1.0, None).label(), "mo");
        assert_eq!(win("odd", 1.0, None).label(), "odd");
    }

    #[test]
    fn the_full_line_reads_like_the_example() {
        let now = at(NOW);
        let shown = view().shown(now);
        assert_eq!(
            segment_text(&shown, Form::Full, now, &Utc),
            "left: Claude 5h 78% · wk 56% | GPT wk 58% as of 12m ago"
        );
        assert_eq!(
            segment_text(&shown, Form::Brief, now, &Utc),
            "left: Claude 5h 78% · wk 56% | GPT wk 58%"
        );
        assert_eq!(
            segment_text(&shown, Form::Compact, now, &Utc),
            "C 78%/56% | G 58%"
        );
    }

    #[test]
    fn a_low_window_says_when_it_resets() {
        let now = at(NOW);
        let mut v = view();
        v.apply(&usage_event(
            "claude-code",
            None,
            vec![
                win(FIVE_HOUR, 90.0, Some("2026-10-09T14:30:00Z")),
                win(SEVEN_DAY, 95.0, Some("2026-10-14T05:00:00Z")),
            ],
            "2026-10-09T11:59:00Z",
        ));
        let shown = v.shown(now);
        assert_eq!(
            segment_text(&shown, Form::Full, now, &Utc),
            "left: Claude 5h 10% resets 14:30 · wk 5% resets Wed 05:00 | GPT wk 58% as of 12m ago"
        );
        let parts = segment(&shown, Form::Compact, now, &Utc);
        assert!(parts.contains(&("10%".into(), Part::Window(Level::Low, false))));
        assert!(parts.contains(&("58%".into(), Part::Window(Level::Normal, true))));
        for form in [Form::Brief, Form::Compact] {
            assert!(!segment_text(&shown, form, now, &Utc).contains("resets"));
        }
    }

    #[test]
    fn levels() {
        assert_eq!(win(FIVE_HOUR, 74.9, None).level(), Level::Normal);
        assert_eq!(win(FIVE_HOUR, 75.0, None).level(), Level::Warn);
        assert_eq!(win(FIVE_HOUR, 75.1, None).level(), Level::Warn);
        assert_eq!(win(FIVE_HOUR, 89.9, None).level(), Level::Warn);
        assert_eq!(win(FIVE_HOUR, 90.0, None).level(), Level::Low);
        assert_eq!(win(FIVE_HOUR, 90.1, None).level(), Level::Low);
        assert_eq!(win(FIVE_HOUR, 104.4, None).level(), Level::Low);
        assert_eq!(win(FIVE_HOUR, -3.0, None).level(), Level::Normal);
        // Change detection still uses the runtime's used percentage.
        assert_eq!(win(FIVE_HOUR, 104.4, None).pct(), 104);
        assert_eq!(win(FIVE_HOUR, -3.0, None).pct(), 0);
    }

    #[test]
    fn remaining_percentages_are_clamped_then_rounded() {
        for (used, left, drawn) in [
            (-3.0, 100.0, 100),
            (0.0, 100.0, 100),
            (22.5, 77.5, 78),
            (22.6, 77.4, 77),
            (100.0, 0.0, 0),
            (104.4, 0.0, 0),
            (f64::INFINITY, 0.0, 0),
            (f64::NEG_INFINITY, 100.0, 100),
        ] {
            assert!((remaining_pct(used) - left).abs() < 1e-10, "{used}");
            assert_eq!(win(FIVE_HOUR, used, None).left_pct(), drawn, "{used}");
        }
    }

    #[test]
    fn all_forms_show_clamped_remaining_percentages() {
        let now = at(NOW);
        let mut v = UsageView::default();
        v.apply(&usage_event(
            "claude-code",
            None,
            vec![win(FIVE_HOUR, 104.4, None), win(SEVEN_DAY, -3.0, None)],
            NOW,
        ));
        for (form, expected) in [
            (Form::Full, "left: Claude 5h 0% · wk 100%"),
            (Form::Brief, "left: Claude 5h 0% · wk 100%"),
            (Form::Compact, "C 0%/100%"),
        ] {
            assert_eq!(segment_text(&v.shown(now), form, now, &Utc), expected);
        }
    }

    #[test]
    fn windows_that_reset_are_not_shown() {
        // The 5-hour window reset at 11:00: only the weekly one still holds.
        let mut v = UsageView::default();
        v.apply(&usage_event(
            "claude-code",
            None,
            vec![
                win(FIVE_HOUR, 80.0, Some("2026-10-09T11:00:00Z")),
                win(SEVEN_DAY, 44.0, Some("2026-10-14T05:00:00Z")),
            ],
            "2026-10-09T08:00:00Z",
        ));
        let now = at(NOW);
        let shown = v.shown(now);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].windows.len(), 1);
        assert!(shown[0].stale);
        assert_eq!(
            segment_text(&shown, Form::Full, now, &Utc),
            "left: Claude wk 56% as of 4h ago"
        );
        // Once every window reset, the runtime is not shown at all.
        assert!(v.shown(at("2026-10-15T00:00:00Z")).is_empty());
        assert_eq!(segment_text(&[], Form::Full, now, &Utc), "");
    }

    #[test]
    fn several_accounts_name_theirs() {
        let mut v = view();
        v.apply(&usage_event(
            "claude-code",
            Some("work"),
            vec![win(SEVEN_DAY, 10.0, Some("2026-10-14T05:00:00Z"))],
            "2026-10-09T11:59:00Z",
        ));
        let now = at(NOW);
        let shown = v.shown(now);
        let labels: Vec<&str> = shown.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Claude:default", "Claude:work", "GPT"]);
        assert_eq!(
            segment_text(&shown, Form::Compact, now, &Utc),
            "C:default 78%/56% | C:work 90% | G 58%"
        );
        // Once the native account's windows reset, work keeps its label: the runtime still
        // has two accounts.
        let later = at("2026-10-14T06:00:00Z");
        v.apply(&usage_event(
            "claude-code",
            Some("work"),
            vec![win(SEVEN_DAY, 11.0, Some("2026-10-21T05:00:00Z"))],
            "2026-10-14T05:59:00Z",
        ));
        let shown = v.shown(later);
        let labels: Vec<&str> = shown.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["Claude:work"]);
    }

    /// The observation time a record carries (a runner's capture time) wins over its `ts`.
    #[test]
    fn the_observation_time_is_the_reports() {
        let mut v = UsageView::default();
        let mut ev = usage_event("codex", None, vec![win(SEVEN_DAY, 1.0, None)], NOW);
        if let EventBody::AccountUsage(p) = &mut ev.body {
            p.observed_at = Some(at("2026-10-09T11:30:00Z"));
        }
        v.apply(&ev);
        let e = v.get("codex", None).unwrap();
        assert_eq!(e.observed_at, at("2026-10-09T11:30:00Z"));
        assert!(v.shown(at(NOW))[0].stale);
    }

    #[test]
    fn only_hub_events_count() {
        let mut v = UsageView::default();
        let mut ev = usage_event("codex", None, vec![win(SEVEN_DAY, 1.0, None)], NOW);
        ev.thread = "t1".into();
        v.apply(&ev);
        assert!(v.entries().next().is_none());
    }

    #[test]
    fn merging_keeps_what_the_new_report_leaves_out() {
        let now = at(NOW);
        let old = vec![
            win(FIVE_HOUR, 10.0, Some("2026-10-09T15:00:00Z")),
            win(SEVEN_DAY, 40.0, Some("2026-10-14T05:00:00Z")),
            win("other", 5.0, Some("2026-10-09T11:00:00Z")),
        ];
        let new = vec![
            win(SEVEN_DAY, 41.0, Some("2026-10-14T05:00:00Z")),
            win("reset", 7.0, Some("2026-10-09T11:59:00Z")),
        ];
        let merged = merge_windows(&old, &new, now);
        assert_eq!(merged, vec![old[0].clone(), new[0].clone()]);
    }

    /// Each window keeps its newest report: an older report of a window (a runner's replayed
    /// frame) does not replace a newer one, but still updates the windows it has newer.
    #[test]
    fn merging_keeps_each_windows_newest_report() {
        let now = at(NOW);
        let seen = |w: UsageWindow, t: &str| UsageWindow {
            observed_at: Some(at(t)),
            ..w
        };
        let old = vec![
            seen(win(FIVE_HOUR, 10.0, None), "2026-10-09T11:00:00Z"),
            seen(win(SEVEN_DAY, 45.0, None), "2026-10-09T11:10:00Z"),
        ];
        let new = vec![
            seen(win(FIVE_HOUR, 20.0, None), "2026-10-09T11:05:00Z"),
            seen(win(SEVEN_DAY, 41.0, None), "2026-10-09T11:05:00Z"),
        ];
        let merged = merge_windows(&old, &new, now);
        assert_eq!(merged, vec![new[0].clone(), old[1].clone()]);
        // The data is as old as its oldest window.
        let mut v = UsageView::default();
        let mut ev = usage_event("claude-code", None, merged, NOW);
        if let EventBody::AccountUsage(p) = &mut ev.body {
            p.observed_at = Some(at("2026-10-09T11:10:00Z"));
        }
        v.apply(&ev);
        let shown = v.shown(at("2026-10-09T11:20:00Z"));
        assert_eq!(shown[0].observed_at, at("2026-10-09T11:05:00Z"));
        assert!(shown[0].stale);
    }

    #[test]
    fn change_detection() {
        let a = vec![win(FIVE_HOUR, 22.2, Some("2026-10-09T15:20:00.055Z"))];
        // The same whole percent and a reset that moved by under a second: no change.
        assert!(!changed(
            &a,
            &[win(FIVE_HOUR, 21.6, Some("2026-10-09T15:20:00.9Z"))]
        ));
        assert!(changed(
            &a,
            &[win(FIVE_HOUR, 23.0, Some("2026-10-09T15:20:00Z"))]
        ));
        assert!(changed(
            &a,
            &[win(FIVE_HOUR, 22.0, Some("2026-10-09T20:20:00Z"))]
        ));
        assert!(changed(&a, &[win(FIVE_HOUR, 22.0, None)]));
        assert!(changed(&a, &[]));
        assert!(changed(
            &a,
            &[win(SEVEN_DAY, 22.0, Some("2026-10-09T15:20:00Z"))]
        ));
    }

    #[test]
    fn ages() {
        assert_eq!(age(TimeDelta::seconds(40)), "40s");
        assert_eq!(age(TimeDelta::minutes(12)), "12m");
        assert_eq!(age(TimeDelta::hours(3)), "3h");
        assert_eq!(age(TimeDelta::days(2)), "2d");
        assert_eq!(age_secs(720), "12m");
    }

    #[test]
    fn spent_sums_turns_and_says_when_a_cost_is_known() {
        use crate::event::Usage;
        let mut s = Spent::default();
        assert!(s.is_empty());
        assert_eq!(s.short(), None);
        s.add(&Usage {
            input: 1_200_000,
            output: 30_000,
            cost: None,
        });
        assert_eq!(s.cost(), None);
        assert_eq!(s.short().as_deref(), Some("1.2M tok"));
        s.add(&Usage {
            input: 5,
            output: 5,
            cost: Some(3.4),
        });
        s.add(&Usage {
            input: 0,
            output: 0,
            cost: Some(f64::NAN),
        });
        s.add(&Usage {
            input: 0,
            output: 0,
            cost: Some(-1.0),
        });
        assert_eq!((s.turns, s.costed_turns), (4, 1));
        assert_eq!(s.cost(), Some(3.4));
        assert_eq!(s.short().as_deref(), Some("1.2M tok $3.40"));
        // A `usage.threads` row: a null cost reads as none reported.
        let row: Spent = serde_json::from_value(serde_json::json!({
            "input": 9, "output": 1, "cost": null, "turns": 1, "costed_turns": 0
        }))
        .unwrap();
        assert_eq!((row.turns, row.cost()), (1, None));
        let mut both = s.clone();
        both.merge(&s);
        assert_eq!(
            (both.input, both.turns, both.cost()),
            (2_400_010, 8, Some(6.8))
        );
    }

    #[test]
    fn token_and_cost_text() {
        for (n, text) in [
            (0, "0"),
            (950, "950"),
            (1_234, "1.2k"),
            (12_345, "12k"),
            (9_949, "9.9k"),
            (9_950, "10k"),
            (999_499, "999k"),
            (999_999, "1.0M"),
            (999_949_999, "999.9M"),
            (999_950_000, "1.0B"),
            (1_234_567, "1.2M"),
            (3_400_000_000, "3.4B"),
        ] {
            assert_eq!(tokens_text(n), text);
        }
        assert_eq!(cost_text(0.424), "$0.42");
        assert_eq!(cost_text(12.3), "$12.30");
        assert_eq!(cost_text(1234.4), "$1234");
    }
}

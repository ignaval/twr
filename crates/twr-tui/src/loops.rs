//! Review loops in the TUI (docs/design/loops.md): what the cards, the action popup, the
//! transcript and the settings editor show of them, and the commands that start and control
//! them.
//!
//! - **The loop line**: a thread running a loop has, as the first row of its card's section
//!   (stack and sidebar cards; not grid tiles), the badge ` REVIEW LOOP ` and
//!   [`twr_core::loops::line`] ([`card_line`]), which drops the profile and then shortens
//!   on a narrow card before it is cut; on a one-row card the short form ([`short_line`]).
//!   Its action popup shows the loop's progress ([`progress_rows`]), a phase the loop passed
//!   over (a skipped sweep or verification) marked `skipped`.
//! - **The setup card** (`AttentionItem::loop_setup`) and **the stuck card**
//!   (`AttentionItem::loop_stuck`): badges of their own ([`item_badge`]); a card shows the
//!   setup card's compact form ([`setup_compact`]), its popup and detail view the hub's
//!   summary with its rows aligned ([`setup_rows`]).
//! - **The editor** ([`LoopEditor`]): a setup card's draft (`loop.setup.get`, saved with
//!   `loop.setup.update`) or the saved settings (`loop.settings.get`, `loop.settings.set`,
//!   `loop.settings.reset`), drawn as the settings overlay is ([`render`]). Nothing is saved
//!   until its Save row (or `s`) saves it.
//! - **The transcript**: a turn a loop's prompt started folds to one row naming its step
//!   ([`prompt_step`]); a round's findings and triage are one row ([`round_head`]) that opens
//!   to them ([`round_rows`]).

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;
use serde_json::{json, Value};
use twr_core::loops::config::{
    ImplCommit, ModelChoice, PlanCheck, ReviewCommit, ReviewerChoice, SweepMode, REVIEWER_RUNTIMES,
};
use twr_core::loops::contract::{Finding, TriageItem, Verdict};
use twr_core::loops::settings::{
    default_model, LoopSettings, Preset, RuntimeAvailability, SaveFor,
};
use twr_core::loops::{LoopKind, LoopView, Phase, Wait};
use twr_core::reduce::AttentionItem;

use crate::app::{Action, AppState};
use crate::select::{PaneId, Panes, Target};

/// The fill of the ` REVIEW LOOP ` badge: a color no other badge or status has (light
/// magenta; a dive's band is the darker magenta), which stays itself with 16 colors.
pub const LOOP_COLOR: Color = Color::LightMagenta;

/// The fill of the ` LOOP STUCK ` badge (white text on magenta): apart from the loop's own
/// badge and from every attention badge, with 16 colors too.
pub const STUCK_COLOR: Color = Color::Magenta;

/// The fill a warning's mark is on (`Warning:`, `1 warning`), its text black ([`warning_mark`]).
const WARNING_COLOR: Color = Color::Yellow;

/// A warning's mark (`Warning:`, `1 warning`): black on yellow, as a question's badge is. A
/// yellow text would not read on a light terminal (1.7:1 on white, and no named yellow does
/// better with 16 colors); on its fill it reads on a dark one and a light one alike, and with
/// no colors it is drawn in reverse video. The rest of a warning is in the terminal's own
/// colors.
fn warning_mark(text: impl Into<String>) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default()
            .fg(crate::color::text_on(WARNING_COLOR))
            .bg(WARNING_COLOR),
    )
}

/// The badge of a loop of `kind`: ` REVIEW LOOP `.
pub fn kind_badge(kind: LoopKind) -> Span<'static> {
    Span::styled(
        format!(" {} ", twr_core::loops::badge(kind)),
        Style::default()
            .fg(Color::Black)
            .bg(LOOP_COLOR)
            .add_modifier(Modifier::BOLD),
    )
}

/// The badge of a stuck loop's item: ` LOOP STUCK `.
pub fn stuck_badge() -> Span<'static> {
    Span::styled(
        " LOOP STUCK ",
        Style::default()
            .fg(Color::White)
            .bg(STUCK_COLOR)
            .add_modifier(Modifier::BOLD),
    )
}

/// The badge of a loop's item: its setup card's or its stuck item's; `None` for any other.
pub fn item_badge(item: &AttentionItem) -> Option<Span<'static>> {
    if item.loop_setup {
        Some(kind_badge(setup_kind(&item.summary)))
    } else if item.loop_stuck {
        Some(stuck_badge())
    } else {
        None
    }
}

/// A loop of `kind` in words: `review loop`, `implementation loop`.
pub fn kind_words(kind: LoopKind) -> &'static str {
    match kind {
        LoopKind::Review => "review loop",
        LoopKind::Implement => "implementation loop",
    }
}

/// What a detail view's title calls a loop's item (`review loop`, `loop stuck`).
pub fn item_label(item: &AttentionItem) -> Option<&'static str> {
    if item.loop_setup {
        Some(kind_words(setup_kind(&item.summary)))
    } else if item.loop_stuck {
        Some("loop stuck")
    } else {
        None
    }
}

/// A loop's item: the user answers it, and no one dismisses it.
pub fn is_loop_item(item: &AttentionItem) -> bool {
    item.loop_setup || item.loop_stuck
}

/// The kind of loop a setup card's summary is for (`api: review loop setup (…)`).
pub fn setup_kind(summary: &str) -> LoopKind {
    let first = summary.lines().next().unwrap_or_default();
    if first.contains(" implementation loop setup") {
        LoopKind::Implement
    } else {
        LoopKind::Review
    }
}

/// Why a setup card is raised, as its summary's first line says it in parentheses
/// (`first run: choose its models`).
pub fn setup_why(summary: &str) -> String {
    let first = summary.lines().next().unwrap_or_default();
    first
        .split_once(" setup (")
        .and_then(|(_, rest)| rest.rsplit_once(')'))
        .map(|(why, _)| why.to_string())
        .unwrap_or_else(|| "check its settings".into())
}

/// The setup card's compact form, as a card shows it: the badge and why it is raised
/// (`REVIEW LOOP  first run: choose its models`), and, when the hub says something has to
/// change first, how many things (the popup and the detail view show the card in full).
pub fn setup_compact(item: &AttentionItem) -> Vec<Span<'static>> {
    let mut spans = vec![
        kind_badge(setup_kind(&item.summary)),
        Span::raw(" "),
        Span::raw(setup_why(&item.summary)),
    ];
    let first = |p: &str| item.summary.lines().filter(|l| l.starts_with(p)).count();
    match (first("Change first: "), first("Warning: ")) {
        (0, 0) => {}
        (0, w) => spans.extend([
            Span::raw(" · "),
            warning_mark(format!("{w} warning{}", if w == 1 { "" } else { "s" })),
        ]),
        (n, _) => spans.push(Span::styled(
            format!(" · {n} to change first"),
            Style::default().fg(Color::Red),
        )),
    }
    spans
}

/// Columns of a setup card's row label (`Implementer  `), as the hub pads it.
const LABEL_W: usize = 13;

/// A setup card's summary `width` cells wide, as the hub wrote it: its badge and first line,
/// then its `Label   value` rows with the values aligned (each value wrapped under itself; on
/// a row too narrow for that, under its label), then its warnings (`Warning:` marked,
/// [`warning_mark`]) and what has to change first (red).
pub fn setup_rows(item: &AttentionItem, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = item.summary.lines();
    let first = lines.next().unwrap_or_default().to_string();
    let mut rows = crate::ui::wrap_line(
        Line::from(vec![
            kind_badge(setup_kind(&item.summary)),
            Span::raw(" "),
            Span::raw(first),
        ]),
        width,
    );
    let dim = Style::default().fg(Color::DarkGray);
    for line in lines {
        if let Some(rest) = line.strip_prefix("Warning:") {
            rows.extend(crate::ui::wrap_line(
                Line::from(vec![warning_mark("Warning:"), Span::raw(rest.to_string())]),
                width,
            ));
            continue;
        }
        // A value's next line (a task of several lines), as the hub indents it: under the
        // value.
        if let Some(rest) = line
            .strip_prefix(&" ".repeat(LABEL_W))
            .filter(|r| !r.trim().is_empty())
        {
            rows.extend(aligned("", rest.trim_end(), width, dim));
            continue;
        }
        let (style, text) = if line.starts_with("Change first: ") {
            (
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                line,
            )
        } else {
            match label_row(line) {
                Some((label, value)) => {
                    rows.extend(aligned(label, value, width, dim));
                    continue;
                }
                None => (Style::default(), line),
            }
        };
        rows.extend(crate::ui::wrap_line(
            Line::from(Span::styled(text.to_string(), style)),
            width,
        ));
    }
    rows
}

/// A `Label   value` row of a setup card's summary: its label and value, or `None`.
fn label_row(line: &str) -> Option<(&str, &str)> {
    let at = line.find("  ")?;
    let (label, value) = (&line[..at], line[at..].trim_start());
    (!label.is_empty() && !label.contains("  ") && !value.is_empty() && at < LABEL_W)
        .then_some((label, value))
}

/// `label` and `value` `width` cells wide: the value after the label padded to [`LABEL_W`],
/// its other rows under it; with no room for that, the value on rows of its own, under the
/// label.
fn aligned(label: &str, value: &str, width: usize, dim: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if width >= LABEL_W + 12 {
        let rows = crate::ui::wrap_line(Line::from(Span::raw(value.to_string())), width - LABEL_W);
        for (k, mut row) in rows.into_iter().enumerate() {
            let lead = if k == 0 {
                format!("{label:<LABEL_W$}")
            } else {
                " ".repeat(LABEL_W)
            };
            row.spans.insert(0, Span::styled(lead, dim));
            out.push(row);
        }
    } else {
        if !label.is_empty() {
            out.extend(crate::ui::wrap_line(
                Line::from(Span::styled(label.to_string(), dim)),
                width,
            ));
        }
        out.extend(crate::ui::wrap_line(
            Line::from(Span::raw(format!("  {value}"))),
            width,
        ));
    }
    out
}

// ---------------------------------------------------------------------------------------------
// The loop line and the loop's progress
// ---------------------------------------------------------------------------------------------

/// The loop line of a card `width` cells wide: the badge, then [`twr_core::loops::line`]
/// (`REVIEW LOOP  high · ladder m3 · ledger 2`). Where that does not fit it degrades in steps
/// ([`twr_core::loops::line_forms`]): first without the profile (`ladder m3 · ledger 2`),
/// then short (`m3 · ldg 2`), and only then cut with `…`, from the end, so where the loop is
/// survives longest.
pub fn card_line(l: &LoopView, width: usize) -> Line<'static> {
    let width = width.max(1);
    let badge = kind_badge(l.kind);
    let room = width.saturating_sub(crate::edit::width(&badge.content) + 1);
    let forms = twr_core::loops::line_forms(l);
    let text = forms
        .iter()
        .find(|f| crate::edit::width(f) <= room)
        .unwrap_or(&forms[2])
        .clone();
    // The text in the terminal's own color: a light grey would not read on a light terminal.
    let spans = vec![badge, Span::raw(" "), Span::raw(text)];
    Line::from(crate::ui::cut_spans(spans, width))
}

/// Where a loop is, in a word: its round in the ladder and the verification (`m3`, `v1`),
/// else its phase, short (`scope`, `sweep`; an implementation loop's `plan`, `p1`, `impl`; the
/// sweep's `rules` and `aud 2/5` when the hub audits it).
pub fn where_word(l: &LoopView) -> String {
    match (l.phase, &l.round) {
        (Phase::Ladder | Phase::Verify | Phase::PlanCheck, Some(r)) => r.clone(),
        (Phase::Implement, _) => "impl".into(),
        (Phase::Sweep, _) if l.wait == Wait::Rules => "rules".into(),
        (Phase::Sweep, _) if l.wait == Wait::Audits => {
            let done = l.audits.iter().filter(|a| a.status.ended()).count();
            format!("aud {done}/{}", l.audits.len())
        }
        (p, _) => p.as_str().to_string(),
    }
}

/// The loop line of a one-row card: the badge, where the loop is and the thread's state,
/// `…` saying the rest is cut (`REVIEW LOOP m3 · running…`).
pub fn short_line(l: &LoopView, state: &str, width: usize) -> Line<'static> {
    let mut text = format!(" {} · {state}", where_word(l));
    text.push('…');
    let spans = vec![kind_badge(l.kind), Span::raw(text)];
    Line::from(crate::ui::cut_spans(spans, width.max(1)))
}

/// Where phase `p` comes in a loop, to tell the phases a loop is past.
fn phase_order(p: Phase) -> u8 {
    match p {
        Phase::Setup => 0,
        Phase::Plan | Phase::Scope => 1,
        Phase::PlanCheck => 2,
        Phase::Implement => 3,
        Phase::Ladder => 4,
        Phase::Sweep => 5,
        Phase::Verify => 6,
        Phase::Report => 7,
    }
}

/// Whether an implementation loop passed its plan check over: it is past it and ran no
/// round `p1` (the plan said no contract risk under `contracts`, or the setting is `never`).
fn plan_check_skipped(l: &LoopView) -> bool {
    phase_order(l.phase) > phase_order(Phase::PlanCheck)
        && !l
            .rounds
            .iter()
            .any(|r| r.id == twr_core::loops::PLAN_CHECK_ROUND)
}

/// The phases of a loop, as its progress lists them (the sweep only when its profile sweeps),
/// each with whether the loop passed it over without running it: an implementation loop's
/// plan check when it did not run, the sweep once the ladder ended without one (it fixed
/// nothing, or skipped to verify), the verification when the ladder's end skipped it
/// (`verify_skipped`: v1 could only repeat the last round). A review loop starts with its
/// scope; an implementation loop with its plan, plan check and implement phases.
fn phases(l: &LoopView) -> Vec<(Phase, bool)> {
    let t = l.transition.as_ref();
    let swept = l.plan.sweep
        && l.fixed > 0
        && t.is_some_and(|t| t.reason != twr_core::loops::TransitionReason::Skipped);
    let mut out = match l.kind {
        LoopKind::Review => vec![(Phase::Scope, false)],
        LoopKind::Implement => vec![
            (Phase::Plan, false),
            (Phase::PlanCheck, plan_check_skipped(l)),
            (Phase::Implement, false),
        ],
    };
    out.push((Phase::Ladder, false));
    if l.plan.sweep || l.phase == Phase::Sweep {
        out.push((
            Phase::Sweep,
            t.is_some() && !swept && l.phase != Phase::Sweep,
        ));
    }
    out.push((Phase::Verify, t.is_some_and(|t| t.verify_skipped)));
    out.push((Phase::Report, false));
    out
}

/// Why `skip to verify` does nothing now, or `None` when it does: before an implementation
/// loop has implemented its plan, and once a loop is past its ladder (as the hub refuses it).
pub fn skip_refused(l: &LoopView) -> Option<&'static str> {
    match l.phase {
        Phase::Plan | Phase::PlanCheck | Phase::Implement => {
            Some("the loop has not implemented its plan yet: skip to verify comes after it")
        }
        Phase::Scope | Phase::Ladder if l.skip => Some("the loop already skips to verify"),
        Phase::Scope | Phase::Ladder => None,
        _ => Some("the loop is past its ladder"),
    }
}

/// Phase `p` as a progress lists it: `verify skipped` when the loop passed it over.
fn phase_name(p: Phase, skipped: bool) -> String {
    if skipped {
        format!("{} skipped", p.as_str())
    } else {
        p.as_str().to_string()
    }
}

/// The rounds of a loop in a line: each answered one with its findings and how many were
/// fixed as a new class (`m1 5(3)`), a failed one `m2 ✗`, the one going on marked `▸`
/// (`m3 ▸` while its review runs, `m3 2 ▸` while it is triaged).
pub fn rounds_text(l: &LoopView) -> String {
    let current = l.round.as_deref();
    l.rounds
        .iter()
        .map(|r| {
            let now = Some(r.id.as_str()) == current;
            match (&l.wait, now) {
                (Wait::Review { .. }, true) if r.failed => format!("{} ✗ ▸", r.id),
                (Wait::Review { .. }, true) => format!("{} ▸", r.id),
                (Wait::Triage { .. }, true) => format!("{} {} ▸", r.id, r.findings),
                _ if r.failed => format!("{} ✗", r.id),
                _ => format!("{} {}({})", r.id, r.findings, r.new_class),
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The sweep's audits in a line, when the hub audits it (`sweep = "runs"`): each rule's run with
/// its findings once it answered (`s1 identity-gating 2`), the ones running marked `▸`, a
/// failed one about to run again `✗ ▸`, one that did not run `✗`, one waiting for a slot `…`.
/// Empty when there are none.
pub fn audits_text(l: &LoopView) -> String {
    use twr_core::loops::AuditStatus;
    l.audits
        .iter()
        .map(|a| {
            let what = match a.status {
                AuditStatus::Answered => a.findings.to_string(),
                AuditStatus::Running if a.retried => "✗ ▸".into(),
                AuditStatus::Running => "▸".into(),
                AuditStatus::NotRun => "✗ not run".into(),
                AuditStatus::Pending if a.retried => "✗ …".into(),
                AuditStatus::Pending => "…".into(),
            };
            format!("{} {} {what}", a.id(), a.name)
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A loop's progress `width` cells wide, for its thread's action popup: the loop line, its
/// phases with the current one marked (`▸`), its rounds ([`rounds_text`]), what it waits
/// for, and why it is stuck when it is.
pub fn progress_rows(l: &LoopView, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let mut rows = vec![card_line(l, width)];
    let mut spans = Vec::new();
    for (k, (p, skipped)) in phases(l).into_iter().enumerate() {
        if k > 0 {
            spans.push(Span::styled(" · ", dim));
        }
        if p == l.phase {
            spans.push(Span::styled(
                format!("▸{}", p.as_str()),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(phase_name(p, skipped), dim));
        }
    }
    rows.extend(crate::ui::wrap_line(Line::from(spans), width));
    let rounds = rounds_text(l);
    if !rounds.is_empty() {
        rows.extend(crate::ui::wrap_line(
            Line::from(vec![Span::styled("rounds ", dim), Span::raw(rounds)]),
            width,
        ));
    }
    let audits = audits_text(l);
    if !audits.is_empty() {
        rows.extend(crate::ui::wrap_line(
            Line::from(vec![Span::styled("audits ", dim), Span::raw(audits)]),
            width,
        ));
    }
    let waits = match (&l.stuck, l.waiting_on) {
        (Some(_), _) => None,
        (None, twr_core::loops::WaitingOn::Review) if l.wait == Wait::Audits => Some(format!(
            "waiting on the audits: {} {}",
            l.reviewer.model, l.plan.audit_effort
        )),
        (None, twr_core::loops::WaitingOn::Review) => Some(format!(
            "waiting on the reviewer: {} {}",
            l.reviewer.model,
            l.reviewer.effort.as_deref().unwrap_or_default()
        )),
        (None, twr_core::loops::WaitingOn::Thread) => Some("waiting on the thread".into()),
        (None, twr_core::loops::WaitingOn::User) => Some("waiting on you".into()),
    };
    if let Some(w) = waits {
        rows.extend(crate::ui::wrap_line(
            Line::from(Span::styled(w.trim_end().to_string(), dim)),
            width,
        ));
    }
    if let Some(why) = &l.stuck {
        rows.extend(crate::ui::wrap_line(
            Line::from(Span::styled(
                format!("stuck: {why}"),
                Style::default().fg(Color::Red),
            )),
            width,
        ));
    }
    rows
}

/// `/loop status` in the status bar: where thread `name`'s loop is (its line and rounds),
/// or how its last one ended (with what it committed, [`commit_words`]).
pub fn status_text(t: &twr_core::reduce::ThreadView) -> String {
    if let Some(l) = &t.r#loop {
        let mut out = format!(
            "{}: {} {} · {}",
            t.name,
            twr_core::loops::badge(l.kind),
            twr_core::loops::line(l),
            phase_words(l)
        );
        let rounds = rounds_text(l);
        if !rounds.is_empty() {
            out.push_str(&format!(" · rounds {rounds}"));
        }
        let audits = audits_text(l);
        if !audits.is_empty() {
            out.push_str(&format!(" · audits {audits}"));
        }
        if let Some(why) = &l.stuck {
            out.push_str(&format!(" · stuck: {why}"));
        }
        return out;
    }
    match &t.last_loop {
        Some(last) => {
            let mut out = format!(
                "{} runs no loop; its last {} ended {}",
                t.name,
                kind_words(last.kind),
                outcome_words(last.outcome)
            );
            if let Some(c) = commit_words(last, true) {
                out.push_str(&format!("; {c}"));
            }
            out
        }
        None => format!("{} runs no loop (/implement or /review starts one)", t.name),
    }
}

/// A commit's short sha (its first 7 digits).
pub fn short_sha(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// What an ended loop committed, in words: `committed abc1234` (with each repository's name
/// when there are several: `committed abc1234 in api, def5678 in web`), then, when its
/// commit's check failed twice for the other repositories, what was not (`not committed:
/// why` with `why`; without, `not committed`, or `not committed in web` after commits): a
/// loop that committed something never reads `not committed` alone. `None` when it
/// committed nothing and was asked for no commit.
pub fn commit_words(last: &twr_core::reduce::LastLoop, why: bool) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(c) = commits_words(&last.commits) {
        parts.push(c);
    }
    if let Some(problem) = &last.commit_problem {
        parts.push(if why {
            format!("not committed: {}", first_line(problem))
        } else {
            not_committed_short(problem, !last.commits.is_empty())
        });
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The commits a check found, in words (`committed abc1234`, `committed abc1234 in api,
/// def5678 in web`); `None` for none.
fn commits_words(commits: &[twr_core::loops::contract::Commit]) -> Option<String> {
    match commits {
        [] => None,
        [c] => Some(format!("committed {}", short_sha(&c.sha))),
        all => Some(format!(
            "committed {}",
            all.iter()
                .map(|c| format!("{} in {}", short_sha(&c.sha), repo_name(&c.repo)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// A commit problem without its why: `not committed`; after commits, which repositories
/// were not (`not committed in web`, from the paths each problem starts with), so it never
/// reads as if nothing was.
fn not_committed_short(problem: &str, after_commits: bool) -> String {
    if !after_commits {
        return "not committed".into();
    }
    let repos: Vec<&str> = problem
        .split("; ")
        .filter_map(|p| p.split_once(": ").map(|(repo, _)| repo))
        .filter(|repo| repo.contains('/') || repo.contains('\\'))
        .map(repo_name)
        .collect();
    if repos.is_empty() {
        "not all committed".into()
    } else {
        format!("not committed in {}", repos.join(", "))
    }
}

/// A repository's name: the last part of its path.
fn repo_name(repo: &str) -> &str {
    repo.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or(repo)
}

/// Whether thread `t`'s card says how its last loop ended ([`ended_line`]): it runs no loop
/// now, and no turn of the thread's started since that one ended.
pub fn ended_due(t: &twr_core::reduce::ThreadView) -> Option<&twr_core::reduce::LastLoop> {
    let last = t.last_loop.as_ref()?;
    (t.r#loop.is_none()
        && t.current_turn.is_none()
        && last.ended_turn.is_some()
        && last.ended_turn == t.last_turn.as_ref().map(|l| l.turn))
    .then_some(last)
}

/// The line a card shows in its loop line's place once the loop ended, until the thread's
/// next turn: the badge, how it ended and what it committed (`IMPLEMENT LOOP  ended clean ·
/// committed abc1234`, `ended clean · not committed`, `ended clean · committed abc1234 · not
/// committed in web`). Too narrow, it drops `ended`, then shortens the commit to its sha
/// (`clean · abc1234`, `clean · abc1234 · not all committed`), then is cut with `…`.
pub fn ended_line(last: &twr_core::reduce::LastLoop, width: usize) -> Line<'static> {
    let width = width.max(1);
    let badge = kind_badge(last.kind);
    let room = width.saturating_sub(crate::edit::width(&badge.content) + 1);
    let outcome = outcome_short(last.outcome);
    let commit = commit_words(last, false);
    let short_commit = match (&last.commit_problem, last.commits.as_slice()) {
        (Some(_), []) => Some("not committed".to_string()),
        (Some(_), [c, ..]) => Some(format!("{} · not all committed", short_sha(&c.sha))),
        (None, []) => None,
        (None, [c, ..]) => Some(short_sha(&c.sha).to_string()),
    };
    let join =
        |parts: [Option<String>; 2]| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
    let forms = [
        join([Some(format!("ended {outcome}")), commit.clone()]),
        join([Some(outcome.to_string()), commit]),
        join([Some(outcome.to_string()), short_commit]),
    ];
    let text = forms
        .iter()
        .find(|f| crate::edit::width(f) <= room)
        .unwrap_or(&forms[2])
        .clone();
    let spans = vec![badge, Span::raw(" "), Span::raw(text)];
    Line::from(crate::ui::cut_spans(spans, width))
}

/// How a loop ended, in a word or two, as a card says it (`clean`, `cap expired`).
fn outcome_short(o: twr_core::loops::Outcome) -> &'static str {
    use twr_core::loops::Outcome;
    match o {
        Outcome::Clean => "clean",
        Outcome::LedgerFixedPoint => "at a ledger fixed point",
        Outcome::CapExpired => "cap expired",
        Outcome::Stopped => "stopped",
        Outcome::Failed => "failed",
    }
}

/// A loop's phases in a line, the current one marked: `scope · ▸ladder · sweep · verify ·
/// report`.
fn phase_words(l: &LoopView) -> String {
    phases(l)
        .into_iter()
        .map(|(p, skipped)| {
            if p == l.phase {
                format!("▸{}", p.as_str())
            } else {
                phase_name(p, skipped)
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// How a loop ended, in words.
pub fn outcome_words(o: twr_core::loops::Outcome) -> &'static str {
    use twr_core::loops::Outcome;
    match o {
        Outcome::Clean => "clean",
        Outcome::LedgerFixedPoint => "at a ledger fixed point",
        Outcome::CapExpired => "with its verification cap spent",
        Outcome::Stopped => "stopped",
        Outcome::Failed => "failed",
    }
}

// ---------------------------------------------------------------------------------------------
// The transcript
// ---------------------------------------------------------------------------------------------

/// The loop and step a loop's prompt names on its first line (`[twr loop <id> <step>]`, a
/// correction's `… correction` kept: `triage m3 correction`); `None` for another prompt.
pub fn prompt_step(prompt: &str) -> Option<(String, String)> {
    let rest = prompt.strip_prefix(twr_core::event::LOOP_PROMPT_START)?;
    let line = rest.lines().next()?;
    let inner = &line[..line.find(']')?];
    let (id, step) = inner.split_once(' ')?;
    let step = step.trim();
    (!id.is_empty() && !step.is_empty()).then(|| (id.to_string(), step.to_string()))
}

/// Whether a loop prompt's step is one only an implementation loop has: its plan, the plan
/// check's triage (`triage p1`), its implement turn and its commit's correction.
pub fn implementation_step(step: &str) -> bool {
    let step = step.strip_suffix(" correction").unwrap_or(step);
    matches!(step, "plan" | "implement" | "commit")
        || step == format!("triage {}", twr_core::loops::PLAN_CHECK_ROUND)
}

/// The folded row's text of a turn a loop's prompt started: `review loop · triage m3`,
/// `implementation loop · plan`.
pub fn prompt_marker(step: &str, kind: LoopKind) -> String {
    format!("{} · {step}", kind_words(kind))
}

/// How long a review run took, as a round's row says it (`14m`, `40s`).
pub fn took_words(ms: i64) -> String {
    twr_core::work::ago(ms.max(0) / 1000)
}

/// A round's folded row's text (`review round m3 · gpt-6.1-sol medium · 14m · 2 findings`; the
/// plan check's `plan check p1 · …`; a failed run's `review round m3 · failed: <why>`), with
/// how the thread triaged it once it did. `took`: how long its run took, when known.
pub fn round_head(
    round: &str,
    reviewer: Option<&str>,
    took: Option<&str>,
    findings: &[Finding],
    failed: Option<&str>,
    triage: &[TriageItem],
) -> String {
    let what = if round == twr_core::loops::PLAN_CHECK_ROUND {
        "plan check"
    } else {
        "review round"
    };
    let mut parts = vec![format!("{what} {round}")];
    if let Some(r) = reviewer.filter(|r| !r.is_empty()) {
        parts.push(r.to_string());
    }
    if let Some(t) = took.filter(|t| !t.is_empty()) {
        parts.push(t.to_string());
    }
    if let Some(why) = failed {
        parts.push(format!("failed: {}", first_line(why)));
        return parts.join(" · ");
    }
    let n = findings.len();
    parts.push(match n {
        0 => "no findings".to_string(),
        1 => "1 finding".to_string(),
        n => format!("{n} findings"),
    });
    if !triage.is_empty() {
        let count = |v: Verdict| triage.iter().filter(|t| t.verdict == v).count();
        let mut said = Vec::new();
        for (v, word) in [
            (Verdict::Fixed, "fixed"),
            (Verdict::Dismissed, "dismissed"),
            (Verdict::Duplicate, "duplicate"),
        ] {
            let k = count(v);
            if k > 0 {
                said.push(format!("{k} {word}"));
            }
        }
        parts.push(said.join(", "));
    }
    parts.join(" · ")
}

/// An implementation loop's step the transcript folds to one row ([`step_head`]) that opens
/// to its details ([`step_rows`]): its plan, its implement turn, the hub's check of its
/// commit.
#[derive(Debug, Clone, PartialEq)]
pub enum StepNote {
    /// `loop.plan`: the plan the thread wrote (revision 1) or the plan check's revision (2).
    Plan(twr_core::event::LoopPlanned),
    /// `loop.implemented`: the implement turn's `twr-implement` block.
    Implemented(twr_core::event::LoopImplemented),
    /// `loop.commit`: the hub's check of the loop's commit.
    Commit(twr_core::event::LoopCommitChecked),
    /// `loop.rules`: the rules the sweep's audit runs check (`sweep = "runs"`, either kind
    /// of loop).
    Rules(twr_core::event::LoopRules),
    /// `loop.audit`: a rule's audit run answered, or failed.
    Audit(twr_core::event::LoopAudit),
}

impl StepNote {
    /// Whether only an implementation loop has a step of this kind (its plan, implement turn,
    /// commit): the sweep's rules and audits are either kind's.
    pub fn implementation_only(&self) -> bool {
        matches!(
            self,
            StepNote::Plan(_) | StepNote::Implemented(_) | StepNote::Commit(_)
        )
    }
}

/// A step's folded row's text: `plan · revision 1 · contract risk: no`, `implement · gates
/// pass` (`implement · partial · gates fail`), `committed abc1234` (one per repository,
/// `committed abc1234 in api, def5678 in web`) or `not committed: <why>`, both when some
/// repositories were committed and others not (`committed abc1234 · not committed: <why>`).
pub fn step_head(n: &StepNote) -> String {
    match n {
        StepNote::Plan(p) => format!(
            "plan · revision {} · contract risk: {}",
            p.revision,
            if p.contract_risk { "yes" } else { "no" }
        ),
        StepNote::Implemented(i) => {
            let gates = match i.gates.result {
                twr_core::loops::contract::GateResult::Pass => "gates pass",
                twr_core::loops::contract::GateResult::Fail => "gates fail",
            };
            match i.status {
                twr_core::loops::contract::ImplementStatus::Done => format!("implement · {gates}"),
                s => format!("implement · {} · {gates}", s.as_str()),
            }
        }
        StepNote::Commit(c) => {
            let problem = c
                .problems
                .first()
                .map(|p| format!("not committed: {}", first_line(p)));
            match (commits_words(&c.commits), problem) {
                (Some(done), Some(not)) => format!("{done} · {not}"),
                (Some(done), None) => done,
                (None, Some(not)) => not,
                (None, None) => "committed nothing: there was nothing to commit".into(),
            }
        }
        StepNote::Rules(r) if r.rules.is_empty() => "sweep rules · none to audit".into(),
        StepNote::Rules(r) => format!(
            "sweep rules · {}",
            r.rules
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        StepNote::Audit(a) => {
            let head = format!(
                "sweep audit {} {}",
                twr_core::loops::contract::audit_id(a.rule),
                a.name
            );
            if a.failed {
                return format!(
                    "{head} · failed: {}",
                    first_line(a.reason.as_deref().unwrap_or("the run failed"))
                );
            }
            match a.findings.len() {
                0 => format!("{head} · no findings"),
                1 => format!("{head} · 1 finding"),
                n => format!("{head} · {n} findings"),
            }
        }
    }
}

/// A step's row opened: the plan's file and why it named its contract risk; the gates the
/// implement turn ran; each repository's commit, then every problem of the check, then what
/// it noted that is no problem (`note: …`).
pub fn step_rows(n: &StepNote) -> Vec<String> {
    match n {
        StepNote::Plan(p) => {
            let mut out = Vec::new();
            if let Some(path) = p.path.as_deref().filter(|p| !p.is_empty()) {
                out.push(format!("plan.md: {path}"));
            }
            if !p.reason.trim().is_empty() {
                out.push(format!("contract risk: {}", first_line(&p.reason)));
            }
            out
        }
        StepNote::Implemented(i) if i.gates.ran.is_empty() => vec!["no gates named".into()],
        StepNote::Implemented(i) => i.gates.ran.iter().map(|g| format!("ran {g}")).collect(),
        StepNote::Commit(c) => c
            .commits
            .iter()
            .map(|c| format!("{} {}", c.repo, short_sha(&c.sha)))
            .chain(c.problems.iter().cloned())
            .chain(c.notes.iter().map(|n| format!("note: {}", first_line(n))))
            .collect(),
        StepNote::Rules(r) => r
            .rules
            .iter()
            .map(|r| {
                format!(
                    "{}: {} (where: {})",
                    r.name,
                    first_line(&r.rule),
                    first_line(&r.r#where)
                )
            })
            .collect(),
        StepNote::Audit(a) => a
            .findings
            .iter()
            .map(|f| {
                format!(
                    "{} {} {} — {}",
                    f.id,
                    f.severity.as_str(),
                    f.location,
                    first_line(&f.problem)
                )
            })
            .collect(),
    }
}

/// A step's row is in red: an implement turn that is not done with green gates, a commit
/// whose check failed, an audit run that failed.
pub fn step_failed(n: &StepNote) -> bool {
    match n {
        StepNote::Plan(_) => false,
        StepNote::Implemented(i) => {
            i.status != twr_core::loops::contract::ImplementStatus::Done
                || i.gates.result != twr_core::loops::contract::GateResult::Pass
        }
        StepNote::Commit(c) => !c.problems.is_empty(),
        StepNote::Rules(_) => false,
        StepNote::Audit(a) => a.failed,
    }
}

/// A round opened: each finding (`m3.1 major src/a.rs:4 — the problem's first line`), each
/// with the thread's verdict under it once it triaged them (`fixed (new-class,
/// identity-gating)`, `dismissed: why`, `duplicate of L4`), as `(text, verdict)` lines.
pub fn round_rows(findings: &[Finding], triage: &[TriageItem]) -> Vec<(String, Option<String>)> {
    findings
        .iter()
        .map(|f| {
            let head = format!(
                "{} {} {} — {}",
                f.id,
                f.severity.as_str(),
                f.location,
                first_line(&f.problem)
            );
            let verdict = triage.iter().find(|t| t.id == f.id).map(verdict_words);
            (head, verdict)
        })
        .collect()
}

/// A triage verdict in words.
fn verdict_words(t: &TriageItem) -> String {
    match t.verdict {
        Verdict::Fixed => {
            let mut what: Vec<&str> = Vec::new();
            what.extend(t.class.map(|c| c.as_str()));
            what.extend(t.rule.as_deref());
            if what.is_empty() {
                "fixed".into()
            } else {
                format!("fixed ({})", what.join(", "))
            }
        }
        Verdict::Dismissed => format!(
            "dismissed: {}",
            first_line(t.reason.as_deref().unwrap_or("-"))
        ),
        Verdict::Duplicate => format!("duplicate of {}", t.of.as_deref().unwrap_or("?")),
    }
}

/// The first line of `s`, trimmed.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or_default().trim()
}

// ---------------------------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------------------------

/// What the editor edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorFor {
    /// A setup card's draft: loaded with `loop.setup.get`, saved with `loop.setup.update` at
    /// the revision loaded (`None` until it loaded).
    Card {
        /// The card.
        attn_id: String,
        /// The revision loaded.
        revision: Option<u64>,
    },
    /// The saved settings (`loop.settings.get {project}`, `loop.settings.set`,
    /// `loop.settings.reset`).
    Saved,
}

/// A row of the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKey {
    /// The presets (and `custom`).
    Preset,
    /// The profile.
    Profile,
    /// The implementer: the thread's own, or named.
    Implementer,
    /// A named implementer's runtime.
    ImplRuntime,
    /// A named implementer's model.
    ImplModel,
    /// A named implementer's effort.
    ImplEffort,
    /// The workers' model.
    WorkersModel,
    /// The workers' effort.
    WorkersEffort,
    /// The reviewer's runtime.
    ReviewerRuntime,
    /// The reviewer's model.
    ReviewerModel,
    /// The reviewer's account, when its runtime lists several.
    ReviewerAccount,
    /// An implementation loop's plan check (`None`: the profile's).
    PlanCheck,
    /// What an implementation loop commits.
    ImplCommit,
    /// What a review loop commits.
    ReviewCommit,
    /// Who audits the sweep's rules: the thread, or the hub's runs (`sweep = "runs"`).
    Sweep,
    /// Where the settings are saved.
    SaveFor,
    /// Saves (`s`).
    Save,
    /// Forgets the saved settings (`y` to confirm); the saved settings only.
    Reset,
}

/// The editor's state while it is open.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoopEditor {
    /// What it edits.
    pub of: Option<EditorFor>,
    /// The project `this project` saves for (a card's thread's, `--project`'s, the thread
    /// whose popup opened it), if any.
    pub project: Option<String>,
    /// The project asked for with `loop.settings.get` (the saved settings: the set that
    /// applies to it).
    pub query: Option<String>,
    /// `loop.settings.get`'s answer: the presets, profiles and runtimes the rows cycle
    /// through; `None` until it came.
    pub got: Option<Value>,
    /// The draft; `None` until it loaded.
    pub draft: Option<LoopSettings>,
    /// The draft as loaded: a change since is unsaved.
    pub loaded: Option<LoopSettings>,
    /// Where it saves.
    pub save_for: SaveFor,
    /// The selected row.
    pub selected: Option<RowKey>,
    /// The hub's warnings, as it last returned them.
    pub warnings: Vec<String>,
    /// What the hub says must change first, as it last returned it.
    pub problems: Vec<String>,
    /// A call is on its way (`loading…`, `saving…`).
    pub busy: Option<&'static str>,
    /// What the last answer said (the card changed and was reloaded, the save failed).
    pub note: Option<String>,
    /// `Reset` asked `y`.
    pub confirm_reset: bool,
    /// The thread of the card (its runtime decides whether workers apply).
    pub thread: Option<String>,
    /// The card's profile, resolved (`setup.profile`): its summary.
    pub profile: Option<twr_core::loops::config::Profile>,
    /// The kind of loop the card is for (`setup.kind`); the saved settings serve both.
    pub kind: Option<LoopKind>,
}

impl LoopEditor {
    /// The runtimes as `loop.settings.get` found them.
    pub fn runtimes(&self) -> Vec<RuntimeAvailability> {
        self.got
            .as_ref()
            .and_then(|g| serde_json::from_value(g["runtimes"].clone()).ok())
            .unwrap_or_default()
    }

    /// The presets.
    pub fn presets(&self) -> Vec<Preset> {
        self.got
            .as_ref()
            .and_then(|g| serde_json::from_value(g["presets"].clone()).ok())
            .unwrap_or_default()
    }

    /// The profiles.
    pub fn profiles(&self) -> Vec<String> {
        self.got
            .as_ref()
            .and_then(|g| serde_json::from_value(g["profiles"].clone()).ok())
            .unwrap_or_default()
    }

    /// Whether it edits a setup card's draft.
    pub fn is_card(&self) -> bool {
        matches!(self.of, Some(EditorFor::Card { .. }))
    }

    /// Whether the draft changed since it loaded.
    pub fn changed(&self) -> bool {
        self.draft != self.loaded
    }
}

/// Whether runtime `rt` is installed and logged in, as `runtimes` found it, or why not. One
/// the hub does not probe for (`probed: false`) is usable: the hub does not know
/// ([`not_checked`]).
fn usable(runtimes: &[RuntimeAvailability], rt: &str) -> Result<(), &'static str> {
    match runtimes.iter().find(|r| r.runtime == rt) {
        None => Err("not found"),
        Some(r) if !r.probed => Ok(()),
        Some(r) if !r.found => Err("not installed"),
        Some(r) if !r.login => Err("not logged in"),
        Some(_) => Ok(()),
    }
}

/// Whether the hub did not check runtime `rt` (it does not probe its agent CLIs).
fn not_checked(runtimes: &[RuntimeAvailability], rt: &str) -> bool {
    runtimes.iter().any(|r| r.runtime == rt && !r.probed)
}

/// The preset `s` matches, if any.
fn preset_of<'a>(s: &LoopSettings, presets: &'a [Preset]) -> Option<&'a Preset> {
    presets.iter().find(|p| {
        s.implementer.as_ref() == Some(&p.implementer)
            && s.workers == p.workers
            && s.reviewer == p.reviewer
    })
}

/// A profile's one-line summary: a card's resolved one, or a built-in's; one of the config
/// file's says where it is.
pub fn profile_summary(name: &str, card: Option<&twr_core::loops::config::Profile>) -> String {
    if let Some(p) = card.filter(|p| p.name == name) {
        return p.summary();
    }
    match twr_core::loops::config::Profile::builtin(name) {
        Some(p) => p.summary(),
        None => format!("{name} · yours ([loops.profiles.{name}])"),
    }
}

/// The runtime the implementer runs on: a named one's, else the card's thread's (`thread`).
fn impl_runtime<'a>(s: &'a LoopSettings, thread: Option<&'a str>) -> Option<&'a str> {
    s.implementer
        .as_ref()
        .map(|m| m.runtime.as_str())
        .or(thread)
}

/// One row as drawn: what it sets, its label, its value, a dim line under it, whether a
/// change is possible.
pub struct Row {
    /// What it sets.
    pub key: RowKey,
    /// Its label.
    pub label: &'static str,
    /// Its value.
    pub value: String,
    /// A dim line under it, if any.
    pub detail: Option<String>,
    /// Its value is drawn dimmed (a runtime that is not usable).
    pub dim: bool,
}

/// The editor's rows for `e` (`thread_rt`: the card's thread's runtime, when known).
pub fn rows(e: &LoopEditor, thread_rt: Option<&str>) -> Vec<Row> {
    let Some(s) = e.draft.as_ref() else {
        return Vec::new();
    };
    let runtimes = e.runtimes();
    let presets = e.presets();
    // Runtimes found on a thread's remote box are named for it.
    let on = e
        .got
        .as_ref()
        .and_then(|g| g["box"].as_str())
        .map(|b| format!(" on box {b}"))
        .unwrap_or_default();
    let row = |key, label, value: String| Row {
        key,
        label,
        value,
        detail: None,
        dim: false,
    };
    let skipped = || {
        let out: Vec<String> = REVIEWER_RUNTIMES
            .iter()
            .filter_map(|rt| {
                usable(&runtimes, rt)
                    .err()
                    .map(|why| format!("{rt} ({why})"))
            })
            .collect();
        (!out.is_empty()).then(|| format!("skipped{on}: {}", out.join(", ")))
    };
    let runtime_value = |rt: &str| match usable(&runtimes, rt) {
        Ok(()) if not_checked(&runtimes, rt) => (format!("{rt} (not checked)"), false),
        Ok(()) => (rt.to_string(), false),
        Err(why) => (format!("{rt} ({why})"), true),
    };
    // The reviewer's choices also hold the ACP presets: one that reviews names its read-only
    // mode, one that can't says why.
    let reviewer_skipped = || {
        let mut out: Vec<String> = REVIEWER_RUNTIMES
            .iter()
            .filter_map(|rt| {
                usable(&runtimes, rt)
                    .err()
                    .map(|why| format!("{rt} ({why})"))
            })
            .collect();
        out.extend(runtimes.iter().filter(|r| r.acp).filter_map(|r| {
            match (r.reviews(), usable(&runtimes, &r.runtime)) {
                (Err(why), _) => Some(format!("{} ({why})", r.runtime)),
                (Ok(()), Err(why)) => Some(format!("{} ({why})", r.runtime)),
                _ => None,
            }
        }));
        (!out.is_empty()).then(|| format!("skipped{on}: {}", out.join(", ")))
    };
    let reviewer_value = |rt: &str| {
        let acp = runtimes.iter().find(|r| r.runtime == rt && r.acp);
        match acp {
            Some(r) => match (r.reviews(), usable(&runtimes, rt)) {
                (Err(why), _) => (format!("{rt} ({why})"), true),
                (Ok(()), Err(why)) => (format!("{rt} ({why})"), true),
                (Ok(()), Ok(())) => (
                    format!(
                        "{rt} (acp, {} mode)",
                        r.review_mode.as_deref().unwrap_or("read-only")
                    ),
                    false,
                ),
            },
            None => runtime_value(rt),
        }
    };
    let mut out = Vec::new();
    out.push(row(
        RowKey::Preset,
        "Preset",
        preset_of(s, &presets).map_or_else(|| "custom".to_string(), |p| p.label.clone()),
    ));
    out.push(Row {
        detail: Some(profile_summary(&s.profile, e.profile.as_ref())),
        ..row(RowKey::Profile, "Profile", s.profile.clone())
    });
    match &s.implementer {
        None => out.push(Row {
            detail: thread_rt.map(|rt| format!("the thread keeps its model (on {rt})")),
            ..row(
                RowKey::Implementer,
                "Implementer",
                "the thread's own".into(),
            )
        }),
        Some(m) => {
            out.push(row(RowKey::Implementer, "Implementer", "named".into()));
            let (value, dim) = runtime_value(&m.runtime);
            out.push(Row {
                detail: skipped(),
                dim,
                ..row(RowKey::ImplRuntime, "  runtime", value)
            });
            out.push(row(RowKey::ImplModel, "  model", m.model.clone()));
            out.push(row(
                RowKey::ImplEffort,
                "  effort",
                m.effort.clone().unwrap_or_else(|| "default".into()),
            ));
        }
    }
    let workers = match impl_runtime(s, thread_rt) {
        Some(rt) => runtimes.iter().any(|r| r.runtime == rt && r.subagents),
        // The thread's own model, no thread known (the saved settings): they may apply.
        None => true,
    };
    if workers {
        out.push(row(
            RowKey::WorkersModel,
            "Workers",
            s.workers.model.clone(),
        ));
        out.push(row(
            RowKey::WorkersEffort,
            "  effort",
            s.workers.effort.clone().unwrap_or_else(|| "default".into()),
        ));
    }
    let (value, dim) = reviewer_value(&s.reviewer.runtime);
    out.push(Row {
        detail: reviewer_skipped(),
        dim,
        ..row(RowKey::ReviewerRuntime, "Reviewer", value)
    });
    out.push(Row {
        detail: Some("its effort is each round's, from the profile".into()),
        ..row(RowKey::ReviewerModel, "  model", s.reviewer.model.clone())
    });
    let accounts = runtimes
        .iter()
        .find(|r| r.runtime == s.reviewer.runtime)
        .map(|r| r.accounts.clone())
        .unwrap_or_default();
    if !accounts.is_empty() {
        out.push(row(
            RowKey::ReviewerAccount,
            "  account",
            s.reviewer
                .account
                .clone()
                .unwrap_or_else(|| "the runtime's default".into()),
        ));
    }
    // The implementation loop's rows: on its card and on the saved settings, which serve
    // both kinds of loop; a review loop's card leaves them out.
    if e.kind != Some(LoopKind::Review) {
        let profile_check = e
            .profile
            .as_ref()
            .filter(|p| p.name == s.profile)
            .cloned()
            .or_else(|| twr_core::loops::config::Profile::builtin(&s.profile))
            .map(|p| p.plan_check);
        out.push(Row {
            detail: Some(
                match s.plan_check.or(profile_check) {
                    Some(PlanCheck::Contracts) => {
                        "a review of the plan when it touches money, schemas or cross-repo contracts"
                    }
                    Some(PlanCheck::Always) => "a review of the plan, always",
                    Some(PlanCheck::Never) => "no review of the plan",
                    None => "as the profile says",
                }
                .into(),
            ),
            ..row(
                RowKey::PlanCheck,
                "Plan check",
                match (s.plan_check, profile_check) {
                    (Some(v), _) => v.as_str().to_string(),
                    (None, Some(v)) => format!("the profile's ({})", v.as_str()),
                    (None, None) => "the profile's".into(),
                },
            )
        });
        out.push(Row {
            detail: Some(
                match s.commit {
                    ImplCommit::End => "an implementation loop commits its work once, at the end",
                    ImplCommit::PerRound => {
                        "commit after implementing and after every round that fixed something"
                    }
                    ImplCommit::None => "an implementation loop leaves its work uncommitted",
                }
                .into(),
            ),
            ..row(RowKey::ImplCommit, "Commit", s.commit.as_str().into())
        });
    }
    // A review loop's commit: on its card and on the saved settings.
    if e.kind != Some(LoopKind::Implement) {
        out.push(Row {
            detail: Some(
                match s.review_commit {
                    ReviewCommit::Match => "commit the fixes if the thread commits its work",
                    ReviewCommit::None => "leave the fixes uncommitted",
                }
                .into(),
            ),
            ..row(
                RowKey::ReviewCommit,
                "Review commits",
                s.review_commit.as_str().into(),
            )
        });
    }
    out.push(Row {
        detail: Some(
            match s.sweep {
                SweepMode::Thread => {
                    "the thread audits the sweep's rules, with its subagents where it has them"
                }
                SweepMode::Runs => {
                    "one read-only reviewer run per rule, a few at once: one reviewer call per rule"
                }
            }
            .into(),
        ),
        ..row(
            RowKey::Sweep,
            "Sweep",
            match s.sweep {
                SweepMode::Thread => "thread",
                SweepMode::Runs => "runs",
            }
            .into(),
        )
    });
    out.push(row(
        RowKey::SaveFor,
        "Save for",
        match (e.save_for, &e.project) {
            (SaveFor::Project, Some(p)) => format!("this project ({p})"),
            _ => "all projects".into(),
        },
    ));
    out.push(Row {
        detail: Some(if e.is_card() {
            "the card shows these; 1 on it then starts the loop and saves them".into()
        } else if e.changed() {
            "unsaved changes".into()
        } else {
            "nothing changed yet".into()
        }),
        ..row(
            RowKey::Save,
            "Save",
            if e.is_card() {
                "save to the card (s)".into()
            } else {
                "save (s)".into()
            },
        )
    });
    if !e.is_card() {
        out.push(Row {
            detail: Some(match &e.query {
                Some(p) => format!("{p} uses the workspace's settings again"),
                None => "forget every saved set: the next loop shows its setup card".into(),
            }),
            ..row(
                RowKey::Reset,
                "Reset",
                if e.confirm_reset {
                    "y to reset, any other key keeps them".into()
                } else {
                    "reset…".into()
                },
            )
        });
    }
    out
}

/// The model to take on runtime `rt`: `keep` if `rt` reports it (or reports none), else
/// twr's default for it, else the first it reports.
fn model_on(runtimes: &[RuntimeAvailability], rt: &str, keep: &str) -> String {
    let models = runtimes
        .iter()
        .find(|r| r.runtime == rt)
        .map(|r| r.models.clone())
        .unwrap_or_default();
    if !keep.is_empty() && (models.is_empty() || models.iter().any(|m| m == keep)) {
        return keep.to_string();
    }
    default_model(rt)
        .map(|(m, _)| m.to_string())
        .filter(|m| models.is_empty() || models.contains(m))
        .or_else(|| models.first().cloned())
        .unwrap_or_else(|| keep.to_string())
}

/// The next (`forward`) or previous of `all` after `cur` (the first or last when `cur` is not
/// in it).
fn cycle<T: Clone + PartialEq>(all: &[T], cur: &T, forward: bool) -> Option<T> {
    let n = all.len();
    if n == 0 {
        return None;
    }
    let next = match all.iter().position(|x| x == cur) {
        Some(i) if forward => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None if forward => 0,
        None => n - 1,
    };
    Some(all[next].clone())
}

/// The usable runtimes of `runtimes`, in [`REVIEWER_RUNTIMES`]' order.
fn usable_runtimes(runtimes: &[RuntimeAvailability]) -> Vec<String> {
    REVIEWER_RUNTIMES
        .iter()
        .filter(|rt| usable(runtimes, rt).is_ok())
        .map(|rt| rt.to_string())
        .collect()
}

/// The runtimes a reviewer can be on: the usable built-in ones, then the usable ACP presets
/// that review (they name a read-only mode), in the hub's order.
fn reviewer_runtimes(runtimes: &[RuntimeAvailability]) -> Vec<String> {
    let mut out = usable_runtimes(runtimes);
    out.extend(
        runtimes
            .iter()
            .filter(|r| r.acp && r.reviews().is_ok() && usable(runtimes, &r.runtime).is_ok())
            .map(|r| r.runtime.clone()),
    );
    out
}

/// Steps row `key` of `e` (`forward`: Enter, Space, →, a tap; else ←): the draft changed, or
/// why it could not be (for the status bar). `thread_rt`: the card's thread's runtime.
pub fn step(
    e: &mut LoopEditor,
    key: RowKey,
    forward: bool,
    thread_rt: Option<&str>,
) -> Result<(), String> {
    let runtimes = e.runtimes();
    let presets = e.presets();
    let profiles = e.profiles();
    let has_project = e.project.is_some();
    let Some(s) = e.draft.as_mut() else {
        return Err("the settings are still loading".into());
    };
    let efforts_of = |rt: &str| -> Vec<String> {
        runtimes
            .iter()
            .find(|r| r.runtime == rt)
            .map(|r| r.efforts.clone())
            .unwrap_or_default()
    };
    let models_of = |rt: &str| -> Vec<String> {
        runtimes
            .iter()
            .find(|r| r.runtime == rt)
            .map(|r| r.models.clone())
            .unwrap_or_default()
    };
    // An effort or none (`default`), cycled.
    let effort_step = |efforts: Vec<String>, cur: &Option<String>| -> Option<Option<String>> {
        let mut all: Vec<Option<String>> = vec![None];
        all.extend(efforts.into_iter().map(Some));
        if all.len() == 1 {
            return None;
        }
        cycle(&all, cur, forward)
    };
    let usable_rts = usable_runtimes(&runtimes);
    match key {
        RowKey::Preset => {
            // Presets whose runtimes are all usable; one that is not is skipped.
            let ok: Vec<&Preset> = presets
                .iter()
                .filter(|p| {
                    usable(&runtimes, &p.implementer.runtime).is_ok()
                        && usable(&runtimes, &p.reviewer.runtime).is_ok()
                })
                .collect();
            let ids: Vec<String> = ok.iter().map(|p| p.id.clone()).collect();
            let cur = preset_of(s, &presets)
                .map(|p| p.id.clone())
                .unwrap_or_default();
            let next = cycle(&ids, &cur, forward)
                .ok_or("no preset's runtimes are installed and logged in")?;
            let p = ok
                .iter()
                .find(|p| p.id == next)
                .expect("a preset of the list");
            *s = s.with_preset(p);
        }
        RowKey::Profile => {
            s.profile = cycle(&profiles, &s.profile, forward).ok_or("no profiles to pick")?;
        }
        RowKey::Implementer => {
            if s.implementer.is_some() {
                s.implementer = None;
            } else {
                let rt = thread_rt
                    .filter(|rt| usable_rts.iter().any(|u| u == rt))
                    .map(str::to_string)
                    .or_else(|| usable_rts.first().cloned())
                    .ok_or("no runtime is installed and logged in")?;
                let (model, effort) = default_model(&rt).unwrap_or(("", "high"));
                let model = model_on(&runtimes, &rt, model);
                let effort = Some(effort.to_string()).filter(|e| efforts_of(&rt).contains(e));
                s.implementer = Some(ModelChoice {
                    runtime: rt,
                    account: None,
                    model,
                    effort,
                });
            }
        }
        RowKey::ImplRuntime => {
            let m = s
                .implementer
                .as_mut()
                .ok_or("the implementer is the thread's own")?;
            let rt = cycle(&usable_rts, &m.runtime, forward)
                .ok_or("no runtime is installed and logged in")?;
            m.model = model_on(&runtimes, &rt, &m.model);
            if m.effort
                .as_ref()
                .is_some_and(|e| !efforts_of(&rt).contains(e))
            {
                m.effort = default_model(&rt)
                    .map(|(_, e)| e.to_string())
                    .filter(|e| efforts_of(&rt).contains(e));
            }
            m.account = None;
            m.runtime = rt;
        }
        RowKey::ImplModel => {
            let m = s
                .implementer
                .as_mut()
                .ok_or("the implementer is the thread's own")?;
            m.model = cycle(&models_of(&m.runtime), &m.model, forward)
                .ok_or_else(|| format!("{} reports no models to pick", m.runtime))?;
        }
        RowKey::ImplEffort => {
            let m = s
                .implementer
                .as_mut()
                .ok_or("the implementer is the thread's own")?;
            m.effort = effort_step(efforts_of(&m.runtime), &m.effort)
                .ok_or_else(|| format!("{} takes no efforts", m.runtime))?;
        }
        RowKey::WorkersModel => {
            let rt = impl_runtime(s, thread_rt)
                .unwrap_or("claude-code")
                .to_string();
            s.workers.model = cycle(&models_of(&rt), &s.workers.model, forward)
                .ok_or_else(|| format!("{rt} reports no models to pick"))?;
        }
        RowKey::WorkersEffort => {
            let rt = impl_runtime(s, thread_rt)
                .unwrap_or("claude-code")
                .to_string();
            s.workers.effort = effort_step(efforts_of(&rt), &s.workers.effort)
                .ok_or_else(|| format!("{rt} takes no efforts"))?;
        }
        RowKey::ReviewerRuntime => {
            let rt = cycle(&reviewer_runtimes(&runtimes), &s.reviewer.runtime, forward)
                .ok_or("no runtime is installed and logged in")?;
            s.reviewer = ReviewerChoice {
                model: model_on(&runtimes, &rt, &s.reviewer.model),
                runtime: rt,
                account: None,
            };
        }
        RowKey::ReviewerModel => {
            let rt = s.reviewer.runtime.clone();
            s.reviewer.model = cycle(&models_of(&rt), &s.reviewer.model, forward)
                .ok_or_else(|| format!("{rt} reports no models to pick"))?;
        }
        RowKey::ReviewerAccount => {
            let accounts = runtimes
                .iter()
                .find(|r| r.runtime == s.reviewer.runtime)
                .map(|r| r.accounts.clone())
                .unwrap_or_default();
            let mut all: Vec<Option<String>> = vec![None];
            all.extend(accounts.into_iter().map(Some));
            s.reviewer.account =
                cycle(&all, &s.reviewer.account, forward).ok_or("no accounts to pick")?;
        }
        RowKey::PlanCheck => {
            // The profile's, then each value.
            let all = [
                None,
                Some(PlanCheck::Contracts),
                Some(PlanCheck::Always),
                Some(PlanCheck::Never),
            ];
            s.plan_check = cycle(&all, &s.plan_check, forward).flatten();
        }
        RowKey::ImplCommit => {
            let all = [ImplCommit::End, ImplCommit::PerRound, ImplCommit::None];
            s.commit = cycle(&all, &s.commit, forward).unwrap_or_default();
        }
        RowKey::ReviewCommit => {
            s.review_commit = match s.review_commit {
                ReviewCommit::Match => ReviewCommit::None,
                ReviewCommit::None => ReviewCommit::Match,
            };
        }
        RowKey::Sweep => {
            s.sweep = match s.sweep {
                SweepMode::Thread => SweepMode::Runs,
                SweepMode::Runs => SweepMode::Thread,
            };
        }
        RowKey::SaveFor => {
            if !has_project {
                return Err("no project here to save for: they are saved for all projects".into());
            }
            e.save_for = match e.save_for {
                SaveFor::All => SaveFor::Project,
                SaveFor::Project => SaveFor::All,
            };
        }
        RowKey::Save | RowKey::Reset => {}
    }
    Ok(())
}

/// The request that saves `e`'s draft: `loop.setup.update` at the revision loaded for a card,
/// else `loop.settings.set` (`project` for `this project`).
pub fn save_request(e: &LoopEditor) -> Option<(&'static str, Value)> {
    let s = e.draft.as_ref()?;
    match e.of.as_ref()? {
        EditorFor::Card { attn_id, revision } => Some((
            "loop.setup.update",
            json!({
                "attn_id": attn_id,
                "revision": (*revision)?,
                "settings": s,
                "save_for": e.save_for,
            }),
        )),
        EditorFor::Saved => {
            let mut p = json!({ "settings": s });
            if let (SaveFor::Project, Some(project)) = (e.save_for, &e.project) {
                p["project"] = json!(project);
            }
            Some(("loop.settings.set", p))
        }
    }
}

/// Widest the editor gets.
const MAX_WIDTH: u16 = 84;
/// Columns of a row's label.
const ROW_LABEL: usize = 16;

/// A row's dim line under it, `width` cells wide: under the value column, each wrapped line
/// too; with no room for that, from the row's start.
fn detail_rows(d: &str, width: usize, dim: Style) -> Vec<Line<'static>> {
    let indent = ROW_LABEL + 2;
    if width < indent + 12 {
        return crate::ui::wrap_line(Line::from(Span::styled(d.to_string(), dim)), width);
    }
    crate::ui::wrap_line(Line::from(Span::styled(d.to_string(), dim)), width - indent)
        .into_iter()
        .map(|mut row| {
            row.spans.insert(0, Span::raw(" ".repeat(indent)));
            row
        })
        .collect()
}

/// The editor, centered over `area`, as the settings overlay is drawn: what it edits at the
/// top, a row per setting (`label  value`, a dim line under some), the selected row
/// reversed, the hub's warnings and problems under them, the keys at the bottom. Every row
/// is its tap target ([`Target::LoopRow`]); its top border closes it.
pub fn render(frame: &mut Frame, area: Rect, state: &AppState, panes: &mut Panes) {
    let Some(e) = state.view.loop_editor.as_ref() else {
        return;
    };
    let width = area.width.min(MAX_WIDTH);
    let inner_w = usize::from(width.saturating_sub(2)).max(1);
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let thread_rt = state.loop_editor_thread_runtime();
    let what = match (&e.of, &e.thread) {
        (Some(EditorFor::Card { revision, .. }), thread) => {
            let name = thread
                .as_deref()
                .and_then(|t| state.registry.get(t))
                .map_or("its thread".to_string(), |t| t.name.clone());
            match revision {
                Some(r) => format!("{name}'s setup card, as raised at revision {r}"),
                None => format!("{name}'s setup card"),
            }
        }
        _ => match (e.got.as_ref().and_then(|g| g["source"].as_str()), &e.query) {
            (Some("project"), Some(p)) => format!("{p}'s own saved settings"),
            (Some(_), Some(p)) => format!("the saved settings {p} uses: all projects'"),
            (Some(_), None) => "the saved settings: all projects'".into(),
            (None, _) if e.got.is_some() => {
                "nothing saved yet: what the first setup card shows".into()
            }
            (None, _) => "the saved loop settings".into(),
        },
    };
    lines.extend(crate::ui::wrap_line(
        Line::from(Span::styled(what, dim)),
        inner_w,
    ));
    lines.push(Line::default());
    let mut spans = Vec::new();
    let rows = rows(e, thread_rt.as_deref());
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            e.busy.unwrap_or("loading…").to_string(),
            dim,
        )));
    }
    let selected = state.loop_editor_selected();
    for row in rows {
        let start = lines.len();
        let is_sel = Some(row.key) == selected;
        let base = if is_sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        // Selected, a row is all in reverse video, in the terminal's own colors: a dimmed or
        // marked value reversed would not read on a light terminal.
        let mut value_style = base.add_modifier(Modifier::BOLD);
        if row.dim {
            value_style = if is_sel {
                base
            } else {
                base.fg(Color::DarkGray)
            };
        }
        if matches!(row.key, RowKey::Save) && e.is_card() && !is_sel {
            value_style = value_style
                .fg(crate::color::text_on(WARNING_COLOR))
                .bg(WARNING_COLOR);
        }
        let label = format!(" {:<w$} ", row.label, w = ROW_LABEL);
        let mut head = Line::from(vec![
            Span::styled(label, base),
            Span::styled(row.value, value_style),
        ]);
        // Cut with `…` only when it is too wide; else padded to the width (a selected row is
        // reversed across it).
        if head.width() > inner_w {
            head = Line::from(crate::ui::cut_spans(head.spans, inner_w));
        }
        let pad = inner_w.saturating_sub(head.width());
        head.spans.push(Span::styled(" ".repeat(pad), base));
        lines.push(head);
        if let Some(d) = row.detail {
            lines.extend(detail_rows(&d, inner_w, dim));
        }
        spans.push((row.key, start..lines.len()));
    }
    if !e.warnings.is_empty() || !e.problems.is_empty() {
        lines.push(Line::default());
    }
    for w in &e.warnings {
        lines.extend(crate::ui::wrap_line(
            Line::from(vec![warning_mark("warning:"), Span::raw(format!(" {w}"))]),
            inner_w,
        ));
    }
    for p in &e.problems {
        lines.extend(crate::ui::wrap_line(
            Line::from(Span::styled(
                format!("change first: {p}"),
                Style::default().fg(Color::Red),
            )),
            inner_w,
        ));
    }
    if let Some(n) = e.note.as_ref().or(e.busy.map(str::to_string).as_ref()) {
        lines.push(Line::default());
        lines.extend(crate::ui::wrap_line(
            // Bold in the terminal's own color: no cyan reads on white with 16 colors.
            Line::from(Span::styled(
                n.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            inner_w,
        ));
    }
    lines.push(Line::default());
    let hint = if state.agent.is_some() {
        "read-only: this TUI runs for an agent · Esc closes"
    } else {
        "↑↓ select · ←→ Enter Space change · s saves · Esc closes, saving nothing"
    };
    lines.extend(crate::ui::wrap_line(
        Line::from(Span::styled(hint, dim)),
        inner_w,
    ));
    let rect = crate::ui::centered(
        area,
        width,
        u16::try_from(lines.len())
            .unwrap_or(u16::MAX)
            .saturating_add(2),
    );
    // Taller than the screen, it scrolls to keep the selected row in view.
    let fit = usize::from(rect.height.saturating_sub(2));
    let mut scroll = 0;
    if let Some((_, span)) = spans.iter().find(|(k, _)| Some(*k) == selected) {
        if span.end > fit {
            scroll = (span.end - fit).min(span.start);
        }
    }
    let title = match (e.is_card(), e.kind) {
        (true, Some(LoopKind::Implement)) => " implementation loop setup ".to_string(),
        (true, _) => " review loop setup ".to_string(),
        (false, _) => " loop settings ".to_string(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(LOOP_COLOR))
        .title(title)
        .title(crate::ui::close_title());
    let inner = block.inner(rect);
    panes.cover(None);
    panes.add(PaneId::Text, inner);
    crate::ui::close_row(panes, rect, Target::Close);
    for (key, span) in spans {
        let (start, end) = (span.start.max(scroll), span.end.min(scroll + fit));
        if start >= end {
            continue;
        }
        let y = inner
            .y
            .saturating_add(u16::try_from(start - scroll).unwrap_or(u16::MAX));
        let h = u16::try_from(end - start).unwrap_or(u16::MAX);
        let cells = Rect::new(inner.x, y, inner.width, h).intersection(inner);
        panes.target(cells, Target::LoopRow(key));
    }
    let scroll = u16::try_from(scroll).unwrap_or(u16::MAX);
    frame.render_widget(Clear, rect);
    frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)).block(block), rect);
}

/// A hub call about loops ([`Action::Loop`]).
pub fn call(method: &'static str, params: Value) -> Action {
    Action::Loop { method, params }
}

// ---------------------------------------------------------------------------------------------
// The state's side: commands, popup rows, the editor's keys, the hub's answers
// ---------------------------------------------------------------------------------------------

/// How a `loop.control` action is written, and what the status bar says while it goes.
fn control_words(action: &str) -> &'static str {
    match action {
        "pause" => "pausing",
        "resume" => "resuming",
        "stop" => "stopping",
        _ => "skipping to verification in",
    }
}

impl AppState {
    /// A `loop.settings.get` for the slash popup's profiles is due ([`AppState::loop_profiles`]):
    /// once per connection, and again after the saved settings changed. Its params.
    pub fn loop_settings_call(&mut self) -> Option<Value> {
        if self.loop_settings_asked {
            return None;
        }
        self.loop_settings_asked = true;
        Some(json!({}))
    }

    /// The loop profiles `/review --profile` completes to, with what each does (from the
    /// last `loop.settings.get`; the built-in three before it came).
    pub fn loop_profiles(&self) -> Vec<(String, String)> {
        let names: Vec<String> = self
            .loop_settings
            .as_ref()
            .and_then(|g| serde_json::from_value(g["profiles"].clone()).ok())
            .unwrap_or_else(|| {
                twr_core::loops::config::BUILTIN_PROFILES
                    .iter()
                    .map(|p| p.to_string())
                    .collect()
            });
        names
            .into_iter()
            .map(|n| {
                let what = profile_summary(&n, None);
                (n, what)
            })
            .collect()
    }

    /// The runtime of the thread the editor's card is about, if known.
    pub fn loop_editor_thread_runtime(&self) -> Option<String> {
        let e = self.view.loop_editor.as_ref()?;
        let t = self.registry.get(e.thread.as_deref()?)?;
        (!t.runtime.is_empty()).then(|| t.runtime.clone())
    }

    /// The editor's selected row: the one selected while it is drawn, else the first.
    pub fn loop_editor_selected(&self) -> Option<RowKey> {
        let e = self.view.loop_editor.as_ref()?;
        let rt = self.loop_editor_thread_runtime();
        let rows = rows(e, rt.as_deref());
        match e.selected {
            Some(k) if rows.iter().any(|r| r.key == k) => Some(k),
            _ => rows.first().map(|r| r.key),
        }
    }

    /// `/review [thread] [flags]` and the popup's `review loop`: `loop.request {thread, kind:
    /// review, setup?, scope?, overrides?}` on the named thread, else the one dived into, else
    /// the selected card's.
    pub(crate) fn request_review(
        &mut self,
        thread: Option<&str>,
        setup: bool,
        scope: Option<String>,
        overrides: Vec<(&'static str, Option<String>)>,
    ) -> (Vec<Action>, bool) {
        self.request_loop(LoopKind::Review, thread, setup, scope, None, overrides)
    }

    /// `/implement [thread] [flags] [task…]`: `loop.request {thread, kind: implement, task?,
    /// setup?, overrides?}`. In a dive the thread is that one and every word is the task;
    /// elsewhere a first word that names a live thread is the thread, else the selected
    /// card's ([`twr_core::slash::implement_target`]).
    pub(crate) fn request_implement(
        &mut self,
        words: &[String],
        setup: bool,
        overrides: Vec<(&'static str, Option<String>)>,
    ) -> (Vec<Action>, bool) {
        let dive = self.view.dive.is_some();
        let (thread, task) = twr_core::slash::implement_target(words, dive, |w| {
            self.registry
                .resolve(w)
                .ok()
                .is_some_and(|t| t.id == w || t.name == w || t.aliases.iter().any(|a| a == w))
        });
        if thread.is_none() && self.thread_arg(None).is_err() {
            self.view.status = Some(
                "no thread to run it in: select a card, dive into a thread, or name one first (/implement <thread> <task>)"
                    .into(),
            );
            return (Vec::new(), false);
        }
        self.request_loop(
            LoopKind::Implement,
            thread.as_deref(),
            setup,
            None,
            task,
            overrides,
        )
    }

    /// `loop.request {thread, kind, setup?, scope?, task?, overrides?}` on the named thread,
    /// else the one dived into, else the selected card's.
    fn request_loop(
        &mut self,
        kind: LoopKind,
        thread: Option<&str>,
        setup: bool,
        scope: Option<String>,
        task: Option<String>,
        overrides: Vec<(&'static str, Option<String>)>,
    ) -> (Vec<Action>, bool) {
        let t = match self.thread_arg(thread) {
            Ok(t) => t,
            Err(e) => {
                self.view.status = Some(e);
                return (Vec::new(), false);
            }
        };
        if t.is_orchestrator() {
            self.view.status = Some("the orchestrator runs no loop: name a worker thread".into());
            return (Vec::new(), false);
        }
        let (id, name) = (t.id.clone(), t.name.clone());
        let mut p = json!({ "thread": id, "kind": kind.as_str() });
        if let Some(task) = task {
            p["task"] = json!(task);
        }
        if setup {
            p["setup"] = json!(true);
        }
        if let Some(s) = scope {
            p["scope"] = json!(s);
        }
        let o: serde_json::Map<String, Value> = overrides
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), json!(v?))))
            .collect();
        if !o.is_empty() {
            p["overrides"] = Value::Object(o);
        }
        self.view.status = Some(format!("{} on {name}: asking the hub", kind_words(kind)));
        (vec![call("loop.request", p)], true)
    }

    /// The popup's `implementation loop…`: `/implement ` in the input box (with the thread's
    /// name when it is not the one dived into), ready for the task. A line already in the box
    /// goes to the history (↑).
    pub(crate) fn prefill_implement(&mut self, thread: &str) {
        let Some(t) = self.registry.get(thread) else {
            return;
        };
        let name = t.name.clone();
        // In a dive every word of `/implement` is the task, for the thread dived into: to
        // name another one, the dive is left first, its card selected.
        let left = match self.view.dive.as_deref() {
            Some(d) if d != thread => {
                self.back();
                self.set_focus(Some(thread.to_string()));
                true
            }
            _ => false,
        };
        let line = if self.view.dive.as_deref() == Some(thread) {
            "/implement ".to_string()
        } else {
            format!("/implement {name} ")
        };
        let old = self.view.input.as_str().to_string();
        let kept = !old.trim().is_empty() && old != line;
        if kept {
            self.keep_taken_back(&old);
        }
        let (cols, rows) = (self.input_cols(), self.input_rows());
        self.view.input.set(&line);
        self.view.input.keep_visible(cols, rows);
        let mut said = "type the task and Enter starts the implementation loop".to_string();
        if left {
            said.push_str(&format!(" on {name} (out of the dive)"));
        }
        said.push_str(if kept {
            " (your line is in the history, ↑)"
        } else {
            " (no task: what the conversation settled)"
        });
        self.view.status = Some(said);
    }

    /// `/loop [thread] pause|resume|stop|skip|status` and the popup's loop rows: `stop` asks
    /// `y` first, `status` says where the loop is in the status bar.
    pub(crate) fn control_loop(
        &mut self,
        thread: Option<&str>,
        action: &str,
    ) -> (Vec<Action>, bool) {
        let t = match self.thread_arg(thread) {
            Ok(t) => t,
            Err(e) => {
                self.view.status = Some(e);
                return (Vec::new(), false);
            }
        };
        if action == "status" {
            self.view.status = Some(status_text(t));
            return (Vec::new(), true);
        }
        let (id, name) = (t.id.clone(), t.name.clone());
        let Some(l) = &t.r#loop else {
            self.view.status = Some(format!(
                "{name} runs no loop (/implement or /review starts one)"
            ));
            return (Vec::new(), false);
        };
        let kind = kind_words(l.kind);
        if action == "skip_to_verify" {
            if let Some(why) = skip_refused(l) {
                self.view.status = Some(format!("{name}: {why}"));
                return (Vec::new(), false);
            }
        }
        let act = call("loop.control", json!({ "thread": id, "action": action }));
        let pending = format!(
            "{} {name}'s loop: waiting for the hub",
            control_words(action)
        );
        if action == "stop" {
            self.view.status = None;
            self.view.confirm = Some(crate::app::Confirm {
                action: act,
                prompt: format!(
                    "stop {name}'s {kind}? The thread and its work stay as they are. y to \
                     confirm, any other key cancels"
                ),
                pending,
                alt: None,
                offer: false,
            });
            return (Vec::new(), true);
        }
        self.view.status = Some(pending);
        (vec![act], true)
    }

    /// Opens the editor on the saved settings that apply to `project` (`None`: all
    /// projects'); `prefer_project`: `Save for` starts on `this project`.
    pub(crate) fn open_loop_settings(
        &mut self,
        project: Option<String>,
        prefer_project: bool,
    ) -> Vec<Action> {
        self.view.action = None;
        self.view.loop_editor = Some(LoopEditor {
            of: Some(EditorFor::Saved),
            project: project.clone(),
            query: project.clone(),
            save_for: if prefer_project && project.is_some() {
                SaveFor::Project
            } else {
                SaveFor::All
            },
            busy: Some("loading…"),
            ..Default::default()
        });
        let mut p = json!({});
        if let Some(pr) = project {
            p["project"] = json!(pr);
        }
        vec![call("loop.settings.get", p)]
    }

    /// The setup card `attn_id` of thread `thread`'s `change…`: the editor on its draft
    /// (`loop.setup.get`), with what the runtimes offer (`loop.settings.get`).
    pub(crate) fn open_loop_card_editor(&mut self, attn_id: &str, thread: &str) -> Vec<Action> {
        self.view.action = None;
        self.view.loop_editor = Some(LoopEditor {
            of: Some(EditorFor::Card {
                attn_id: attn_id.to_string(),
                revision: None,
            }),
            thread: Some(thread.to_string()),
            busy: Some("loading…"),
            ..Default::default()
        });
        // (What the runtimes offer on the thread's box: a remote box's own, named.)
        vec![
            call("loop.setup.get", json!({ "attn_id": attn_id })),
            call("loop.settings.get", json!({ "thread": thread })),
        ]
    }

    /// The editor's keys: ↑↓ (Home/End) select a row, ←→ Enter Space change it (Enter or Space
    /// on Save saves, on Reset asks `y`), `s` saves, Esc or `q` closes it, saving nothing. A
    /// `y` answers Reset's question; any other key then keeps the settings. It takes every
    /// key.
    pub(crate) fn loop_editor_key(&mut self, key: crossterm::event::KeyEvent) -> Vec<Action> {
        use crossterm::event::KeyCode;
        let Some(e) = self.view.loop_editor.as_mut() else {
            return Vec::new();
        };
        if e.confirm_reset {
            e.confirm_reset = false;
            if key.code == KeyCode::Char('y') {
                return self.loop_reset();
            }
            self.view.status = Some("reset cancelled: the saved settings stay".into());
            return Vec::new();
        }
        let rt = self.loop_editor_thread_runtime();
        let Some(e) = self.view.loop_editor.as_ref() else {
            return Vec::new();
        };
        let keys: Vec<RowKey> = rows(e, rt.as_deref()).iter().map(|r| r.key).collect();
        let at = self
            .loop_editor_selected()
            .and_then(|k| keys.iter().position(|x| *x == k));
        let select = |st: &mut AppState, i: usize| {
            if let (Some(e), Some(k)) = (st.view.loop_editor.as_mut(), keys.get(i)) {
                e.selected = Some(*k);
            }
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                let unsaved = self.view.loop_editor.as_ref().is_some_and(|e| e.changed());
                self.view.loop_editor = None;
                if unsaved {
                    self.view.status = Some("closed: nothing was saved".into());
                }
            }
            KeyCode::Up => select(self, at.unwrap_or(0).saturating_sub(1)),
            KeyCode::Down => select(
                self,
                at.map_or(0, |i| (i + 1).min(keys.len().saturating_sub(1))),
            ),
            KeyCode::Home => select(self, 0),
            KeyCode::End => select(self, keys.len().saturating_sub(1)),
            KeyCode::Char('s') => return self.loop_save(),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Right => {
                if let Some(k) = at.map(|i| keys[i]) {
                    return self.loop_change(k, true);
                }
            }
            KeyCode::Left => {
                if let Some(k) = at.map(|i| keys[i]) {
                    return self.loop_change(k, false);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// A tap on the editor's row `key`: selects it, and a tap on the selected one changes it
    /// (Enter).
    pub(crate) fn tap_loop_row(&mut self, key: RowKey) -> Vec<Action> {
        if self.loop_editor_selected() != Some(key) {
            if let Some(e) = self.view.loop_editor.as_mut() {
                e.selected = Some(key);
                e.confirm_reset = false;
            }
            return Vec::new();
        }
        self.loop_change(key, true)
    }

    /// Changes row `key` of the editor (`forward` or back): the draft, or for Save and Reset
    /// what they do.
    fn loop_change(&mut self, key: RowKey, forward: bool) -> Vec<Action> {
        match key {
            RowKey::Save => return self.loop_save(),
            RowKey::Reset => {
                if let Some(agent) = &self.agent {
                    self.view.status =
                        Some(format!("this TUI runs for {agent}: it changes no setting"));
                    return Vec::new();
                }
                if let Some(e) = self.view.loop_editor.as_mut() {
                    e.confirm_reset = true;
                }
                self.view.status = Some("reset the saved loop settings? y to confirm".into());
                return Vec::new();
            }
            _ => {}
        }
        let rt = self.loop_editor_thread_runtime();
        let Some(e) = self.view.loop_editor.as_mut() else {
            return Vec::new();
        };
        match step(e, key, forward, rt.as_deref()) {
            Ok(()) => {
                e.note = None;
                self.view.status = None;
            }
            Err(why) => self.view.status = Some(why),
        }
        Vec::new()
    }

    /// Saves the editor's draft ([`save_request`]); a TUI an agent runs saves nothing.
    fn loop_save(&mut self) -> Vec<Action> {
        if let Some(agent) = &self.agent {
            self.view.status = Some(format!("this TUI runs for {agent}: it changes no setting"));
            return Vec::new();
        }
        let Some(e) = self.view.loop_editor.as_mut() else {
            return Vec::new();
        };
        if e.busy.is_some() {
            self.view.status = Some("wait for the hub's answer first".into());
            return Vec::new();
        }
        let Some((method, params)) = save_request(e) else {
            self.view.status = Some("the settings are still loading".into());
            return Vec::new();
        };
        e.busy = Some("saving…");
        e.note = None;
        self.view.status = None;
        vec![call(method, params)]
    }

    /// Reset's `y`: `loop.settings.reset` for the editor's project (none: every saved set).
    fn loop_reset(&mut self) -> Vec<Action> {
        let Some(e) = self.view.loop_editor.as_mut() else {
            return Vec::new();
        };
        e.busy = Some("resetting…");
        let mut p = json!({});
        if let Some(pr) = &e.query {
            p["project"] = json!(pr);
        }
        self.view.status = None;
        vec![call("loop.settings.reset", p)]
    }

    /// What a loop call returned (`v`: `{asked, result}`, or `{asked, failed, code, error}`):
    /// the status bar says what it did; `needs_setup` selects the thread's card; the editor
    /// takes what it asked for.
    pub fn loop_result(&mut self, method: &str, v: &Value) {
        let asked = &v["asked"];
        let r = &v["result"];
        let failed = v["failed"].as_bool() == Some(true);
        let error = v["error"].as_str().unwrap_or("failed").to_string();
        let code = v["code"].as_str().unwrap_or_default().to_string();
        let name_of = |st: &AppState, id: &str| {
            st.registry
                .get(id)
                .map_or_else(|| id.to_string(), |t| t.name.clone())
        };
        match method {
            "loop.request" if failed => {
                let kind = kind_of(asked);
                self.view.status = Some(format!("{}: {error}", kind_words(kind)));
            }
            "loop.request" => {
                let kind = kind_words(kind_of(asked));
                let thread = asked["thread"].as_str().unwrap_or_default().to_string();
                let name = name_of(self, &thread);
                if r["status"] == "needs_setup" {
                    // The card in front of the user, never answered here.
                    if self.view.dive.is_some() {
                        self.back();
                    }
                    if self.compact() {
                        self.show_pane(crate::app::CompactPane::Needs);
                    }
                    self.set_focus(Some(thread));
                    self.view.status = Some(format!(
                        "{name}'s {kind} waits for its setup card: read it on {name}'s card \
                         (Enter opens it); 1 starts the loop and saves the settings"
                    ));
                } else {
                    let l = &r["loop"];
                    let line = l["line"].as_str().unwrap_or("started");
                    let reviewer = match (
                        l["reviewer"]["runtime"].as_str(),
                        l["reviewer"]["model"].as_str(),
                    ) {
                        (Some(rt), Some(m)) => format!("; reviewer {rt} · {m}"),
                        _ => String::new(),
                    };
                    self.view.status = Some(format!("{name}: {kind} started ({line}){reviewer}"));
                }
            }
            "loop.control" if failed => {
                self.view.status = Some(format!("loop: {error}"));
            }
            "loop.control" => {
                let name = r["thread_name"].as_str().unwrap_or("the thread");
                // The kind the hub names, else the one the thread's loop has.
                let kind = kind_words(
                    serde_json::from_value(r["kind"].clone())
                        .ok()
                        .or_else(|| {
                            asked["thread"]
                                .as_str()
                                .and_then(|t| self.registry.get(t))
                                .and_then(|t| t.r#loop.as_ref().map(|l| l.kind))
                        })
                        .unwrap_or_default(),
                );
                let what = match asked["action"].as_str().unwrap_or_default() {
                    "pause" => "paused",
                    "resume" => "resumed",
                    "stop" => "stopped",
                    _ => "skips to verification after this round",
                };
                self.view.status = Some(match r["line"].as_str() {
                    Some(line) if r["running"] == true => {
                        format!("{name}'s {kind} {what}: {line}")
                    }
                    _ => format!("{name}'s {kind} {what}"),
                });
            }
            "loop.settings.get" => {
                let project = asked["project"].as_str().map(str::to_string);
                // (A thread's are its box's: not the workspace's settings line.)
                if !failed && project.is_none() && asked["thread"].is_null() {
                    self.loop_settings = Some(r.clone());
                }
                let Some(e) = self.view.loop_editor.as_mut() else {
                    return;
                };
                if failed {
                    e.busy = None;
                    e.note = Some(format!("could not read the loop settings: {error}"));
                    return;
                }
                if e.is_card() {
                    if project.is_none() {
                        e.got = Some(r.clone());
                    }
                    return;
                }
                if project != e.query {
                    return;
                }
                let settings: Option<LoopSettings> =
                    serde_json::from_value(r["settings"].clone()).ok();
                let shown =
                    settings.or_else(|| serde_json::from_value(r["first_card"].clone()).ok());
                e.got = Some(r.clone());
                e.draft = shown.clone();
                e.loaded = shown;
                e.warnings = strings(&r["warnings"]);
                e.problems = strings(&r["problems"]);
                if r["source"] == "project" && e.project.is_some() {
                    e.save_for = SaveFor::Project;
                }
                e.busy = None;
            }
            "loop.setup.get" => {
                let Some(e) = self.view.loop_editor.as_mut() else {
                    return;
                };
                let same = matches!(&e.of, Some(EditorFor::Card { attn_id, .. }) if Some(attn_id.as_str()) == asked["attn_id"].as_str());
                if !same {
                    return;
                }
                if failed {
                    e.busy = None;
                    e.note = Some(format!("the setup card is gone: {error}"));
                    return;
                }
                e.load_card(r);
            }
            "loop.setup.update" => {
                let Some(e) = self.view.loop_editor.as_mut() else {
                    if failed {
                        self.view.status = Some(format!("the setup card: {error}"));
                    }
                    return;
                };
                let attn = asked["attn_id"].as_str().unwrap_or_default().to_string();
                e.busy = None;
                if failed {
                    if code == twr_core::proto::codes::CONFLICT {
                        // Raised again since: what it shows now, and the editor on it.
                        e.busy = Some("loading…");
                        e.note = Some(
                            "the card changed since it was loaded: reloaded with what it shows \
                             now; change it again"
                                .into(),
                        );
                        self.pending_loop
                            .push(call("loop.setup.get", json!({ "attn_id": attn })));
                    } else {
                        e.note = Some(format!("not saved: {error}"));
                    }
                    return;
                }
                e.load_card(r);
                if e.problems.is_empty() {
                    let thread = e.thread.clone().unwrap_or_default();
                    self.view.loop_editor = None;
                    let name = name_of(self, &thread);
                    self.view.status = Some(format!(
                        "{name}'s setup card shows your settings now: 1 on it starts the loop \
                         and saves them"
                    ));
                } else {
                    e.note = Some(
                        "the card took these settings, but they can't start a loop yet: change \
                         what it says"
                            .into(),
                    );
                }
            }
            "loop.settings.set" => {
                let Some(e) = self.view.loop_editor.as_mut() else {
                    return;
                };
                e.busy = None;
                if failed {
                    e.note = Some(format!("not saved: {error}"));
                    return;
                }
                let whose = match asked["project"].as_str() {
                    Some(p) => format!("{p}'s own"),
                    None => "all projects'".into(),
                };
                self.view.loop_editor = None;
                self.view.status = Some(format!(
                    "loop settings saved as {whose}: loops start on them without a card"
                ));
            }
            "loop.settings.reset" => {
                let Some(e) = self.view.loop_editor.as_mut() else {
                    return;
                };
                e.busy = None;
                if failed {
                    e.note = Some(format!("not reset: {error}"));
                    return;
                }
                self.view.loop_editor = None;
                self.view.status = Some(match (asked["project"].as_str(), r["reset"].as_bool()) {
                    (_, Some(false)) => "nothing was saved to reset".into(),
                    (Some(p), _) => format!("{p} uses the workspace's loop settings again"),
                    (None, _) => {
                        "loop settings forgotten: the next loop shows its setup card first".into()
                    }
                });
            }
            _ => {}
        }
    }

    /// The calls a loop answer asked for (a reload after a conflict), to send now.
    pub fn take_loop_calls(&mut self) -> Vec<Action> {
        std::mem::take(&mut self.pending_loop)
    }
}

impl LoopEditor {
    /// A setup card as `loop.setup.get` or `loop.setup.update` returned it (`r`): its revision,
    /// its settings, where it saves, its warnings and problems.
    fn load_card(&mut self, r: &Value) {
        let setup = &r["setup"];
        if let Some(EditorFor::Card { revision, .. }) = self.of.as_mut() {
            *revision = r["revision"].as_u64();
        }
        let settings: Option<LoopSettings> = serde_json::from_value(setup["settings"].clone()).ok();
        self.draft = settings.clone();
        self.loaded = settings;
        self.save_for = serde_json::from_value(setup["save_for"].clone()).unwrap_or_default();
        self.project = setup["project"].as_str().map(str::to_string);
        self.profile = serde_json::from_value(setup["profile"].clone()).ok();
        self.kind = serde_json::from_value(setup["kind"].clone()).ok();
        self.warnings = strings(&setup["warnings"]);
        self.problems = strings(&setup["problems"]);
        if let Some(t) = r["thread"].as_str() {
            self.thread = Some(t.to_string());
        }
        self.busy = None;
    }
}

/// The kind of loop a `loop.request` asked for (`kind`), a review loop when it says none.
fn kind_of(asked: &Value) -> LoopKind {
    serde_json::from_value(asked["kind"].clone()).unwrap_or_default()
}

/// A list of strings in `v`.
fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}

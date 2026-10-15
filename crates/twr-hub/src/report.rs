//! Report summaries: the hub's fallback report (from the turn's final text) and the trimming
//! applied to reports the orchestrator or a user writes. Grading is the decision ladder's
//! (`twr_decisions`).
//!
//! The fallback report quotes the reply's own summary when it has one ([`reply_summary`]: a
//! `## Summary` / `**TL;DR**` section or an inline `Summary: ...` paragraph), else its first
//! sentences ([`first_sentences`]). It is a heuristic over the text: workers are
//! not asked to write a summary.

use twr_core::blob::truncate_utf8;
use twr_core::event::{ReportReady, TurnResult};

/// Maximum fallback report summary length, in bytes.
pub const SUMMARY_MAX: usize = 400;

/// Most words a bold line has to read as a section title (more is a bold sentence).
const TITLE_WORDS: usize = 6;

/// `report.ready` `ext.twr` key of the hub's fallback report: the seq of the reply it
/// summarizes (the turn's last final `agent.text`), which `read_transcript seq=<seq>` reads in
/// full.
pub const REPLY_SEQ_KEY: &str = "reply_seq";

/// `report.ready` `ext.twr` key of the hub's fallback report: how many background tasks were
/// still running when the turn ended. Such a report is interim: the result comes later.
pub const BACKGROUND_TASKS_KEY: &str = "background_tasks";

/// `report.ready` `ext.twr` key of the hub's fallback report of a turn the runtime started
/// itself, after background work (`"runtime"`): a follow-up turn.
pub const TURN_FROM_KEY: &str = "turn_from";

/// How many background tasks were still running when `report`'s turn ended (0: none, or a
/// report that does not say: a final one).
pub fn interim_tasks(report: &ReportReady) -> u64 {
    report_twr(report)
        .and_then(|t| t.get(BACKGROUND_TASKS_KEY))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// Whether `report` covers a turn the runtime started itself (a follow-up turn).
pub fn is_follow_up(report: &ReportReady) -> bool {
    report_twr(report)
        .and_then(|t| t.get(TURN_FROM_KEY))
        .and_then(serde_json::Value::as_str)
        == Some("runtime")
}

fn report_twr(report: &ReportReady) -> Option<&serde_json::Value> {
    report.ext.as_ref().and_then(|e| e.get("twr"))
}

/// First at most three sentences and [`SUMMARY_MAX`] bytes of `text`, whitespace collapsed.
pub fn first_sentences(text: &str) -> String {
    trim_summary(text, SUMMARY_MAX)
}

/// `text` fit for a terminal: C0 and C1 control characters and DEL removed (a terminal acts
/// on escape sequences), except tab and carriage return, which become spaces, and newline,
/// kept when `newlines` (else a space). Agent-controlled text the hub shows its clients
/// (attention summaries and options, notes, report summaries) is stored this way; the raw
/// original stays in the event's raw blob.
pub fn display_text(text: &str, newlines: bool) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\n' if newlines => Some('\n'),
            '\n' | '\t' | '\r' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// First at most three sentences and `max` bytes of `text` (at least 4), whitespace collapsed,
/// control characters removed ([`display_text`]); a cut ends in `...`.
pub fn trim_summary(text: &str, max: usize) -> String {
    let max = max.max(4);
    let collapsed = display_text(text, false)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut end = collapsed.len();
    let mut sentences = 0;
    let bytes = collapsed.as_bytes();
    for (i, c) in collapsed.char_indices() {
        if matches!(c, '.' | '!' | '?') && bytes.get(i + 1).is_none_or(|b| *b == b' ') {
            sentences += 1;
            if sentences == 3 {
                end = i + 1;
                break;
            }
        }
    }
    let s = &collapsed[..end];
    if s.len() <= max {
        return s.to_string();
    }
    let cut = truncate_utf8(s, max - 3);
    format!("{}...", cut.trim_end())
}

/// Labels that name a summary section (compared lowercased, emphasis and a trailing `:`
/// removed). The first [`STRONG_LABELS`] of them are preferred over the rest.
const LABELS: &[&str] = &[
    "summary",
    "tl;dr",
    "tldr",
    "in short",
    "bottom line",
    "results",
    "result",
    "outcome",
    "conclusion",
    "status",
    "verdict",
];

/// How many of [`LABELS`] (the first ones) are summaries proper; the others (results, status,
/// ...) count only when the reply has none of these.
const STRONG_LABELS: usize = 5;

/// What a line of a reply is to [`reply_summary`].
enum Line<'a> {
    /// A markdown heading: the index of its label in [`LABELS`], if it is one.
    Heading(Option<usize>),
    /// A line made only of a label (`**Summary**`, `Summary:`, `*TL;DR*`), or only of bold
    /// text (`**Next steps**`, which ends a section): the label index, if it is one.
    Label(Option<usize>),
    /// `Summary: text` / `**TL;DR:** text`: the label index and the text after it.
    Inline(usize, &'a str),
    /// A short plain title ending in a colon (`Tests:`, `Next steps:`): it ends a label
    /// line's section or an inline label's paragraph; under a heading it is text.
    Title,
    /// A thematic break (`---`, `***`).
    Rule,
    /// An empty line.
    Blank,
    /// Anything else.
    Text,
}

/// `s` without surrounding emphasis markers (`*`, `_`) and whitespace.
fn strip_emphasis(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '*' || c == '_').trim()
}

/// The index in [`LABELS`] of `text` (a heading's or label line's text), if it names a
/// summary. With `loose` (a heading or a bold line) it may say more after `summary`
/// (`Summary of changes`).
fn label_of(text: &str, loose: bool) -> Option<usize> {
    let t = strip_emphasis(text);
    let t = strip_emphasis(t.strip_suffix(':').unwrap_or(t));
    let t = t.strip_suffix(':').unwrap_or(t).trim().to_ascii_lowercase();
    LABELS.iter().position(|l| *l == t).or_else(|| {
        t.strip_prefix("summary of ")
            .filter(|rest| loose && !rest.is_empty())
            .map(|_| 0)
    })
}

/// `Summary: text` (or `**Summary:** text`, `**TL;DR**: text`): the label index and the text.
fn inline_label(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_start_matches(['*', '_']);
    LABELS.iter().enumerate().find_map(|(i, l)| {
        // Labels are ASCII: a prefix that matches ends on a char boundary.
        if !t.get(..l.len()).is_some_and(|p| p.eq_ignore_ascii_case(l)) {
            return None;
        }
        let rest = t[l.len()..].trim_start_matches(['*', '_']);
        let rest = rest.strip_prefix(':')?;
        let rest = rest.trim_start_matches(['*', '_']).trim();
        (!rest.is_empty()).then_some((i, rest))
    })
}

fn classify(line: &str) -> Line<'_> {
    let t = line.trim();
    if t.is_empty() {
        return Line::Blank;
    }
    let hashes = t.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
        return Line::Heading(label_of(t[hashes..].trim().trim_end_matches('#'), true));
    }
    let mut marks = t.chars().filter(|c| *c != ' ');
    let first = marks.next().unwrap_or(' ');
    if matches!(first, '-' | '*' | '_')
        && t.chars().filter(|c| *c != ' ').count() >= 3
        && marks.all(|c| c == first)
    {
        return Line::Rule;
    }
    if let Some((i, rest)) = inline_label(t) {
        return Line::Inline(i, rest);
    }
    let emphasized = t.starts_with("**") || t.starts_with("__");
    if let Some(i) = label_of(t, emphasized) {
        return Line::Label(Some(i));
    }
    // Only bold text (`**Next steps**`, `**Changes:**`, `__Notes__:`): a title. A bold
    // sentence (`**Fixed the redirect bug.**`) is text: it ends in a sentence mark or runs
    // longer than a title.
    let b = t.strip_suffix(':').unwrap_or(t);
    let bold_only = ["**", "__"].iter().any(|m| {
        b.len() > 2 * m.len()
            && b.starts_with(m)
            && b.ends_with(m)
            && !b[m.len()..b.len() - m.len()].contains(m)
            && {
                let inner = b[m.len()..b.len() - m.len()].trim();
                !inner.ends_with(['.', '!', '?']) && inner.split_whitespace().count() <= TITLE_WORDS
            }
    });
    if bold_only {
        return Line::Label(None);
    }
    if let Some(title) = t.strip_suffix(':') {
        let first = title.chars().next().unwrap_or(' ');
        if first.is_alphabetic()
            && !title.contains([':', '.', '!', '?', '*', '`'])
            && title.split_whitespace().count() <= TITLE_WORDS
        {
            return Line::Title;
        }
    }
    Line::Text
}

/// One line of a summary section, list marker removed (`- `, `* `, `+ `, `1. `, `1) `).
fn unlisted(line: &str) -> (&str, bool) {
    let t = line.trim();
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(m) {
            return (rest.trim(), true);
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(rest) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return (rest.trim(), true);
        }
    }
    (t, false)
}

/// Whether section line `line` (or an inline label's text) gives its section any text: list
/// marker and bold markers (`**`) removed, something is left.
fn gives_text(line: &str) -> bool {
    let text = unlisted(line).0;
    if text.contains("**") {
        !text.replace("**", "").trim().is_empty()
    } else {
        !text.is_empty()
    }
}

/// Joins section lines into one paragraph: list items are separated by `; ` unless they end
/// in punctuation; bold markers (`**`) go. Stops once the paragraph holds more than `max`
/// bytes a summary shows (neither whitespace nor control characters, [`display_text`]): a
/// summary cut at `max` ([`cut_summary`]) reads the same without the lines after that.
fn join_section<'a>(lines: impl IntoIterator<Item = &'a str>, max: usize) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for line in lines {
        let (text, item) = unlisted(line);
        if text.is_empty() {
            continue;
        }
        if !out.is_empty() {
            if item && !out.ends_with(['.', '!', '?', ';', ':', ',']) {
                out.push(';');
            }
            out.push(' ');
        }
        out.push_str(text);
        shown += text
            .replace("**", "")
            .chars()
            .filter(|c| !c.is_whitespace() && !c.is_control())
            .map(char::len_utf8)
            .sum::<usize>();
        if shown > max {
            break;
        }
    }
    out.replace("**", "")
}

/// The reply's own summary, if it has one: the section under a markdown heading or a label
/// line (bold or plain) whose text is a summary label ([`LABELS`]: summary, tl;dr, in short,
/// bottom line, result(s), outcome, conclusion, status, verdict; a heading or label line may
/// also read `Summary of ...`), or an inline `Summary: ...` / `**TL;DR:** ...` paragraph. A
/// heading's section runs to the next heading, bold line, label line or thematic break (an
/// inline `Result: ...` line in it is part of it), and so does a label line's; an inline
/// label's ends with its paragraph too. Fenced code blocks are skipped. A summary label wins
/// over the others (`## Summary` over an earlier `## Status`); among equals the first one
/// counts. `None` when no non-empty section exists. The text is returned as written (lines
/// joined, `**` removed), but only as far as a summary cut at [`SUMMARY_MAX`] needs it
/// ([`join_section`]); [`summary`] collapses and cuts it.
///
/// Linear in the reply: one backward pass finds where a section starting at each line would
/// end and whether it has text, one forward pass picks the section, and only that one is
/// joined.
pub fn reply_summary(text: &str) -> Option<String> {
    let raw: Vec<&str> = text.lines().collect();
    let mut fenced = false;
    let lines: Vec<Line<'_>> = raw
        .iter()
        .map(|l| {
            let fence = l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~");
            if fence {
                fenced = !fenced;
                return Line::Text;
            }
            if fenced {
                Line::Text
            } else {
                classify(l)
            }
        })
        .collect();
    // A setext heading (`Summary` over a line of `=` or `-`): the text line is the heading
    // and its underline goes.
    let mut lines = lines;
    let mut in_code = vec![false; raw.len()];
    let mut open = false;
    for (i, l) in raw.iter().enumerate() {
        let fence = l.trim_start().starts_with("```") || l.trim_start().starts_with("~~~");
        in_code[i] = open || fence;
        open ^= fence;
    }
    for i in 1..lines.len() {
        if in_code[i] || in_code[i - 1] {
            continue;
        }
        let under = raw[i].trim();
        let setext = under.len() >= 3
            && (under.bytes().all(|b| b == b'=') || under.bytes().all(|b| b == b'-'));
        if setext
            && matches!(lines[i - 1], Line::Text | Line::Title | Line::Label(_))
            && !raw[i - 1].trim().is_empty()
        {
            lines[i - 1] = Line::Heading(label_of(raw[i - 1].trim(), true));
            lines[i] = Line::Blank;
        }
    }
    let n = lines.len();
    // For a section whose lines start at `j`: the line that ends a heading's section (the
    // next heading, label line or rule), the one that ends a label line's section (also a
    // plain title such as `Tests:`), the one that ends an inline label's paragraph (also a
    // blank line), and the first line with text from `j` on (`n`: none).
    let mut section_end = vec![n; n + 1];
    let mut label_end = vec![n; n + 1];
    let mut paragraph_end = vec![n; n + 1];
    let mut next_text = vec![n; n + 1];
    for j in (0..n).rev() {
        let (s, l, p, t) = (
            section_end[j + 1],
            label_end[j + 1],
            paragraph_end[j + 1],
            next_text[j + 1],
        );
        (section_end[j], label_end[j], paragraph_end[j], next_text[j]) = match lines[j] {
            Line::Heading(_) | Line::Label(_) | Line::Rule => (j, j, j, t),
            Line::Title => (s, j, j, j),
            Line::Blank => (s, l, j, t),
            // `Result: ...` inside a section is part of it.
            Line::Text | Line::Inline(..) if gives_text(raw[j]) => (s, l, p, j),
            Line::Text | Line::Inline(..) => (s, l, p, t),
        };
    }
    // (the inline label's own text, first line after the label, end) of the section.
    let section = |i: usize| -> Option<(Option<&str>, usize, usize)> {
        let (own, end) = match lines[i] {
            Line::Heading(Some(_)) => (None, section_end[i + 1]),
            Line::Label(Some(_)) => (None, label_end[i + 1]),
            Line::Inline(_, rest) => (Some(rest), paragraph_end[i + 1]),
            _ => return None,
        };
        let has_text = own.is_some_and(gives_text) || next_text[i + 1] < end;
        has_text.then_some((own, i + 1, end))
    };
    let mut weak = None;
    let mut chosen = None;
    for (i, line) in lines.iter().enumerate() {
        let label = match line {
            Line::Heading(Some(l)) | Line::Label(Some(l)) | Line::Inline(l, _) => *l,
            _ => continue,
        };
        let Some(s) = section(i) else {
            continue;
        };
        if label < STRONG_LABELS {
            chosen = Some(s);
            break;
        }
        weak.get_or_insert(s);
    }
    let (own, from, end) = chosen.or(weak)?;
    let body = (from..end)
        .filter(|j| matches!(lines[*j], Line::Text | Line::Title | Line::Inline(..)))
        .map(|j| raw[j]);
    Some(join_section(own.into_iter().chain(body), SUMMARY_MAX))
}

/// [`SUMMARY_MAX`] bytes of `text`, whitespace collapsed and control characters removed; a cut
/// ends in `...`.
fn cut_summary(text: &str) -> String {
    let collapsed = display_text(text, false)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if collapsed.len() <= SUMMARY_MAX {
        return collapsed;
    }
    let cut = truncate_utf8(&collapsed, SUMMARY_MAX - 3);
    format!("{}...", cut.trim_end())
}

/// The summary the hub's fallback report quotes from a reply: its own summary section
/// ([`reply_summary`]) cut to [`SUMMARY_MAX`], else its [`first_sentences`].
pub fn reply_report(text: &str) -> String {
    match reply_summary(text) {
        Some(s) => cut_summary(&s),
        None => first_sentences(text),
    }
}

/// The fallback report summary of a worker's turn: the turn's final text's summary
/// ([`reply_report`]), or a stand-in, which says how many background tasks still run.
/// Orchestrator turns get no report (`Core::react_to_orchestrator_turn_end`). It names no
/// way to open the thread: that is the same for every report of it (`twr attach <name>`, the
/// TUI's `/attach`), and the report's readers have the thread.
pub fn summary(
    final_text: Option<&str>,
    result: TurnResult,
    turn: u64,
    background: usize,
) -> String {
    match final_text.map(reply_report).filter(|s| !s.is_empty()) {
        Some(s) => s,
        None => match result {
            TurnResult::Error => format!("Turn {turn} failed."),
            _ if background > 0 => format!(
                "Turn {turn} ended without a reply; {background} background task{} still \
                 running.",
                if background == 1 { "" } else { "s" }
            ),
            _ => format!("Turn {turn} finished without a reply."),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentences_and_caps() {
        assert_eq!(
            first_sentences("One. Two!  Three?\nFour. Five."),
            "One. Two! Three?"
        );
        assert_eq!(first_sentences("v1.2 is out. ok"), "v1.2 is out. ok");
        assert_eq!(first_sentences("no punctuation"), "no punctuation");
        let long = "word ".repeat(200);
        let s = first_sentences(&long);
        assert!(s.len() <= SUMMARY_MAX && s.ends_with("..."), "{s}");
        let multibyte = "é".repeat(500);
        assert!(first_sentences(&multibyte).len() <= SUMMARY_MAX);
        let t = trim_summary(&"word ".repeat(300), 600);
        assert!(t.len() <= 600 && t.len() > 400 && t.ends_with("..."), "{t}");
        assert_eq!(trim_summary("  A.  B. C. D.", 600), "A. B. C.");
    }

    #[test]
    fn control_characters_never_reach_a_summary() {
        assert_eq!(
            display_text("a\x1b[2Jb\tc\r\nd\u{9b}31m\x07e\x7f", true),
            "a[2Jb c \nd31me"
        );
        assert_eq!(display_text("one\ntwo", false), "one two");
        assert_eq!(display_text("plain é ✓", true), "plain é ✓");
        assert_eq!(
            trim_summary("done\x1b]0;pwned\x07 here.\nok", 600),
            "done]0;pwned here. ok"
        );
        assert_eq!(
            summary(Some("\x1b[31mred"), TurnResult::Completed, 1, 0),
            "[31mred"
        );
    }

    /// A long reply that ends with a markdown `## Summary` section: the section is the report.
    /// Setext headings (`Summary` underlined with `=` or `-`) are headings: the underline is
    /// not text, and the next one ends the section.
    #[test]
    fn setext_headings_are_headings() {
        for under in ["-------", "======="] {
            let reply =
                format!("Intro first.\n\nSummary\n{under}\nFixed X.\n\nNotes\n{under}\nMore.");
            assert_eq!(
                reply_summary(&reply).as_deref(),
                Some("Fixed X."),
                "{under}"
            );
        }
    }

    /// A plain title (`Tests:`) ends a label line's section; under a heading it is text.
    #[test]
    fn a_plain_title_ends_a_label_section() {
        let reply = "Summary:\nFixed the bug.\nTests:\nAll green.\nRisks:\nNone.";
        assert_eq!(reply_summary(reply).as_deref(), Some("Fixed the bug."));
        let reply = "## Summary\nI changed:\n- the parser\n- the docs";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("I changed: the parser; the docs")
        );
    }

    /// A bold sentence under a summary heading is part of it, not a new section's title.
    #[test]
    fn a_bold_sentence_is_summary_text() {
        let reply = "Some intro first.\n\n## Summary\n**Fixed the redirect bug.**\nAll tests \
                     pass.\n**This is a longer bold line that is clearly prose**\nDone.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some(
                "Fixed the redirect bug. All tests pass. This is a longer bold line that is \
                 clearly prose Done."
            )
        );
        // A short bold title still ends the section.
        let reply = "## Summary\nFixed it.\n**Next steps**\nDeploy.";
        assert_eq!(reply_summary(reply).as_deref(), Some("Fixed it."));
    }

    #[test]
    fn a_summary_heading_section_is_the_report() {
        let reply = "I looked at the login flow first.\n\n\
                     ## What I changed\n\
                     - `src/auth.rs`: the redirect now keeps the `next` parameter.\n\
                     - Added a test.\n\n\
                     ```bash\n# Summary\ncargo test\n```\n\n\
                     ## Summary\n\n\
                     - Fixed the login redirect losing `next`\n\
                     - **All 214 tests pass**\n\
                     - Nothing for you to do\n\n\
                     ## Next steps\n\
                     Deploy when ready.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some(
                "Fixed the login redirect losing `next`; All 214 tests pass; Nothing for you to do"
            )
        );
        assert_eq!(
            summary(Some(reply), TurnResult::Completed, 2, 0),
            "Fixed the login redirect losing `next`; All 214 tests pass; Nothing for you to do"
        );
        // `### Summary of changes` and a section that runs to the end.
        let reply = "Done.\n\n### Summary of changes:\nRenamed the crate.\n\nUpdated the docs.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("Renamed the crate. Updated the docs.")
        );
    }

    /// A bold `**Summary**` label line: its paragraph is the report.
    #[test]
    fn a_bold_summary_label_is_the_report() {
        let reply = "Here is what happened in detail. First I ran the tests. Then I read the \
                     logs.\n\n**Summary**\nThe flaky test was a race in the pool; I fixed \
                     it and it passed 50 times in a row.\n\nStill needs a deploy approval.";
        // A label line's section goes on past a blank line, like a heading's.
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some(
                "The flaky test was a race in the pool; I fixed it and it passed 50 times in a \
                 row. Still needs a deploy approval."
            )
        );
        // `**Summary:**` on its own line, a list under it, then another bold line.
        let reply = "Long story.\n**Summary:**\n1. Bumped tokio\n2) Fixed two warnings.\n\
                     **Risks**\nNone.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("Bumped tokio; Fixed two warnings.")
        );
    }

    /// `TL;DR:` inline, anywhere in the reply.
    #[test]
    fn an_inline_tldr_is_the_report() {
        let reply = "TL;DR: the migration is ready but needs a DB backup first.\n\nDetails: \
                     I wrote the migration. It drops two columns.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("the migration is ready but needs a DB backup first.")
        );
        let reply = "Lots of text.\n\n**Summary:** two files changed,\ntests pass.\n\nBye.";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("two files changed, tests pass.")
        );
        // A summary label wins over an earlier weaker one.
        let reply = "Status: still running the suite.\n\n## Summary\nAll green.";
        assert_eq!(reply_summary(reply).as_deref(), Some("All green."));
        // An inline label inside a section is part of it.
        let reply = "Text.\n## Summary\nFixed X.\nResult: all tests pass.\n## Notes\nn/a";
        assert_eq!(
            reply_summary(reply).as_deref(),
            Some("Fixed X. Result: all tests pass.")
        );
        // A weaker one counts when nothing else does.
        let reply = "Worked on it.\n\n## Result\n3 of 4 bugs fixed.";
        assert_eq!(reply_summary(reply).as_deref(), Some("3 of 4 bugs fixed."));
    }

    /// No section: the first sentences, as before. Look-alikes are no section.
    #[test]
    fn without_a_section_the_first_sentences_are_the_report() {
        for reply in [
            "Fixed the bug. Tests pass. Deployed it. Also cleaned up.",
            "Results are in. Tests pass. Deployed it. Also cleaned up.",
            "Summary of the work is below. Tests pass. Deployed it. More.",
            "## Summary\n\n## Details\nNothing here.",
            "Done.\n```\n## Summary\nnot a heading\n```",
        ] {
            assert_eq!(reply_summary(reply), None, "{reply:?}");
            assert_eq!(reply_report(reply), first_sentences(reply), "{reply:?}");
        }
        assert_eq!(
            summary(
                Some("Fixed the bug. Tests pass. Deployed it. Also cleaned up."),
                TurnResult::Completed,
                1,
                0
            ),
            "Fixed the bug. Tests pass. Deployed it."
        );
    }

    /// A long section is cut at SUMMARY_MAX (more than three sentences are kept below it).
    #[test]
    fn a_long_summary_section_is_cut() {
        let reply = format!(
            "Intro.\n## TL;DR\nOne. Two. Three. Four.\n{}",
            "é word ".repeat(200)
        );
        let s = reply_report(&reply);
        assert!(s.starts_with("One. Two. Three. Four. é word"), "{s}");
        assert!(s.len() <= SUMMARY_MAX && s.ends_with("..."), "{s}");
        assert_eq!(reply_report("## Summary\nshort\x1b[2J one"), "short[2J one");
    }

    /// Linear and bounded: a reply of 30k consecutive inline labels (each one a section
    /// running to the paragraph's end; read once per label, that took minutes) is read once,
    /// and only what the cut summary needs is joined; a summary label after them still wins. A
    /// section cut early reads as the whole one cut.
    #[test]
    fn many_label_lines_are_read_in_one_pass() {
        let lines = "Result: x\n".repeat(30_000);
        let s = reply_summary(&lines).unwrap();
        assert!(s.starts_with("x Result: x Result: x"), "{s}");
        assert!(s.len() < 2 * SUMMARY_MAX, "{}", s.len());
        let r = reply_report(&lines);
        assert!(r.len() <= SUMMARY_MAX && r.ends_with("..."), "{r}");
        let reply = format!("{lines}\n## Summary\nAll good.");
        assert_eq!(reply_summary(&reply).as_deref(), Some("All good."));
        let words = "- **word**\n".repeat(10_000);
        let full = vec!["word"; 10_000].join("; ");
        assert_eq!(
            reply_report(&format!("## TL;DR\n{words}")),
            cut_summary(&full)
        );
    }

    /// The summary is the reply's, or a stand-in, and nothing else: no way to open the thread
    /// (reports used to end with `— twr attach <name>`).
    #[test]
    fn summary_is_the_reply_or_a_stand_in() {
        assert_eq!(
            summary(Some("hello world"), TurnResult::Completed, 1, 0),
            "hello world"
        );
        assert_eq!(summary(None, TurnResult::Error, 2, 0), "Turn 2 failed.");
        assert_eq!(
            summary(Some("  "), TurnResult::Completed, 3, 0),
            "Turn 3 finished without a reply."
        );
        assert_eq!(
            summary(None, TurnResult::Completed, 4, 2),
            "Turn 4 ended without a reply; 2 background tasks still running."
        );
        assert_eq!(
            summary(None, TurnResult::Completed, 5, 1),
            "Turn 5 ended without a reply; 1 background task still running."
        );
    }
}

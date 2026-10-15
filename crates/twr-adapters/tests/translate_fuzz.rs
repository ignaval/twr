//! Invariant fuzz for the approval cards of the three stdout translators (Claude Code, Codex,
//! ACP): a translator decides what a permission item shows, and a mis-parse could show a
//! different command from the one being approved.
//!
//! Each case replays a recorded fixture (`tests/fixtures/{claude,codex,grok}`; for the
//! standard ACP dialect a scripted session) through the translator's public entry points,
//! with its approval requests rewritten and multiplied (replacements under the same id, more
//! under new ids) and the other lines now and then mutated: a JSON value replaced with a
//! random one, a key dropped or added, bytes flipped, inserted or dropped, the line cut short,
//! dropped or repeated. Commands, titles, grant roots and tool inputs are drawn from printable
//! ASCII, spaces at the ends, newlines, carriage returns and tabs, other control characters,
//! format characters (zero-width ones, bidirectional controls, the soft hyphen, the BOM),
//! non-ASCII whitespace, combining marks, non-ASCII letters and digits, the card's own marks
//! (`⏎`, `⟨U+0041⟩`, `…`), private use and unassigned code points, and lengths around every cut
//! (the summary's and the 64 KiB of [`FULL_INPUT_MAX`]). Codex commands come wrapped in
//! `bash -lc` / `sh -c` with single or double quotes, nested, or broken.
//!
//! The reference here is the card's escaping rule (`render_command`'s documentation) written
//! out independently: printable ASCII and inner spaces show as themselves, so do non-ASCII
//! letters and digits; a newline is `⏎`, a carriage return `␍`, a tab `⇥`; anything else
//! (an end space too) is `⟨U+XXXX⟩`. A shown command is decoded back with it, and the
//! command each runtime would run is read from the request line the item came from (its raw
//! line), not from the translator. Invariants, for every permission item raised:
//!
//! - (a) no translator panics, whatever the line;
//! - (b) the summary's command (and the item's `full_input`, when the summary shows only its
//!   start) decodes back exactly to the command that runs, in the canonical marks: nothing
//!   invisible shows as itself; a cut summary ends `… N more characters; open the card to see
//!   all`, cut at the last whole mark that fits, with N right;
//! - (c) when even the whole marked command is past [`FULL_INPUT_MAX`] bytes, `full_input` is
//!   its longest prefix of whole marks that fits plus `… M more characters not shown`, and the
//!   item is deny-only (its summary says so, its options are `deny` alone);
//! - (d) an `allow` answer to the item approves exactly the request it was raised for (Claude:
//!   `updatedInput` is the request's input; ACP: the request's first `allow_once` option), and
//!   to a deny-only item it denies.
//!
//! What runs, per runtime:
//!
//! - Claude Code (`can_use_tool`): a `Bash` request runs its input's `command` with its flags
//!   (`dangerouslyDisableSandbox`, `run_in_background`, `allowed_domains`, fields twr does not
//!   know), and the allow answer sends the input back as `updatedInput`. The card shows the
//!   subject (the command; for other tools the path of a file tool, a search's pattern, the
//!   JSON of an unknown tool's input) and then every other field of the input but a Bash
//!   call's `description` and `timeout`, as JSON between `⟦` `⟧`; both parts must read back,
//!   so nothing in the input that changes what is approved is off the card.
//! - Codex (`item/commandExecution/requestApproval`): Codex runs `params.command`. The card
//!   may show it verbatim, or the script of a `<shell> -c` / `-lc` wrapper, which the
//!   reference reads with POSIX quoting (a separate parser: words, single and double quotes,
//!   backslashes; anything with an unquoted operator or expansion is not one plain word): only
//!   when the command is exactly `<shell> <flag> <one word>` may the card show the word. A
//!   `grantRoot` (write access under it for the session) reads back the same way, or makes the
//!   item deny-only when its sentence is too long.
//! - ACP (`session/request_permission`): the user approves the tool call the request names;
//!   its `title` shows (or, without one, the JSON of its `rawInput`).
//!
//! The labels a runtime supplies (Claude's tool name, an ACP call's `kind`) must show marked by
//! the same rule.
//!
//! `TWR_FUZZ_SEED` (default: fixed) and `TWR_FUZZ_CASES` (default 300 per runtime; the
//! ignored long run does 100k) set the run; `TWR_FUZZ_CASE=n` runs case n alone. A failure
//! prints the seed, the case, the line and the item. The long run:
//! `cargo test -p twr-adapters --release --test translate_fuzz -- --ignored`.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use twr_adapters::acp::translate::INPUT_SUMMARY_MAX as ACP_SUMMARY_MAX;
use twr_adapters::claude_code::translate::{
    FULL_INPUT_KEY, FULL_INPUT_MAX, INPUT_EXTRAS_MAX, PERMISSION_INPUT_MAX,
};
use twr_adapters::codex::translate::GRANT_SUMMARY_MAX;
use twr_adapters::{acp, claude_code, codex, grok_build};
use twr_core::adapter::{Answer, SpawnConfig};
use twr_core::event::{AttentionRaised, AttentionReason, EventBody};

const DEFAULT_SEED: u64 = 0x7472_616e_736c_6174;
const DEFAULT_CASES: u64 = 300;
const LONG_CASES: u64 = 100_000;

// ---------------------------------------------------------------------------------------------
// Random numbers
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    /// The generator of case `case` of a run seeded `seed`: each case replays on its own.
    fn for_case(seed: u64, case: u64) -> Rng {
        let mut r = Rng((seed ^ case.wrapping_mul(0x9e37_79b9_7f4a_7c15)) | 1);
        for _ in 0..4 {
            r.next();
        }
        r
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

// ---------------------------------------------------------------------------------------------
// Text to show
// ---------------------------------------------------------------------------------------------

/// Control characters other than the three with marks of their own.
const CONTROLS: &[char] = &[
    '\0', '\u{1}', '\u{7}', '\u{8}', '\u{b}', '\u{c}', '\u{1b}', '\u{1f}', '\u{7f}', '\u{80}',
    '\u{85}', '\u{9b}', '\u{9f}',
];
/// Whitespace and separators outside ASCII.
const SPACES: &[char] = &[
    '\u{a0}', '\u{1680}', '\u{2000}', '\u{2007}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}',
    '\u{202f}', '\u{205f}', '\u{3000}',
];
/// Format characters: zero-width, bidirectional controls, the soft hyphen, the BOM, tags.
const FORMAT: &[char] = &[
    '\u{ad}',
    '\u{61c}',
    '\u{180e}',
    '\u{200b}',
    '\u{200c}',
    '\u{200d}',
    '\u{200e}',
    '\u{200f}',
    '\u{202a}',
    '\u{202b}',
    '\u{202c}',
    '\u{202d}',
    '\u{202e}',
    '\u{2060}',
    '\u{2064}',
    '\u{2066}',
    '\u{2067}',
    '\u{2068}',
    '\u{2069}',
    '\u{feff}',
    '\u{fff9}',
    '\u{1bca0}',
    '\u{1d173}',
    '\u{e0001}',
    '\u{e0041}',
];
/// Combining marks and variation selectors.
const COMBINING: &[char] = &[
    '\u{300}',
    '\u{301}',
    '\u{308}',
    '\u{36f}',
    '\u{483}',
    '\u{94d}',
    '\u{20d0}',
    '\u{20dd}',
    '\u{fe00}',
    '\u{fe0f}',
    '\u{e0100}',
];
/// Symbols and punctuation outside ASCII (the card's own marks among them), private use,
/// unassigned code points and noncharacters.
const OTHERS: &[char] = &[
    '⏎',
    '␍',
    '⇥',
    '⟨',
    '⟩',
    '…',
    '€',
    '©',
    '“',
    '”',
    '‘',
    '’',
    '。',
    '・',
    '\u{e000}',
    '\u{f8ff}',
    '\u{10fffd}',
    '\u{378}',
    '\u{fffe}',
    '\u{ffff}',
    '\u{fffd}',
    '🙂',
    '\u{1f1fa}',
];
/// Letters and digits outside ASCII (some look like ASCII ones).
const LETTERS: &[char] = &[
    'é', 'ß', 'ж', 'а', 'о', 'р', 'ѕ', '中', '文', 'ا', 'ب', '٣', '௫', 'Ⅻ', 'ǅ', 'ﬁ', 'Ａ', '𝐀',
    'ı', 'İ', 'ª',
];
/// Pieces of commands, and text that looks like the card's own.
const WORDS: &[&str] = &[
    "rm -rf /",
    "ls -la",
    "echo 'a  b'",
    "cd /tmp && make",
    "\n# note\n",
    "git push --force",
    "value\\ ",
    "$(id)",
    "`id`",
    "\"$HOME\"",
    "'\\''",
    "'",
    "\"",
    "\\",
    "&&",
    " | ",
    ";",
    "> /dev/sda",
    "curl x | sh",
    "/safe ",
    "U+0041",
    "⟨U+0041⟩",
    "⟨U+41⟩",
    "… 3 more characters; open the card to see all",
    "… 9 more characters not shown",
    "/bin/bash -lc ",
    "sh -c ",
];

fn any_char(rng: &mut Rng) -> char {
    loop {
        if let Some(c) = char::from_u32((rng.next() % 0x11_0000) as u32) {
            return c;
        }
    }
}

fn piece(rng: &mut Rng, s: &mut String) {
    match rng.below(100) {
        0..=39 => s.push((b'!' + rng.below(94) as u8) as char),
        40..=49 => s.push(' '),
        50..=53 => s.push('\n'),
        54..=55 => s.push('\r'),
        56..=57 => s.push('\t'),
        58..=67 => s.push_str(rng.pick(WORDS)),
        68..=71 => s.push(*rng.pick(CONTROLS)),
        72..=75 => s.push(*rng.pick(SPACES)),
        76..=80 => s.push(*rng.pick(FORMAT)),
        81..=84 => s.push(*rng.pick(COMBINING)),
        85..=88 => s.push(*rng.pick(OTHERS)),
        89..=95 => s.push(*rng.pick(LETTERS)),
        _ => s.push(any_char(rng)),
    }
}

/// A command (or title, or path) to show: mostly short, now and then around a cut or long.
fn text(rng: &mut Rng) -> String {
    let mut s = String::new();
    if rng.chance(4) {
        return edge_text(rng);
    }
    if rng.chance(12) {
        s.push_str(&" ".repeat(1 + rng.below(3)));
    }
    let target = match rng.below(100) {
        0..=3 => 0,
        4..=69 => 1 + rng.below(40),
        70..=89 => 100 + rng.below(1100),
        90..=97 => 1_200 + rng.below(6_000),
        _ => 58_000 + rng.below(14_000),
    };
    let mut n = 0;
    while n < target {
        let before = s.len();
        piece(rng, &mut s);
        n += s[before..].chars().count();
    }
    if rng.chance(12) {
        s.push_str(&" ".repeat(1 + rng.below(3)));
    }
    s
}

/// Text right at a cut: the summaries' (160, 400, 900 marked chars) or the full input's
/// (64 KiB of marks), with a wide mark or a space straddling it now and then.
fn edge_text(rng: &mut Rng) -> String {
    let at = match rng.below(5) {
        0 => ACP_SUMMARY_MAX,
        1 => 400,
        2 => PERMISSION_INPUT_MAX,
        3 => GRANT_SUMMARY_MAX - 100,
        _ => FULL_INPUT_MAX,
    };
    let n = (at + 3).saturating_sub(rng.below(7));
    let mut s = "a".repeat(n.saturating_sub(1));
    match rng.below(4) {
        0 => s.push('a'),
        1 => s.push('\u{202e}'),
        2 => {
            s.insert(rng.below(s.len().max(1)), '\n');
        }
        _ => s.push(' '),
    }
    s
}

// ---------------------------------------------------------------------------------------------
// The reference: the card's escaping rule, and reading a card back
// ---------------------------------------------------------------------------------------------

/// How a character of a command shows on the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    /// As itself.
    Itself,
    /// `⏎`, `␍` or `⇥`.
    Mark,
    /// `⟨U+XXXX⟩`.
    Escape,
}

/// Whether `c` shows as itself by the rule (an inner space too; an end space does not).
fn plain(c: char) -> bool {
    ('!'..='~').contains(&c) || (!c.is_ascii() && c.is_alphanumeric())
}

/// Every character of `s` with how it shows and the length of its mark (chars, bytes).
fn marks_of(s: &str) -> Vec<(char, How, usize, usize)> {
    let chars: Vec<char> = s.chars().collect();
    let lead = chars.iter().take_while(|&&c| c == ' ').count();
    let trail = chars.iter().rev().take_while(|&&c| c == ' ').count();
    let n = chars.len();
    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let end = i < lead || i >= n - trail;
            let how = match c {
                '\n' | '\r' | '\t' => How::Mark,
                ' ' if end => How::Escape,
                ' ' => How::Itself,
                c if plain(c) => How::Itself,
                _ => How::Escape,
            };
            let (chars, bytes) = match how {
                How::Itself => (1, c.len_utf8()),
                How::Mark => (1, 3),
                How::Escape => {
                    let hex = format!("{:04X}", u32::from(c)).len();
                    (4 + hex, 8 + hex)
                }
            };
            (c, how, chars, bytes)
        })
        .collect()
}

/// Reads marked text back: each character and how it was shown, or why it cannot be read
/// (something shown as itself that the rule never shows so, a broken or non-canonical
/// escape).
fn decode(s: &str) -> Result<Vec<(char, How)>, String> {
    let mut out = Vec::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '⏎' => out.push(('\n', How::Mark)),
            '␍' => out.push(('\r', How::Mark)),
            '⇥' => out.push(('\t', How::Mark)),
            '⟨' => {
                if it.next() != Some('U') || it.next() != Some('+') {
                    return Err(format!("a ⟨ that starts no escape after {}", read(&out)));
                }
                let mut hex = String::new();
                loop {
                    match it.next() {
                        Some('⟩') => break,
                        Some(h @ ('0'..='9' | 'A'..='F')) if hex.len() < 6 => hex.push(h),
                        other => {
                            return Err(format!(
                                "a broken escape ⟨U+{hex}… then {other:?} after {}",
                                read(&out)
                            ))
                        }
                    }
                }
                let v = u32::from_str_radix(&hex, 16).map_err(|e| format!("⟨U+{hex}⟩: {e}"))?;
                if format!("{v:04X}") != hex {
                    return Err(format!("a non-canonical escape ⟨U+{hex}⟩"));
                }
                let ch = char::from_u32(v).ok_or_else(|| format!("⟨U+{hex}⟩ is no character"))?;
                out.push((ch, How::Escape));
            }
            ' ' => out.push((' ', How::Itself)),
            c if plain(c) => out.push((c, How::Itself)),
            c => {
                return Err(format!(
                    "U+{:04X} shown as itself after {}",
                    u32::from(c),
                    read(&out)
                ))
            }
        }
    }
    Ok(out)
}

/// The text of decoded marks (for messages), cut.
fn read(d: &[(char, How)]) -> String {
    let s: String = d.iter().map(|x| x.0).collect();
    format!("{:?}", cut(&s, 120))
}

fn cut(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…[{} bytes]", &s[..i], s.len()),
        None => s.to_string(),
    }
}

/// Whether decoded marks are exactly the first `d.len()` marks of `subject`.
fn is_prefix(d: &[(char, How)], marks: &[(char, How, usize, usize)]) -> bool {
    d.len() <= marks.len() && d.iter().zip(marks).all(|(a, m)| a.0 == m.0 && a.1 == m.1)
}

/// Checks the rendering of `subject` on a card whose summary line shows at most `max` marked
/// chars: `line` (the summary's part after the label) and `full` (the item's `full_input`).
/// Ok(true) when the whole is past [`FULL_INPUT_MAX`] (the item must be deny-only).
fn check_rendering(
    line: &str,
    full: Option<&str>,
    subject: &str,
    max: usize,
) -> Result<bool, String> {
    if !check_line(line, subject, max)? {
        if full.is_some() {
            return Err("a full input next to a summary that shows all".into());
        }
        return Ok(false);
    }
    check_full(full.ok_or("a cut summary without the full input")?, subject)
}

/// Whether `s` reads back as all of `marks`.
fn reads_whole(
    s: &str,
    marks: &[(char, How, usize, usize)],
    subject: &str,
    what: &str,
) -> Result<(), String> {
    let d = decode(s).map_err(|e| format!("{what}: {e}"))?;
    if d.len() == marks.len() && is_prefix(&d, marks) {
        Ok(())
    } else {
        Err(format!(
            "{what} reads {} where the command is {:?}",
            read(&d),
            cut(subject, 120)
        ))
    }
}

/// Checks a summary's line for `subject` (at most `max` marked chars): Ok(whether it is cut).
fn check_line(line: &str, subject: &str, max: usize) -> Result<bool, String> {
    let marks = marks_of(subject);
    let total: usize = marks.iter().map(|m| m.2).sum();
    if total <= max {
        reads_whole(line, &marks, subject, "the summary")?;
        return Ok(false);
    }
    // Cut: `<start>… N more characters; open the card to see all`.
    let (start, rest) = line.split_once('…').ok_or_else(|| {
        format!(
            "a summary of {total} marked chars, not cut: {}",
            cut(line, 120)
        )
    })?;
    let n: usize = rest
        .strip_prefix(' ')
        .and_then(|r| r.strip_suffix(" more characters; open the card to see all"))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("a cut summary ending {:?}", cut(rest, 120)))?;
    let d = decode(start).map_err(|e| format!("the cut summary: {e}"))?;
    if !is_prefix(&d, &marks) {
        return Err(format!(
            "the cut summary reads {}, not the start of {:?}",
            read(&d),
            cut(subject, 120)
        ));
    }
    let k = d.len();
    let shown: usize = marks[..k].iter().map(|m| m.2).sum();
    if shown > max || (k < marks.len() && shown + marks[k].2 <= max) {
        return Err(format!(
            "the summary is cut after {shown} marked chars (of {max} room, next mark {:?})",
            marks.get(k)
        ));
    }
    if n != total - shown {
        return Err(format!(
            "the cut summary says {n} more, not {}",
            total - shown
        ));
    }
    Ok(true)
}

/// Checks a full input for `subject`: Ok(true) when it is past [`FULL_INPUT_MAX`] (capped).
fn check_full(full: &str, subject: &str) -> Result<bool, String> {
    let marks = marks_of(subject);
    let total: usize = marks.iter().map(|m| m.2).sum();
    let bytes: usize = marks.iter().map(|m| m.3).sum();
    if bytes <= FULL_INPUT_MAX {
        reads_whole(full, &marks, subject, "the full input")?;
        return Ok(false);
    }
    // Too long even whole: `<start>… M more characters not shown`, and only deny.
    let (start, rest) = full
        .split_once('…')
        .ok_or("a full input past the cap without its `…`")?;
    let m: usize = rest
        .strip_prefix(' ')
        .and_then(|r| r.strip_suffix(" more characters not shown"))
        .and_then(|m| m.parse().ok())
        .ok_or_else(|| format!("a capped full input ending {:?}", cut(rest, 120)))?;
    let d = decode(start).map_err(|e| format!("the capped full input: {e}"))?;
    if !is_prefix(&d, &marks) {
        return Err("the capped full input is not the start of the command".into());
    }
    let k = d.len();
    let kept: usize = marks[..k].iter().map(|m| m.3).sum();
    if kept > FULL_INPUT_MAX || (k < marks.len() && kept + marks[k].3 <= FULL_INPUT_MAX) {
        return Err(format!("the full input is capped after {kept} bytes"));
    }
    let kept_chars: usize = marks[..k].iter().map(|m| m.2).sum();
    if m != total - kept_chars {
        return Err(format!(
            "the capped full input says {m} more, not {}",
            total - kept_chars
        ));
    }
    Ok(true)
}

/// The marked bytes of `s` (whole).
fn marked_bytes(s: &str) -> usize {
    marks_of(s).iter().map(|m| m.3).sum()
}

/// `s` marked by the rule, whole: an encoder written from the same table as the reader.
fn mark_whole(s: &str) -> String {
    marks_of(s)
        .into_iter()
        .map(|(c, how, ..)| match (how, c) {
            (How::Itself, c) => c.to_string(),
            (How::Mark, '\n') => "⏎".into(),
            (How::Mark, '\r') => "␍".into(),
            (How::Mark, _) => "⇥".into(),
            (How::Escape, c) => format!("⟨U+{:04X}⟩", u32::from(c)),
        })
        .collect()
}

/// Splits a card text `<subject> ⟦<other fields>⟧` (or `⟦…⟧` alone) at the one raw `⟦`, which
/// the marks never show as itself.
fn split_fields(s: &str) -> Result<(&str, Option<&str>), String> {
    let Some(i) = s.find('⟦') else {
        return Ok((s, None));
    };
    let fields = s[i + '⟦'.len_utf8()..]
        .strip_suffix('⟧')
        .ok_or("the input's other fields do not end the card text with ⟧")?;
    let subject = match &s[..i] {
        "" => "",
        h => h
            .strip_suffix(' ')
            .ok_or("no space before the input's other fields")?,
    };
    Ok((subject, Some(fields)))
}

/// The words of a POSIX shell command line, quotes removed; None when it is anything but
/// plain words (an unquoted operator, expansion, glob or comment, an unterminated quote).
fn shell_words(s: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut cur: Option<String> = None;
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' | '\n' => {
                if let Some(w) = cur.take() {
                    words.push(w);
                }
            }
            '\'' => {
                let w = cur.get_or_insert_with(String::new);
                loop {
                    match it.next()? {
                        '\'' => break,
                        c => w.push(c),
                    }
                }
            }
            '"' => {
                let w = cur.get_or_insert_with(String::new);
                loop {
                    match it.next()? {
                        '"' => break,
                        '\\' => match it.next()? {
                            c @ ('$' | '`' | '"' | '\\') => w.push(c),
                            '\n' => {}
                            c => {
                                w.push('\\');
                                w.push(c);
                            }
                        },
                        '$' | '`' => return None,
                        c => w.push(c),
                    }
                }
            }
            '\\' => match it.next()? {
                '\n' => {}
                c => cur.get_or_insert_with(String::new).push(c),
            },
            c if "|&;<>()$`*?[]#~{}=!".contains(c) => return None,
            c => cur.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(cur);
    Some(words)
}

/// The script a command runs when it is exactly `<shell> -c|-lc <one word>`.
fn shell_script(command: &str) -> Option<String> {
    let w = shell_words(command)?;
    let shell = w.first()?.rsplit('/').next()?;
    (w.len() == 3
        && matches!(shell, "sh" | "bash" | "zsh" | "dash")
        && matches!(w[1].as_str(), "-c" | "-lc"))
    .then(|| w[2].clone())
}

/// Splits a summary `Allow <label>: <line>` / `Allow <label>?` into its line.
fn after_label<'a>(summary: &'a str, head: &str) -> Result<&'a str, String> {
    let rest = summary
        .strip_prefix(head)
        .ok_or_else(|| format!("the summary does not start {head:?}"))?;
    if rest == "?" {
        return Ok("");
    }
    rest.strip_prefix(": ")
        .ok_or_else(|| format!("no `: ` after {head:?}"))
}

fn ext_str<'a>(a: &'a AttentionRaised, section: &str, key: &str) -> Option<&'a str> {
    a.ext.as_ref()?.get(section)?.get(key)?.as_str()
}

fn options(a: &AttentionRaised) -> Vec<&str> {
    a.options.iter().map(String::as_str).collect()
}

/// The options an item must offer: `deny` alone, or `allow` and `deny`.
fn check_options(a: &AttentionRaised, deny_only: bool) -> Result<(), String> {
    let want: &[&str] = if deny_only {
        &["deny"]
    } else {
        &["allow", "deny"]
    };
    if options(a) != want {
        return Err(format!("options {:?}, expected {want:?}", a.options));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------------------------

fn random_value(rng: &mut Rng, depth: usize) -> Value {
    match rng.below(if depth > 2 { 6 } else { 9 }) {
        0 => Value::Null,
        1 => json!(rng.chance(50)),
        2 => json!(rng.next() as i64 >> rng.below(64)),
        3 => json!((rng.next() % 2000) as f64 / 7.0 - 100.0),
        4 | 5 => json!(text(rng)),
        6 | 7 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_value(rng, depth + 1))
                .collect(),
        ),
        _ => Value::Object(
            (0..rng.below(4))
                .map(|_| (short_key(rng), random_value(rng, depth + 1)))
                .collect(),
        ),
    }
}

const KEYS: &[&str] = &[
    "command",
    "title",
    "kind",
    "rawInput",
    "input",
    "tool_name",
    "request_id",
    "id",
    "method",
    "params",
    "type",
    "subtype",
    "grantRoot",
    "options",
    "optionId",
    "turnId",
    "itemId",
    "sessionId",
    "result",
    "error",
    "x",
];

fn short_key(rng: &mut Rng) -> String {
    if rng.chance(80) {
        rng.pick(KEYS).to_string()
    } else {
        let mut s = String::new();
        piece(rng, &mut s);
        s
    }
}

/// Every JSON pointer in `v` (up to a bound).
fn pointers(v: &Value, at: String, out: &mut Vec<String>) {
    if out.len() > 400 {
        return;
    }
    out.push(at.clone());
    match v {
        Value::Object(m) => {
            for (k, x) in m {
                let k = k.replace('~', "~0").replace('/', "~1");
                pointers(x, format!("{at}/{k}"), out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                pointers(x, format!("{at}/{i}"), out);
            }
        }
        _ => {}
    }
}

/// One random edit of a JSON value somewhere in `v`.
fn mutate_value(rng: &mut Rng, v: &mut Value) {
    let mut ps = Vec::new();
    pointers(v, String::new(), &mut ps);
    let p = rng.pick(&ps).clone();
    let Some(node) = v.pointer_mut(&p) else {
        return;
    };
    match rng.below(8) {
        0 | 1 => *node = random_value(rng, 0),
        2 => {
            if let Value::Object(m) = node {
                if let Some(k) = m.keys().nth(rng.below(m.len())).cloned() {
                    m.remove(&k);
                }
            }
        }
        3 => {
            if let Value::Object(m) = node {
                m.insert(short_key(rng), random_value(rng, 1));
            } else if let Value::Array(a) = node {
                a.insert(rng.below(a.len() + 1), random_value(rng, 1));
            }
        }
        4 => {
            if let Value::String(s) = node {
                let mut c: Vec<char> = s.chars().collect();
                let i = rng.below(c.len() + 1);
                match rng.below(3) {
                    0 => c.truncate(i),
                    1 => {
                        let mut p = String::new();
                        piece(rng, &mut p);
                        for (j, ch) in p.chars().enumerate() {
                            c.insert(i + j, ch);
                        }
                    }
                    _ => {
                        if i < c.len() {
                            c.remove(i);
                        }
                    }
                }
                *s = c.into_iter().collect();
            }
        }
        5 => {
            // Another type for the same value.
            *node = match node.take() {
                Value::String(s) => json!([s]),
                Value::Number(n) => json!(n.to_string()),
                Value::Bool(b) => json!(b as u8),
                Value::Null => json!(""),
                other => json!(other.to_string()),
            };
        }
        6 => *node = json!(text(rng)),
        _ => {
            if let Value::Array(a) = node {
                if !a.is_empty() {
                    let x = a[rng.below(a.len())].clone();
                    a.push(x);
                }
            }
        }
    }
}

/// Byte-level damage: bits flipped, bytes inserted or dropped, the line cut short.
fn mutate_bytes(rng: &mut Rng, line: &str) -> String {
    let mut b = line.as_bytes().to_vec();
    for _ in 0..1 + rng.below(3) {
        let i = rng.below(b.len() + 1);
        match rng.below(5) {
            0 if i < b.len() => b[i] ^= 1 << rng.below(8),
            1 => b.insert(i, rng.next() as u8),
            2 if i < b.len() => {
                b.remove(i);
            }
            3 => b.truncate(i),
            _ => b.insert(i, *rng.pick(b"{}[]\",:\\")),
        }
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// What a recorded line that is not an approval request becomes: itself mostly, now and then
/// mutated, dropped or repeated.
fn other_line(rng: &mut Rng, st: &mut Stats, line: &str) -> Vec<String> {
    match rng.below(100) {
        0..=5 => match serde_json::from_str::<Value>(line) {
            Ok(mut v) => {
                mutate_value(rng, &mut v);
                st.mutated += 1;
                vec![v.to_string()]
            }
            Err(_) => vec![line.to_string()],
        },
        6..=8 => {
            st.mutated += 1;
            vec![mutate_bytes(rng, line)]
        }
        9 => {
            st.mutated += 1;
            Vec::new()
        }
        10 => vec![line.to_string(), line.to_string()],
        _ => vec![line.to_string()],
    }
}

// ---------------------------------------------------------------------------------------------
// Fixtures and runs
// ---------------------------------------------------------------------------------------------

/// (from the runtime, line) of a recorded fixture.
fn fixture(dir: &str, name: &str) -> Vec<(bool, String)> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(format!("{name}.both.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| (l.starts_with("<<"), l[3..].to_string()))
        .collect()
}

#[derive(Debug, Default)]
struct Stats {
    lines: u64,
    mutated: u64,
    /// Permission items checked, by what they showed.
    items: BTreeMap<&'static str, u64>,
    allows: u64,
    denied_allows: u64,
}

impl Stats {
    fn count(&mut self, what: &'static str) {
        *self.items.entry(what).or_default() += 1;
    }
    fn n(&self, what: &str) -> u64 {
        self.items.get(what).copied().unwrap_or(0)
    }
}

/// The run's context for failure messages: the last line fed.
#[derive(Default)]
struct Trail {
    line: String,
    item: String,
}

type CaseFn = fn(&mut Rng, &mut Stats, &mut Trail) -> Result<(), String>;

fn env_u64(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

fn run_fuzz(runtime: &str, case_fn: CaseFn, default_cases: u64) -> Stats {
    let seed = env_u64("TWR_FUZZ_SEED").unwrap_or(DEFAULT_SEED);
    let cases = env_u64("TWR_FUZZ_CASES").unwrap_or(default_cases);
    let only = env_u64("TWR_FUZZ_CASE");
    eprintln!("{runtime} translate fuzz: TWR_FUZZ_SEED={seed:#x} TWR_FUZZ_CASES={cases}");
    let started = std::time::Instant::now();
    let mut st = Stats::default();
    for case in 0..cases {
        if only.is_some_and(|o| o != case) {
            continue;
        }
        let mut rng = Rng::for_case(seed, case);
        let mut trail = Trail::default();
        let r = catch_unwind(AssertUnwindSafe(|| case_fn(&mut rng, &mut st, &mut trail)));
        let what = match r {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => format!("invariant violated: {e}"),
            Err(p) => format!(
                "panicked: {}",
                p.downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| p.downcast_ref::<&str>().copied())
                    .unwrap_or("?")
            ),
        };
        panic!(
            "{runtime} translate fuzz (TWR_FUZZ_SEED={seed:#x} TWR_FUZZ_CASE={case}): {what}\n\
             line: {}\nitem: {}",
            cut(&trail.line, 3000),
            cut(&trail.item, 3000)
        );
    }
    eprintln!(
        "{runtime} translate fuzz: {:?} in {:.1}s",
        st,
        started.elapsed().as_secs_f64()
    );
    st
}

/// The generator must reach the interesting cards, or the fuzz proves little.
fn assert_reached(st: &Stats, cases: u64, what: &[&str]) {
    let cases = env_u64("TWR_FUZZ_CASES").unwrap_or(cases);
    if env_u64("TWR_FUZZ_CASE").is_some() || cases < 100 {
        return;
    }
    for w in what {
        assert!(st.n(w) > 0, "never reached: {w} ({st:?})");
    }
    assert!(st.allows > 0 && st.denied_allows > 0, "{st:?}");
}

// ---------------------------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------------------------

const CLAUDE_FIXTURES: &[&str] = &[
    "session_allow_multiturn",
    "session_deny",
    "write_allow",
    "interrupt",
    "ask_user_question",
];

const CLAUDE_TOOLS: &[&str] = &[
    "Bash",
    "Bash",
    "Bash",
    "Bash",
    "Bash",
    "Bash",
    "Bash",
    "Bash",
    "Write",
    "Edit",
    "Read",
    "NotebookEdit",
    "WebFetch",
    "WebSearch",
    "Glob",
    "Grep",
    "Task",
    "ExitPlanMode",
    "mcp__srv__run",
    "Unknown",
    "TodoWrite",
    "MultiEdit",
    "Agent",
];

/// A `can_use_tool` request under `id`.
fn claude_request(rng: &mut Rng, id: &str) -> Value {
    let tool = *rng.pick(CLAUDE_TOOLS);
    let mut input = match tool {
        "Bash" => json!({"command": text(rng), "description": "run it", "timeout": 120000}),
        "Write" => json!({"file_path": text(rng), "content": text(rng)}),
        "Edit" | "Read" => json!({"file_path": text(rng), "old_string": "a", "new_string": "b"}),
        "NotebookEdit" => json!({"notebook_path": text(rng), "new_source": "x"}),
        "WebFetch" => json!({"url": text(rng), "prompt": "read"}),
        "WebSearch" => json!({"query": text(rng)}),
        "Glob" | "Grep" => {
            let mut m = json!({"pattern": text(rng)});
            if rng.chance(50) {
                m["path"] = json!(text(rng));
            }
            m
        }
        "Task" => json!({"description": text(rng), "prompt": "go"}),
        "ExitPlanMode" => json!({"plan": text(rng)}),
        _ => match random_value(rng, 0) {
            Value::Object(mut m) => {
                m.insert("arg".into(), json!(text(rng)));
                Value::Object(m)
            }
            other => other,
        },
    };
    if rng.chance(8) {
        mutate_value(rng, &mut input);
    }
    // Fields beyond the subject: Claude Code's Bash flags (2.1.296: `run_in_background`,
    // `dangerouslyDisableSandbox`, `allowed_domains`, the internal `_simulatedSedEdit`) and
    // fields twr does not know.
    if let Some(m) = input.as_object_mut().filter(|_| rng.chance(35)) {
        for _ in 0..1 + rng.below(3) {
            let (k, v) = match rng.below(6) {
                0 => ("dangerouslyDisableSandbox", json!(rng.chance(80))),
                1 => ("run_in_background", json!(true)),
                2 => (
                    "allowed_domains",
                    json!((0..rng.below(60))
                        .map(|i| format!("h{i}.example"))
                        .collect::<Vec<_>>()),
                ),
                3 => (
                    "_simulatedSedEdit",
                    json!({"filePath": text(rng), "newContent": text(rng)}),
                ),
                4 => ("future\u{202e}flag", json!(text(rng))),
                _ => ("timeout", random_value(rng, 1)),
            };
            m.insert(k.into(), v);
        }
    }
    // Now and then a tool name with odd characters (an MCP server names its tools).
    let tool_name = if rng.chance(5) {
        format!("mcp__s__{}", text(rng).chars().take(12).collect::<String>())
    } else {
        tool.to_string()
    };
    let mut req = json!({
        "subtype": "can_use_tool",
        "tool_name": tool_name,
        "input": input,
        "tool_use_id": format!("toolu_{}", rng.below(1000)),
    });
    if rng.chance(30) {
        req["description"] = json!(text(rng));
    }
    if rng.chance(10) {
        req["requires_user_interaction"] = json!(true);
    }
    if rng.chance(10) {
        req["blocked_path"] = json!(text(rng));
    }
    json!({"type": "control_request", "request_id": id, "request": req})
}

/// What a Claude permission card shows of a request (the module docs of `card_input`): its
/// subject (the command for Bash, the path for file tools, the pattern for searches, compact
/// JSON of the whole input for other tools), and every other field of the input as compact
/// JSON (a subject field that is not a string counts as another field; a Bash call's
/// `description` and `timeout` are left out). For Bash, the subject and the fields are all that
/// runs.
fn claude_card(tool: &str, input: &Value) -> (String, Option<String>) {
    let subject = claude_subject(tool, input);
    let shown: &[&str] = match tool {
        "Bash" => &["command"],
        "Read" | "Write" | "Edit" | "MultiEdit" => &["file_path"],
        "NotebookEdit" => &["notebook_path"],
        "Glob" | "Grep" => &["pattern", "path"],
        "WebFetch" => &["url"],
        "WebSearch" => &["query"],
        "Task" | "Agent" => &["description"],
        "ExitPlanMode" => &["plan"],
        "TodoWrite" => &[],
        _ => return (subject, None),
    };
    let fields = match input {
        Value::Null => return (subject, None),
        Value::Object(m) => {
            let mut rest = m.clone();
            rest.retain(|k, v| {
                let quiet = tool == "Bash" && (k == "description" || k == "timeout");
                !quiet && !(v.is_string() && shown.contains(&k.as_str()))
            });
            if rest.is_empty() {
                return (subject, None);
            }
            Value::Object(rest)
        }
        other => other.clone(),
    };
    (subject, Some(fields.to_string()))
}

fn claude_subject(tool: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or_default();
    match tool {
        "Bash" => s("command").into(),
        "Read" | "Write" | "Edit" | "MultiEdit" => s("file_path").into(),
        "NotebookEdit" => s("notebook_path").into(),
        "Glob" | "Grep" => match s("path") {
            "" => s("pattern").into(),
            p => format!("{} in {p}", s("pattern")),
        },
        "WebFetch" => s("url").into(),
        "WebSearch" => s("query").into(),
        "Task" | "Agent" => s("description").into(),
        "ExitPlanMode" => s("plan").into(),
        "TodoWrite" => match input.get("todos").and_then(Value::as_array) {
            Some(t) => format!("{} todos", t.len()),
            None => String::new(),
        },
        _ => match input {
            Value::Null => String::new(),
            Value::Object(m) if m.is_empty() => String::new(),
            other => other.to_string(),
        },
    }
}

fn claude_case(rng: &mut Rng, st: &mut Stats, trail: &mut Trail) -> Result<(), String> {
    let name = *rng.pick(CLAUDE_FIXTURES);
    let mode = *rng.pick(&["bypassPermissions", "bypassPermissions", "default", "plan"]);
    let mut tr = claude_code::Translator::new(claude_code::TranslatorOptions {
        mode: mode.into(),
        ..Default::default()
    });
    let mut turn = 1;
    let mut ids: Vec<String> = vec!["r1".into()];
    for (from_runtime, line) in fixture("claude", name) {
        if !from_runtime {
            if line.contains("\"type\":\"user\"") {
                tr.user_message("go", turn);
                turn += 1;
            }
            continue;
        }
        let lines = if line.contains("\"can_use_tool\"") {
            let v: Value = serde_json::from_str(&line).unwrap();
            ids.push(v["request_id"].as_str().unwrap_or("r").to_string());
            let mut out = Vec::new();
            for _ in 0..1 + rng.below(4) {
                let id = rng.pick(&ids).clone();
                let mut r = claude_request(rng, &id);
                if rng.chance(10) {
                    mutate_value(rng, &mut r);
                }
                out.push(r.to_string());
            }
            out
        } else {
            other_line(rng, st, &line)
        };
        for l in lines {
            claude_feed(rng, st, trail, &mut tr, &l)?;
        }
    }
    Ok(())
}

fn claude_feed(
    rng: &mut Rng,
    st: &mut Stats,
    trail: &mut Trail,
    tr: &mut claude_code::Translator,
    line: &str,
) -> Result<(), String> {
    trail.line = line.to_string();
    st.lines += 1;
    let out = tr.translate(line);
    for e in &out.events {
        let EventBody::AttentionRaised(a) = &e.body else {
            continue;
        };
        if a.reason != AttentionReason::Permission {
            continue;
        }
        trail.item = format!("{a:?}");
        let raw = e.raw.as_deref().ok_or("a permission without its line")?;
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("raw line: {e}"))?;
        let req = &v["request"];
        if req["subtype"] != "can_use_tool" {
            return Err("a permission from something other than can_use_tool".into());
        }
        let tool = req["tool_name"].as_str().unwrap_or_default();
        let input = req.get("input").cloned().unwrap_or(Value::Null);
        let tag = "[deny only: the input is too long to show in full] ";
        let (tagged, summary) = match a.summary.strip_prefix(tag) {
            Some(s) => (true, s),
            None => (false, a.summary.as_str()),
        };
        // The label is the runtime's tool name, marked.
        let line = after_label(summary, &format!("Allow {}", mark_whole(tool)))?;
        let (subject, fields) = claude_card(tool, &input);
        let full = ext_str(a, "claude", FULL_INPUT_KEY);
        let too_long = match &fields {
            None => check_rendering(line, full, &subject, PERMISSION_INPUT_MAX)?,
            Some(fields) => {
                // `<subject> ⟦<other fields>⟧`, each part read back on its own; the full input
                // (when either part is cut) holds both whole, joined the same way.
                let (sline, fline) = split_fields(line)?;
                let fline = fline.ok_or("the input's other fields are not on the card")?;
                let cut = check_line(sline, &subject, PERMISSION_INPUT_MAX)?
                    | check_line(fline, fields, INPUT_EXTRAS_MAX)?;
                let want = marked_bytes(&subject) + marked_bytes(fields) > FULL_INPUT_MAX;
                match (cut, full) {
                    (false, None) => false,
                    (false, Some(_)) => {
                        return Err("a full input next to a summary that shows all".into())
                    }
                    (true, None) => return Err("a cut summary without the full input".into()),
                    (true, Some(full)) => {
                        let (sfull, ffull) = split_fields(full)?;
                        let ffull = ffull.ok_or("the full input lacks the other fields")?;
                        let capped = check_full(sfull, &subject)? | check_full(ffull, fields)?;
                        if capped && !want {
                            return Err("a capped part of an input under the cap".into());
                        }
                        st.count("claude: other fields cut");
                        want
                    }
                }
            }
        };
        if tagged != too_long {
            return Err(format!("deny-only tag {tagged}, input too long {too_long}"));
        }
        check_options(a, too_long)?;
        if fields.is_some() {
            st.count(if tool == "Bash" {
                "claude: Bash with other fields"
            } else {
                "claude: other tool with other fields"
            });
        }
        st.count(match (tool, too_long, full.is_some()) {
            (_, true, _) => "claude: too long, deny only",
            ("Bash", _, true) => "claude: Bash, cut",
            ("Bash", _, false) => "claude: Bash, whole",
            (_, _, true) => "claude: other tool, cut",
            _ => "claude: other tool, whole",
        });
        if tool == "Bash" && input.get("command").is_some_and(|c| !c.is_string()) {
            st.count("claude: Bash without a string command");
        }
        // (d) What an allow approves.
        if rng.chance(60) {
            let reply = tr
                .respond(&a.attn_id, &Answer::allow())
                .ok_or("no reply to an open item")?
                .reply
                .ok_or("an allow to a permission wrote nothing")?;
            let r: Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
            let resp = &r["response"]["response"];
            if r["response"]["request_id"] != v["request_id"] {
                return Err(format!("the allow answers another request: {reply}"));
            }
            if too_long {
                st.denied_allows += 1;
                if resp["behavior"] != "deny" {
                    return Err(format!("an allow to a deny-only item: {reply}"));
                }
            } else {
                st.allows += 1;
                if resp["behavior"] != "allow" || resp["updatedInput"] != input {
                    return Err(format!(
                        "the allow does not approve the request's input: {}",
                        cut(&reply, 400)
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------------------------

const CODEX_FIXTURES: &[&str] = &["approve", "decline", "cancel", "resume"];

const SHELLS: &[&str] = &[
    "/bin/bash -lc",
    "/bin/bash -lc",
    "bash -lc",
    "/bin/zsh -lc",
    "zsh -lc",
    "/bin/sh -c",
    "sh -c",
    "/bin/bash -c",
    "/usr/bin/bash -lc",
];

fn sq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A Codex command: a bare script, or one wrapped in a shell (single or double quotes,
/// nested, or broken).
fn codex_command(rng: &mut Rng, st: &mut Stats) -> String {
    let script = text(rng);
    let shell = *rng.pick(SHELLS);
    match rng.below(12) {
        0 | 1 => script,
        2..=5 => format!("{shell} {}", sq(&script)),
        6 => {
            if script.contains(['"', '\\', '$', '`']) {
                format!("{shell} {}", sq(&script))
            } else {
                format!("{shell} \"{script}\"")
            }
        }
        7 => {
            st.count("codex: nested shells generated");
            let inner = *rng.pick(SHELLS);
            format!("{shell} {}", sq(&format!("{inner} {}", sq(&script))))
        }
        8 => format!("{shell} '{script}'"),
        9 => format!("{shell} {} {}", sq(&script), sq(&text(rng))),
        10 => format!("{shell}  {}", sq(&script)),
        _ => {
            let mut s = format!("{shell} {}", sq(&script));
            let mut c: Vec<char> = s.chars().collect();
            let i = rng.below(c.len());
            match rng.below(3) {
                0 => {
                    c.remove(i);
                }
                1 => c.insert(i, *rng.pick(&['\'', '"', '\\', ' '])),
                _ => c.truncate(i),
            }
            s = c.into_iter().collect();
            s
        }
    }
}

/// Rewrites an approval request: its command (or its actions), its grant root.
fn codex_request(rng: &mut Rng, st: &mut Stats, v: &mut Value) {
    let command_approval = v["method"] == "item/commandExecution/requestApproval";
    let p = &mut v["params"];
    if command_approval {
        match rng.below(20) {
            0 => {
                p.as_object_mut().map(|m| m.remove("command"));
            }
            1 => p["command"] = random_value(rng, 1),
            _ => p["command"] = json!(codex_command(rng, st)),
        }
        if rng.chance(15) {
            p["commandActions"] = json!([{"type": "unknown", "command": text(rng)},
                                          {"type": "read", "command": text(rng)}]);
        }
    }
    if rng.chance(30) {
        p["grantRoot"] = if rng.chance(85) {
            json!(text(rng))
        } else {
            random_value(rng, 1)
        };
    }
    if rng.chance(5) {
        p.as_object_mut().map(|m| m.remove("itemId"));
    }
    if rng.chance(10) {
        mutate_value(rng, v);
    }
}

fn codex_cfg(name: &str) -> SpawnConfig {
    SpawnConfig {
        thread: "T1".into(),
        cwd: PathBuf::from("/tmp/sandbox-codex"),
        mode: if name == "resume" {
            "read-only"
        } else {
            "plan"
        }
        .into(),
        resume_session: (name == "resume").then(|| "01a0eb1f-bbfe-7030-9892-8a0d81f3d695".into()),
        ..Default::default()
    }
}

/// A request of a method the recordings predate (answered as an old server would).
fn codex_optional(line: &str) -> Option<String> {
    let v: Value = serde_json::from_str(line).ok()?;
    let method = v["method"].as_str()?;
    match method {
        "config/read" => Some(json!({"id": v["id"], "result": {"config": {}}}).to_string()),
        "thread/backgroundTerminals/list" | "account/rateLimits/read" => Some(
            json!({"id": v["id"], "error": {"code": -32601, "message": "unknown method"}})
                .to_string(),
        ),
        _ => None,
    }
}

fn codex_case(rng: &mut Rng, st: &mut Stats, trail: &mut Trail) -> Result<(), String> {
    let name = *rng.pick(CODEX_FIXTURES);
    let open = codex::thread_params(&codex_cfg(name)).map_err(|e| e.to_string())?;
    let mut tr = codex::Translator::new(codex::TranslatorOptions::default(), open);
    let mut outbox: VecDeque<String> = VecDeque::from([tr.start()]);
    let mut turn = 1;
    // Items raised so far: the user can answer only those (a request the translator holds
    // until its turn is known has no item yet).
    let mut raised = HashSet::new();
    for (from_runtime, line) in fixture("codex", name) {
        if from_runtime {
            let lines = if line.contains("/requestApproval\"") {
                let v: Value = serde_json::from_str(&line).unwrap();
                let mut out = Vec::new();
                for k in 0..1 + rng.below(3) {
                    let mut r = v.clone();
                    if k > 0 && rng.chance(50) {
                        r["id"] = json!(100 + rng.below(4));
                    }
                    codex_request(rng, st, &mut r);
                    out.push(r.to_string());
                }
                out
            } else {
                other_line(rng, st, &line)
            };
            for l in lines {
                codex_feed(rng, st, trail, &mut tr, &l, &mut outbox, &mut raised)?;
            }
            continue;
        }
        while let Some(answer) = outbox.front().and_then(|l| codex_optional(l)) {
            outbox.pop_front();
            codex_feed(rng, st, trail, &mut tr, &answer, &mut outbox, &mut raised)?;
        }
        if outbox.is_empty() {
            let rec: Value = serde_json::from_str(&line).unwrap();
            match rec["method"].as_str() {
                Some("turn/start") => {
                    outbox.extend(tr.send("go", turn));
                    turn += 1;
                }
                Some("turn/interrupt") => outbox.extend(tr.interrupt()),
                _ => {
                    let open = tr.pending_attention();
                    if let Some(id) = open.iter().find(|id| raised.contains(*id)) {
                        let a = if rng.chance(50) {
                            Answer::allow()
                        } else {
                            Answer::deny()
                        };
                        if let Some(out) = tr.respond(id, &a) {
                            outbox.extend(out.replies);
                        }
                    }
                }
            }
        }
        outbox.pop_front();
    }
    Ok(())
}

fn codex_feed(
    rng: &mut Rng,
    st: &mut Stats,
    trail: &mut Trail,
    tr: &mut codex::Translator,
    line: &str,
    outbox: &mut VecDeque<String>,
    raised: &mut HashSet<String>,
) -> Result<(), String> {
    trail.line = line.to_string();
    st.lines += 1;
    let out = tr.translate(line);
    outbox.extend(out.replies);
    for e in &out.events {
        let EventBody::AttentionRaised(a) = &e.body else {
            continue;
        };
        raised.insert(a.attn_id.clone());
        if a.reason != AttentionReason::Permission {
            continue;
        }
        trail.item = format!("{a:?}");
        let raw = e.raw.as_deref().ok_or("a permission without its line")?;
        trail.line = raw.to_string();
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("raw line: {e}"))?;
        let method = v["method"].as_str().unwrap_or_default();
        let command_approval = method == "item/commandExecution/requestApproval";
        if !command_approval && method != "item/fileChange/requestApproval" {
            st.count("codex: other permissions (not checked)");
            continue;
        }
        let deny_only = codex_check(st, a, &v["params"], command_approval)?;
        // (d) An allow accepts only an item that offers it.
        if rng.chance(60) {
            let out = tr
                .respond(&a.attn_id, &Answer::allow())
                .ok_or("no reply to an open item")?;
            let reply: Value = out
                .replies
                .first()
                .and_then(|r| serde_json::from_str(r).ok())
                .ok_or("an allow to an approval wrote nothing")?;
            if reply["id"] != v["id"] {
                return Err(format!("the allow answers another request: {reply}"));
            }
            let want = if deny_only { "decline" } else { "accept" };
            if reply["result"]["decision"] != want {
                return Err(format!("an allow answered {reply}, not {want}"));
            }
            if deny_only {
                st.denied_allows += 1;
            } else {
                st.allows += 1;
            }
            outbox.extend(out.replies);
        }
    }
    Ok(())
}

/// Checks a Codex approval card against its request's params: Ok(deny-only).
fn codex_check(
    st: &mut Stats,
    a: &AttentionRaised,
    p: &Value,
    command_approval: bool,
) -> Result<bool, String> {
    let mut s = a.summary.as_str();
    let mut why = None;
    if let Some(r) = s.strip_prefix("[deny only: ") {
        let (w, rest) = r.split_once("] ").ok_or("an unclosed deny-only tag")?;
        why = Some(w);
        s = rest;
    }
    let mut want_why = None;
    let root = p
        .get("grantRoot")
        .filter(|r| !r.is_null())
        .map(|r| match r {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
    if let Some(root) = &root {
        const HEAD: &str = "Approving also lets Codex write anywhere under ";
        const TAIL: &str = " for the rest of the session. ";
        let len = HEAD.chars().count()
            + marks_of(root).iter().map(|m| m.2).sum::<usize>()
            + TAIL.chars().count();
        if len > GRANT_SUMMARY_MAX - 100 {
            want_why = Some("the directory it would grant writes under cannot be shown");
            st.count("codex: grant too long, deny only");
        } else {
            let rest = s
                .strip_prefix(HEAD)
                .ok_or("a grant root that the summary does not lead with")?;
            // The root's marks end at one of the occurrences of TAIL (it may hold TAIL too).
            let mut found = None;
            for (i, _) in rest.match_indices(TAIL) {
                if check_rendering(&rest[..i], None, root, usize::MAX).is_ok() {
                    found = Some(i);
                    break;
                }
            }
            let i = found
                .ok_or_else(|| format!("the grant does not read back as {:?}", cut(root, 200)))?;
            s = &rest[i + TAIL.len()..];
            st.count("codex: grant shown");
        }
    }
    let deny_only = if command_approval {
        let room = if root.is_some() {
            400
        } else {
            GRANT_SUMMARY_MAX - 100
        };
        let line = if s == "Allow a shell command?" {
            ""
        } else {
            s.strip_prefix("Allow shell: ")
                .ok_or("a command summary without `Allow shell: `")?
        };
        let full = ext_str(a, "codex", FULL_INPUT_KEY);
        // What runs: `command`; the card may show the script of its shell wrapper. Without
        // a command, the card shows the actions Codex parsed it into.
        let command = p.get("command").and_then(Value::as_str).unwrap_or_default();
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        if command.is_empty() {
            st.count("codex: no command (actions shown)");
            let actions = p["commandActions"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.get("command").and_then(Value::as_str))
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" && ")
                })
                .unwrap_or_default();
            candidates.push((actions, "actions"));
        } else {
            if let Some(script) = shell_script(command).filter(|s| s != command) {
                candidates.push((script, "script"));
            }
            candidates.push((command.to_string(), "verbatim"));
        }
        let mut errors = Vec::new();
        let mut too_long = None;
        for (subject, how) in &candidates {
            match check_rendering(line, full, subject, room) {
                Ok(t) => {
                    too_long = Some(t);
                    st.count(match *how {
                        "script" => "codex: shell script shown",
                        "verbatim" => "codex: command shown verbatim",
                        _ => "codex: actions shown",
                    });
                    if *how == "script" && shell_script(subject).is_some() {
                        st.count("codex: nested shell unwrapped once");
                    }
                    break;
                }
                Err(e) => errors.push(format!("as the {how}: {e}")),
            }
        }
        let too_long = too_long.ok_or_else(|| errors.join("; "))?;
        if too_long {
            want_why = Some("the command is too long to show in full");
            st.count("codex: command too long, deny only");
        } else if full.is_some() {
            st.count("codex: command cut");
        }
        want_why.is_some()
    } else {
        want_why.is_some()
    };
    if why != want_why {
        return Err(format!("deny-only reason {why:?}, expected {want_why:?}"));
    }
    check_options(a, deny_only)?;
    Ok(deny_only)
}

// ---------------------------------------------------------------------------------------------
// ACP (standard and Grok's dialect)
// ---------------------------------------------------------------------------------------------

const GROK_FIXTURES: &[&str] = &[
    "real_permission_allow_v1044",
    "real_permission_reject_v1044",
    "real_stop_pending_v1044",
    "synthetic_deny",
    "synthetic_cancel",
    "synthetic_new_permission_multiturn",
];

const KINDS: &[&str] = &[
    "execute",
    "execute",
    "edit",
    "read",
    "delete",
    "move",
    "fetch",
    "switch_mode",
    "other",
    "",
    "exe\u{202e}cute",
    "read\n: rm -rf ~",
    "\u{200b}",
];

/// Rewrites a `session/request_permission`'s tool call and options.
fn acp_request(rng: &mut Rng, v: &mut Value) {
    let call = &mut v["params"]["toolCall"];
    if !call.is_object() {
        *call = json!({});
    }
    match rng.below(10) {
        0 => {
            call.as_object_mut().map(|m| m.remove("title"));
        }
        1 => call["title"] = random_value(rng, 1),
        2 => call["title"] = json!(""),
        _ => call["title"] = json!(text(rng)),
    }
    call["kind"] = json!(*rng.pick(KINDS));
    match rng.below(4) {
        0 => {
            call.as_object_mut().map(|m| m.remove("rawInput"));
        }
        1 => call["rawInput"] = random_value(rng, 0),
        _ => call["rawInput"] = json!({"command": text(rng)}),
    }
    if rng.chance(40) {
        let all = [
            json!({"optionId": "allow-once", "name": "Yes", "kind": "allow_once"}),
            json!({"optionId": "always", "name": "Always", "kind": "allow_always"}),
            json!({"optionId": "reject-once", "name": "No", "kind": "reject_once"}),
            json!({"optionId": "never", "name": "Never", "kind": "reject_always"}),
            json!({"optionId": 7, "name": "Odd", "kind": "allow_once"}),
            json!({"optionId": "second-allow", "name": "Yes too", "kind": "allow_once"}),
        ];
        let opts: Vec<Value> = (0..rng.below(5)).map(|_| rng.pick(&all).clone()).collect();
        v["params"]["options"] = Value::Array(opts);
    }
    if rng.chance(10) {
        mutate_value(rng, v);
    }
}

/// A standard ACP agent's session: capabilities, a session without modes, then tool calls,
/// permission requests, text and prompt ends.
fn standard_lines(rng: &mut Rng) -> Vec<String> {
    let mut lines = vec![
        r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":true}}}"#
            .to_string(),
        r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S1"}}"#.to_string(),
    ];
    for i in 0..3 + rng.below(8) {
        let call = format!("call_{}", rng.below(4));
        let l = match rng.below(6) {
            0 => json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "S1",
                "update": {"sessionUpdate": "tool_call", "toolCallId": call, "title": text(rng),
                "kind": "execute", "rawInput": {"command": text(rng)}}}}),
            1 => json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "S1",
                "update": {"sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text(rng)}}}}),
            2 => json!({"jsonrpc": "2.0", "method": "$/cancel_request",
                "params": {"requestId": format!("p{}", rng.below(3))}}),
            _ => {
                let id = if rng.chance(50) {
                    json!(format!("p{}", rng.below(3)))
                } else {
                    json!(10 + i)
                };
                let mut v = json!({"jsonrpc": "2.0", "id": id,
                    "method": "session/request_permission", "params": {"sessionId": "S1",
                    "toolCall": {"toolCallId": call},
                    "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}});
                acp_request(rng, &mut v);
                v
            }
        };
        lines.push(l.to_string());
    }
    lines.push(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#.to_string());
    lines
}

fn acp_case(rng: &mut Rng, st: &mut Stats, trail: &mut Trail) -> Result<(), String> {
    if rng.chance(35) {
        let mut tr = acp::Translator::with_dialect(
            acp::Options {
                cwd: "/work".into(),
                mode: "manual".into(),
                ..Default::default()
            },
            acp::Standard::new(vec!["agent".into()], None, None),
        );
        tr.start();
        tr.send("go", 1);
        for l in standard_lines(rng) {
            let l = if l.contains("request_permission") {
                vec![l]
            } else {
                other_line(rng, st, &l)
            };
            for l in l {
                trail.line = l.clone();
                st.lines += 1;
                let out = tr.translate(&l);
                acp_check(rng, st, trail, &out.events, "acp", |id, a| {
                    tr.respond(id, a)
                })?;
            }
        }
        return Ok(());
    }
    let name = *rng.pick(GROK_FIXTURES);
    let mut tr = grok_build::Translator::new(grok_build::TranslatorOptions {
        cwd: "/work/repo".into(),
        pid: Some(4242),
        exe: Some("/opt/grok".into()),
        thinking: true,
        model: name
            .starts_with("real_")
            .then(|| "grok-4.7-build-fast".into()),
        ..Default::default()
    });
    let mut writes: VecDeque<String> = tr.start().writes.into();
    let mut turn = 1;
    let mut raised = HashSet::new();
    for (from_runtime, line) in fixture("grok", name) {
        if from_runtime {
            let lines = if line.contains("\"session/request_permission\"") {
                let v: Value = serde_json::from_str(&line).unwrap();
                let mut out = Vec::new();
                for k in 0..1 + rng.below(3) {
                    let mut r = v.clone();
                    if k > 0 && rng.chance(50) {
                        r["id"] = json!(50 + rng.below(3));
                    }
                    acp_request(rng, &mut r);
                    out.push(r.to_string());
                }
                out
            } else {
                other_line(rng, st, &line)
            };
            for l in lines {
                trail.line = l.clone();
                st.lines += 1;
                let out = tr.translate(&l);
                writes.extend(out.writes.clone());
                raised.extend(out.events.iter().filter_map(|e| match &e.body {
                    EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
                    _ => None,
                }));
                acp_check(rng, st, trail, &out.events, "grok", |id, a| {
                    tr.respond(id, a)
                })?;
            }
            continue;
        }
        if writes.is_empty() {
            let rec: Value = serde_json::from_str(&line).unwrap();
            let out = match rec["method"].as_str() {
                Some("session/prompt") => {
                    turn += 1;
                    Some(tr.send("go", turn - 1))
                }
                Some("session/cancel") => Some(tr.stop()),
                _ => tr
                    .pending_attention()
                    .into_iter()
                    .find(|id| raised.contains(id))
                    .and_then(|id| {
                        let a = if rng.chance(50) {
                            Answer::allow()
                        } else {
                            Answer::deny()
                        };
                        tr.respond(&id, &a)
                    }),
            };
            if let Some(out) = out {
                writes.extend(out.writes);
            }
        }
        writes.pop_front();
    }
    Ok(())
}

/// Checks the permission items of one translation; `respond` answers one.
fn acp_check(
    rng: &mut Rng,
    st: &mut Stats,
    trail: &mut Trail,
    events: &[acp::Emitted],
    section: &str,
    mut respond: impl FnMut(&str, &Answer) -> Option<acp::Translated>,
) -> Result<(), String> {
    for e in events {
        let EventBody::AttentionRaised(a) = &e.body else {
            continue;
        };
        if a.reason != AttentionReason::Permission {
            continue;
        }
        trail.item = format!("{a:?}");
        let raw = e.raw.as_deref().ok_or("a permission without its line")?;
        trail.line = raw.to_string();
        let v: Value = serde_json::from_str(raw).map_err(|e| format!("raw line: {e}"))?;
        if v["method"] != "session/request_permission" {
            st.count("acp: other permissions (not checked)");
            continue;
        }
        let call = &v["params"]["toolCall"];
        let title = call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = call.get("kind").and_then(Value::as_str).unwrap_or_default();
        // The label is the agent's kind, marked.
        let label = if kind.is_empty() {
            "tool call".to_string()
        } else {
            mark_whole(kind)
        };
        if label != kind && !kind.is_empty() {
            st.count("acp: kind marked");
        }
        let tag = "[deny only: the request is too long to show in full] ";
        let (tagged, summary) = match a.summary.strip_prefix(tag) {
            Some(s) => (true, s),
            None => (false, a.summary.as_str()),
        };
        let line = after_label(summary, &format!("Allow {label}"))?;
        // What the user approves: the tool call its title names, or (untitled) its input.
        let subject = if !title.is_empty() {
            st.count("acp: title shown");
            title.to_string()
        } else {
            match call.get("rawInput") {
                Some(i) if !i.is_null() => {
                    st.count("acp: untitled, input shown");
                    i.to_string()
                }
                _ => {
                    st.count("acp: untitled, no input");
                    String::new()
                }
            }
        };
        let full = ext_str(a, section, FULL_INPUT_KEY);
        let too_long = check_rendering(line, full, &subject, ACP_SUMMARY_MAX)?;
        if too_long {
            st.count("acp: too long, deny only");
        } else if full.is_some() {
            st.count("acp: cut");
        }
        if tagged != too_long {
            return Err(format!("deny-only tag {tagged}, input too long {too_long}"));
        }
        let opts = v["params"]["options"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let first = |kind: &str| {
            opts.iter()
                .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
                .and_then(|o| o.get("optionId").and_then(Value::as_str))
                .map(str::to_string)
        };
        let allow_once = first("allow_once");
        let deny_only = too_long || allow_once.is_none();
        check_options(a, deny_only)?;
        // (d) What an allow selects.
        if rng.chance(60) {
            let out = respond(&a.attn_id, &Answer::allow()).ok_or("no reply to an open item")?;
            let reply: Value = out
                .writes
                .first()
                .and_then(|r| serde_json::from_str(r).ok())
                .ok_or("an allow to a permission wrote nothing")?;
            if reply["id"] != v["id"] {
                return Err(format!("the allow answers another request: {reply}"));
            }
            let outcome = &reply["result"]["outcome"];
            let picked = outcome.get("optionId").and_then(Value::as_str);
            if deny_only {
                st.denied_allows += 1;
                let allowing = opts.iter().any(|o| {
                    o.get("optionId").and_then(Value::as_str) == picked
                        && o.get("kind")
                            .and_then(Value::as_str)
                            .is_some_and(|k| k.starts_with("allow"))
                });
                if picked.is_some() && (allowing || picked != first("reject_once").as_deref()) {
                    return Err(format!("an allow to a deny-only item selected {outcome}"));
                }
            } else {
                st.allows += 1;
                if picked != allow_once.as_deref() {
                    return Err(format!(
                        "an allow selected {outcome}, not the first allow_once {allow_once:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The reference's own checks, and the runs
// ---------------------------------------------------------------------------------------------

/// The reference reads the rule's own examples as the rule says.
#[test]
fn the_reference_reads_the_rule() {
    let ok = |s: &str| -> String { decode(s).unwrap().iter().map(|x| x.0).collect() };
    assert_eq!(ok("ls -la"), "ls -la");
    assert_eq!(ok("a⏎b␍c⇥d"), "a\nb\rc\td");
    assert_eq!(ok("⟨U+0020⟩x⟨U+202E⟩"), " x\u{202e}");
    assert_eq!(ok("жé٣"), "жé٣");
    assert_eq!(ok("⟨U+1BCA0⟩"), "\u{1bca0}");
    for bad in [
        "a\u{202e}b",
        "a\nb",
        "\u{a0}",
        "e\u{301}",
        "⟨U+41⟩",
        "⟨U+0041",
        "⟨u+0041⟩",
        "…",
        "⟩",
    ] {
        assert!(decode(bad).is_err(), "{bad:?}");
    }
    // An end space is escaped, an inner one is not.
    assert!(check_rendering("⟨U+0020⟩a b⟨U+0020⟩", None, " a b ", 100).is_ok());
    assert!(check_rendering(" a b ", None, " a b ", 100).is_err());
    assert!(check_rendering("⟨U+0041⟩", None, "A", 100).is_err());
    // The encoder and the card's two parts.
    assert_eq!(mark_whole(" a\u{202e}\n "), "⟨U+0020⟩a⟨U+202E⟩⏎⟨U+0020⟩");
    assert_eq!(split_fields("ls ⟦{}⟧"), Ok(("ls", Some("{}"))));
    assert_eq!(split_fields("⟦{}⟧"), Ok(("", Some("{}"))));
    assert_eq!(split_fields("ls"), Ok(("ls", None)));
    assert!(split_fields("ls⟦{}⟧").is_err());
    // Shell words.
    assert_eq!(
        shell_script("/bin/bash -lc 'echo '\\''hi'\\'''"),
        Some("echo 'hi'".into())
    );
    assert_eq!(shell_script("sh -c \"a b\""), Some("a b".into()));
    assert_eq!(shell_script("bash -lc 'a' 'b'"), None);
    assert_eq!(shell_script("bash -lc 'a'; rm x"), None);
    assert_eq!(shell_script("bash -lc \"$x\""), None);
    assert_eq!(shell_script("bash -lc 'a"), None);
}

#[test]
fn claude_permission_cards() {
    let st = run_fuzz("claude", claude_case, DEFAULT_CASES);
    assert_reached(
        &st,
        DEFAULT_CASES,
        &[
            "claude: Bash, whole",
            "claude: Bash, cut",
            "claude: too long, deny only",
            "claude: other tool, whole",
            "claude: Bash with other fields",
            "claude: other fields cut",
        ],
    );
}

#[test]
fn codex_approval_cards() {
    let st = run_fuzz("codex", codex_case, DEFAULT_CASES);
    assert_reached(
        &st,
        DEFAULT_CASES,
        &[
            "codex: shell script shown",
            "codex: command shown verbatim",
            "codex: command cut",
            "codex: command too long, deny only",
            "codex: grant shown",
            "codex: grant too long, deny only",
        ],
    );
}

#[test]
fn acp_permission_cards() {
    let st = run_fuzz("acp", acp_case, DEFAULT_CASES);
    assert_reached(
        &st,
        DEFAULT_CASES,
        &[
            "acp: title shown",
            "acp: untitled, input shown",
            "acp: cut",
            "acp: too long, deny only",
            "acp: kind marked",
        ],
    );
}

#[test]
#[ignore = "long run: cargo test -p twr-adapters --release --test translate_fuzz -- --ignored"]
fn claude_permission_cards_long() {
    run_fuzz("claude", claude_case, LONG_CASES);
}

#[test]
#[ignore = "long run: cargo test -p twr-adapters --release --test translate_fuzz -- --ignored"]
fn codex_approval_cards_long() {
    run_fuzz("codex", codex_case, LONG_CASES);
}

#[test]
#[ignore = "long run: cargo test -p twr-adapters --release --test translate_fuzz -- --ignored"]
fn acp_permission_cards_long() {
    run_fuzz("acp", acp_case, LONG_CASES);
}

//! Reading the orchestrator's messages aloud (`docs/read-aloud.md`): which turns may speak,
//! how a message's Markdown becomes the sentences a voice reads, and the wire types of the
//! hub's `speech.*` methods, which the hub, the speaker (`twr speak`, the TUI's own) and the
//! `twr speech feed` proxy share.
//!
//! - **Faithful reading.** [`utterances`] keeps a message's own words in order and only
//!   drops or replaces markup: emphasis, heading marks and quotes go; a link reads as its
//!   text, a bare URL as `link to <host>`; a code block is announced ([`CodeMode`]); a long
//!   path reads as its last two parts ([`PathMode`]); control, bidirectional and zero-width
//!   characters, emoji and box drawing are dropped. No model rewrites anything.
//! - **Chunks.** Utterances are at most [`CHUNK_MAX`] characters, cut at a sentence end, then
//!   at a comma or semicolon, then at a space: stop, pause and skip act within one chunk on
//!   any engine, and an engine that cannot pause restarts the chunk.
//! - **Which turns speak** ([`speakable_turn`]): conversation turns, never maintenance (a
//!   handoff, a compaction).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::{TurnFrom, TurnStarted};

/// The longest utterance, in characters.
pub const CHUNK_MAX: usize = 300;

/// The most text of one message a speaker gets, in bytes (cut on a character boundary, the
/// message then `truncated`).
pub const TEXT_MAX: usize = 64 * 1024;

/// The most messages `speech.request` reads.
pub const REQUEST_MAX: u32 = 50;

/// The most watermarks a speaker sends.
pub const WATERMARKS_MAX: usize = 512;

/// The longest catch-up a speaker may ask for, in seconds (a day).
pub const CATCHUP_MAX: u64 = 86_400;

/// Words per minute an estimate assumes when the speaker's rate is the engine's default.
pub const DEFAULT_WPM: u32 = 175;

/// Inline code up to this many characters reads as its text; longer, as `code`.
const INLINE_CODE_MAX: usize = 40;

/// The keys of `turn.started` `ext.twr` that mark a turn as maintenance, never read aloud:
/// a handoff to a new orchestrator (and the old one's summary turn before it), the handoff
/// prompt, a context compaction (the user's, an offer's or the hub's idle one alike). A new
/// kind of maintenance turn must be added here.
///
/// Not `runtime_handoff`: a thread moved to another runtime or account carries it on the turn
/// of the message that runs next (the handoff only goes before that message's prompt), so
/// that turn is a message like any other and its reply is read.
pub const SILENT_MARKERS: &[&str] = &["handoff", "handoff_prompt", "compact"];

/// Whether a turn's texts may be read aloud: a turn the user, the orchestrator (a notice) or
/// the runtime (a follow-up) started, unless it carries one of [`SILENT_MARKERS`] (set to
/// anything but `null` or `false`).
pub fn speakable_turn(t: &TurnStarted) -> bool {
    let allowed = matches!(
        t.from,
        TurnFrom::User | TurnFrom::Orchestrator | TurnFrom::Runtime
    );
    let twr = t.ext.as_ref().and_then(|m| m.get("twr"));
    let marked = SILENT_MARKERS.iter().any(|k| {
        twr.and_then(|v| v.get(*k))
            .is_some_and(|v| !v.is_null() && v != &Value::Bool(false))
    });
    // A part of `ext` still in the blob store (not hydrated) could hold a marker: silent.
    let unread = |m: Option<&serde_json::Map<String, Value>>| {
        m.is_some_and(|m| {
            m.keys()
                .any(|k| k.ends_with("_ref") || k == crate::blob::CAPPED_KEY)
        })
    };
    let hidden = unread(t.ext.as_ref()) || unread(twr.and_then(Value::as_object));
    allowed && !marked && !hidden
}

/// Whether a turn is one the user started (`[speech] turn_from = "mine"`).
pub fn turn_is_users(t: &TurnStarted) -> bool {
    t.from == TurnFrom::User
}

/// `[speech] code`: what a fenced code block reads as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeMode {
    /// `code block, 12 lines, rust`.
    #[default]
    Announce,
    /// Its lines, one sentence each.
    Read,
    /// Nothing.
    Skip,
}

/// `[speech] paths`: how a file path reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PathMode {
    /// A path of three parts or more reads as its last two: `twr/app.rs, line 120`.
    #[default]
    Short,
    /// As written.
    Full,
}

/// How [`utterances`] reads a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeakOpts {
    /// Code blocks.
    pub code: CodeMode,
    /// Paths.
    pub paths: PathMode,
}

/// `text` with every character a voice should never get removed: C0 controls (tabs and line
/// breaks become spaces), DEL, C1 controls, bidirectional controls, zero-width characters and
/// variation selectors.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter_map(|c| match c {
            '\t' | '\n' | '\r' => Some(' '),
            c if invisible(c) => None,
            c => Some(c),
        })
        .collect()
}

/// A character never spoken and never shown to an engine.
fn invisible(c: char) -> bool {
    let u = c as u32;
    c.is_control()
        || (0x200B..=0x200F).contains(&u)
        || (0x202A..=0x202E).contains(&u)
        || (0x2060..=0x206F).contains(&u)
        || (0xFE00..=0xFE0F).contains(&u)
        || u == 0xFEFF
        || u == 0x061C
        || (0xE0000..=0xE007F).contains(&u)
}

/// A symbol dropped from speech: box drawing, block elements, geometric shapes, dingbats,
/// emoji and pictographs.
fn pictograph(c: char) -> bool {
    let u = c as u32;
    (0x2500..=0x25FF).contains(&u)
        || (0x2600..=0x27BF).contains(&u)
        || (0x2B00..=0x2BFF).contains(&u)
        || (0x1F000..=0x1FAFF).contains(&u)
        || u == 0x20E3
}

/// The sentences of a message, as a voice should read them, in chunks of at most
/// [`CHUNK_MAX`] characters (see the module documentation). Empty for a message with
/// nothing to say.
pub fn utterances(markdown: &str, opts: &SpeakOpts) -> Vec<String> {
    chunk(&sentences(markdown, opts))
}

/// The sentences of a message, before chunking: one per paragraph, heading, list item, table
/// row or code announcement.
pub fn sentences(markdown: &str, opts: &SpeakOpts) -> Vec<String> {
    use pulldown_cmark::{CodeBlockKind, Event as Md, Options, Parser, Tag, TagEnd};
    let markdown = &unwrap_sections(markdown);

    let mut r = Reader {
        opts: *opts,
        out: Vec::new(),
        buf: String::new(),
    };
    // Lists' next numbers (None: a bullet list).
    let mut lists: Vec<Option<u64>> = Vec::new();
    // Inside a code block: its language and lines.
    let mut code: Option<(String, String)> = None;
    // Inside a link: its target and the text so far.
    let mut link: Option<(String, String)> = None;
    // Inside a table: its rows, and the cells of the row so far.
    let mut table: Option<(usize, Vec<String>)> = None;
    let mut cell = String::new();
    let mut in_cell = false;
    let mut image_depth = 0usize;

    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for ev in Parser::new_ext(markdown, options) {
        match ev {
            Md::Start(Tag::CodeBlock(kind)) => {
                r.flush();
                let lang = match kind {
                    CodeBlockKind::Fenced(l) => {
                        l.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                code = Some((lang, String::new()));
            }
            Md::End(TagEnd::CodeBlock) => {
                if let Some((lang, body)) = code.take() {
                    r.code_block(&lang, &body);
                }
            }
            Md::Text(t) if code.is_some() => {
                if let Some((_, body)) = code.as_mut() {
                    body.push_str(&t);
                }
            }
            Md::Start(Tag::Table(_)) => {
                r.flush();
                table = Some((0, Vec::new()));
            }
            Md::End(TagEnd::Table) => {
                if let Some((rows, lines)) = table.take() {
                    let noun = if rows == 1 { "row" } else { "rows" };
                    r.out.push(format!("Table, {rows} {noun}."));
                    for line in lines {
                        r.push_sentence(&line);
                    }
                }
            }
            Md::Start(Tag::TableHead) | Md::Start(Tag::TableRow) => {
                if let Some((_, lines)) = table.as_mut() {
                    lines.push(String::new());
                }
            }
            Md::End(TagEnd::TableHead) => {}
            Md::End(TagEnd::TableRow) => {
                if let Some((rows, _)) = table.as_mut() {
                    *rows += 1;
                }
            }
            Md::Start(Tag::TableCell) => {
                in_cell = true;
                cell.clear();
            }
            Md::End(TagEnd::TableCell) => {
                in_cell = false;
                let text = squash(&cell);
                if let Some((_, lines)) = table.as_mut() {
                    if let Some(line) = lines.last_mut() {
                        if !text.is_empty() {
                            if !line.is_empty() {
                                line.push_str(", ");
                            }
                            line.push_str(&text);
                        }
                    }
                }
            }
            Md::Start(Tag::Link { dest_url, .. }) => {
                link = Some((dest_url.to_string(), String::new()));
            }
            Md::End(TagEnd::Link) => {
                if let Some((url, text)) = link.take() {
                    let text = squash(&text);
                    // Only a label that is no more than a URL (`<https://...>`, or the URL
                    // written out) reads as the destination's host; any other is read as words,
                    // a URL in it as its own host.
                    let bare = !text.contains(' ') && text.contains("://");
                    let said = if text.is_empty() || text == url || bare {
                        url_words(&url)
                    } else {
                        squash(&r.words(&text))
                    };
                    r.inline(&said, &mut cell, in_cell);
                }
            }
            Md::Start(Tag::Image { .. }) => {
                image_depth += 1;
                // "image", then its alt text (the image's own `Text` events), in a link's label
                // when it is inside one.
                match link.as_mut() {
                    Some((_, text)) => text.push_str(" image "),
                    None => r.inline(" image ", &mut cell, in_cell),
                }
            }
            Md::End(TagEnd::Image) => {
                image_depth = image_depth.saturating_sub(1);
            }
            Md::Text(t) => {
                let words = r.words(&t);
                if let Some((_, text)) = link.as_mut() {
                    // Cleaned now; the label is read as words when the link ends.
                    text.push_str(&clean(&t));
                } else {
                    r.inline(&words, &mut cell, in_cell);
                }
            }
            Md::Code(c) => {
                let c = clean(&c);
                let said =
                    if c.chars().count() <= INLINE_CODE_MAX && c.split_whitespace().count() <= 4 {
                        r.words(&c)
                    } else {
                        "code".to_string()
                    };
                if let Some((_, text)) = link.as_mut() {
                    text.push_str(&said);
                } else {
                    r.inline(&said, &mut cell, in_cell);
                }
            }
            Md::SoftBreak | Md::HardBreak => {
                if let Some((_, text)) = link.as_mut() {
                    text.push(' ');
                } else {
                    r.inline(" ", &mut cell, in_cell);
                }
            }
            Md::Start(Tag::List(start)) => {
                r.flush();
                lists.push(start);
            }
            Md::End(TagEnd::List(_)) => {
                r.flush();
                lists.pop();
            }
            Md::Start(Tag::Item) => {
                r.flush();
                if let Some(Some(n)) = lists.last_mut() {
                    r.buf.push_str(&format!("{n}. "));
                    *n += 1;
                }
            }
            Md::TaskListMarker(done) => {
                r.buf.push_str(if done { "Done: " } else { "To do: " });
            }
            Md::End(TagEnd::Item)
            | Md::End(TagEnd::Paragraph)
            | Md::End(TagEnd::Heading(_))
            | Md::End(TagEnd::BlockQuote(_)) => r.flush(),
            Md::Start(Tag::Paragraph) | Md::Start(Tag::Heading { .. }) => {
                if !in_cell {
                    r.flush();
                }
            }
            Md::Rule => r.flush(),
            // An HTML block holds text too (Markdown takes the lines after a tag, up to a
            // blank line, as part of it): read without its tags.
            Md::Start(Tag::HtmlBlock) => r.flush(),
            Md::End(TagEnd::HtmlBlock) => r.flush(),
            Md::Html(h) => {
                let words = r.words(&strip_tags(&h));
                r.inline(&words, &mut cell, in_cell);
                r.inline(" ", &mut cell, in_cell);
            }
            // Inline tags, footnotes, math and metadata: not read.
            _ => {}
        }
    }
    r.flush();
    let _ = image_depth;
    r.out
}

/// The orchestrator's thread sections, as the TUI draws them in a box each: a line that is
/// only `<thread name="api">` opens one, a line that is only `</thread>` closes it. Markdown
/// would take those lines for HTML and drop what follows up to a blank line; here they become
/// the thread's name as a paragraph of its own (`api:`) and a blank line, and the section's
/// text is read as any other.
fn unwrap_sections(markdown: &str) -> String {
    if !markdown.contains("<thread") && !markdown.contains("</thread>") {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    for line in markdown.split('\n') {
        let t = line.trim();
        let name = t
            .strip_prefix("<thread name=\"")
            .and_then(|r| r.strip_suffix("\">"))
            .map(str::trim)
            .filter(|n| !n.is_empty() && !n.contains('"'));
        if let Some(name) = name {
            out.push('\n');
            out.push_str(&name.replace(['*', '_', '`', '[', ']', '<', '>', '#'], " "));
            out.push_str(":\n\n");
        } else if t == "</thread>" {
            out.push_str("\n\n");
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// `html` without its tags (`<...>`) and comments; what is between them stays.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(i) = rest.find('<') {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        // A tag starts `<` and a letter, `/`, `!` or `?`; any other `<` is text (`a < 5`).
        let tag = after[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        if !tag {
            out.push('<');
            rest = &after[1..];
            continue;
        }
        let end = if after.starts_with("<!--") {
            after.find("-->").map(|e| e + 3)
        } else {
            after.find('>').map(|e| e + 1)
        };
        match end {
            Some(e) => {
                out.push(' ');
                rest = &after[e..];
            }
            None => {
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// What [`sentences`] builds.
struct Reader {
    opts: SpeakOpts,
    out: Vec<String>,
    buf: String,
}

impl Reader {
    /// Appends inline words to the sentence being built (or the table cell).
    fn inline(&mut self, words: &str, cell: &mut String, in_cell: bool) {
        if in_cell {
            cell.push_str(words);
        } else {
            self.buf.push_str(words);
        }
    }

    /// Ends the sentence being built.
    fn flush(&mut self) {
        let s = std::mem::take(&mut self.buf);
        self.push_sentence(&s);
    }

    /// Adds `s` as a sentence, its spaces collapsed and a full stop added when it ends in no
    /// punctuation. Nothing for one without a letter or digit.
    fn push_sentence(&mut self, s: &str) {
        let s = squash(s);
        if !s.chars().any(char::is_alphanumeric) {
            return;
        }
        let ends = s
            .chars()
            .last()
            .is_some_and(|c| matches!(c, '.' | '!' | '?' | ':' | ';' | '…'));
        self.out.push(if ends { s } else { format!("{s}.") });
    }

    /// A code block as [`CodeMode`] says.
    fn code_block(&mut self, lang: &str, body: &str) {
        let lines = body.lines().filter(|l| !l.trim().is_empty()).count();
        match self.opts.code {
            CodeMode::Skip => {}
            CodeMode::Read => {
                for line in body.lines() {
                    let w = self.words(line);
                    self.push_sentence(&w);
                }
            }
            CodeMode::Announce => {
                let noun = if lines == 1 { "line" } else { "lines" };
                let lang = squash(&clean(lang));
                let lang: String = lang
                    .chars()
                    .filter(|c| c.is_alphanumeric() || "+#-_".contains(*c))
                    .take(20)
                    .collect();
                if lang.is_empty() {
                    self.out.push(format!("Code block, {lines} {noun}."));
                } else {
                    self.out
                        .push(format!("Code block, {lines} {noun}, {lang}."));
                }
            }
        }
    }

    /// Plain text as words to say: control characters out, URLs and paths read short,
    /// arrows as "to", pictographs dropped.
    fn words(&self, text: &str) -> String {
        let text = clean(text);
        let mut out = String::with_capacity(text.len());
        let mut first = true;
        // Keep the text's own leading and trailing space so adjacent inline pieces join.
        if text.starts_with(' ') {
            out.push(' ');
        }
        for token in text.split(' ').filter(|t| !t.is_empty()) {
            if !first {
                out.push(' ');
            }
            first = false;
            out.push_str(&self.token(token));
        }
        if text.ends_with(' ') && !text.trim().is_empty() {
            out.push(' ');
        }
        out
    }

    /// One whitespace-free token as said.
    fn token(&self, token: &str) -> String {
        if matches!(token, "→" | "->" | "⇒" | "=>" | "⟶") {
            return "to".to_string();
        }
        let lead = token
            .find(|c: char| !matches!(c, '(' | '[' | '"' | '\'' | '<' | '«' | '“' | '‘'))
            .unwrap_or(token.len());
        let (pre, rest) = token.split_at(lead);
        let core_end = rest
            .trim_end_matches(|c: char| {
                matches!(
                    c,
                    '.' | ','
                        | ';'
                        | ':'
                        | '!'
                        | '?'
                        | ')'
                        | ']'
                        | '"'
                        | '\''
                        | '>'
                        | '»'
                        | '”'
                        | '’'
                )
            })
            .len();
        let (core, post) = rest.split_at(core_end);
        let said = if core.starts_with("http://") || core.starts_with("https://") {
            url_words(core)
        } else if self.opts.paths == PathMode::Short && is_path(core) {
            short_path(core)
        } else {
            core.replace(['→', '⇒', '⟶'], " to ")
        };
        let said: String = said.chars().filter(|c| !pictograph(*c)).collect();
        format!("{pre}{said}{post}")
            .chars()
            .filter(|c| !pictograph(*c))
            .collect()
    }
}

/// A URL as `link to <host>`.
fn url_words(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    let host = host.strip_prefix("www.").unwrap_or(host);
    let host: String = clean(host).chars().filter(|c| !pictograph(*c)).collect();
    if host.is_empty() {
        "a link".to_string()
    } else {
        format!("link to {host}")
    }
}

/// Whether a token looks like a file path of three parts or more.
fn is_path(token: &str) -> bool {
    if token.contains("://") || token.starts_with("//") {
        return false;
    }
    let path = strip_line(token).0;
    let parts = path.split('/').filter(|p| !p.is_empty()).count();
    parts >= 3 && path.contains('/') && !path.contains(char::is_whitespace)
}

/// A path's `:line[:col]` suffix split off: (`path`, `Some(line)`).
fn strip_line(token: &str) -> (&str, Option<&str>) {
    let name = token.rfind('/').map(|i| i + 1).unwrap_or(0);
    let Some(colon) = token[name..].find(':') else {
        return (token, None);
    };
    let (path, rest) = token.split_at(name + colon);
    let nums: Vec<&str> = rest[1..].split(':').collect();
    let digits = |n: &&str| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit());
    if nums.len() <= 2 && nums.iter().all(digits) {
        (path, Some(nums[0]))
    } else {
        (token, None)
    }
}

/// A path as its last two parts, and its line.
fn short_path(token: &str) -> String {
    let (path, line) = strip_line(token);
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let tail = parts[parts.len().saturating_sub(2)..].join("/");
    match line {
        Some(n) => format!("{tail}, line {n}"),
        None => tail,
    }
}

/// `s` with runs of whitespace as one space, trimmed.
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sentences joined into utterances of at most [`CHUNK_MAX`] characters: whole sentences
/// while they fit, a longer one cut at a sentence end, a comma or semicolon, a space, or
/// (a word longer than a chunk) anywhere.
pub fn chunk(sentences: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for s in sentences {
        for piece in split_long(s) {
            let len = cur.chars().count();
            if len == 0 {
                cur = piece;
            } else if len + 1 + piece.chars().count() <= CHUNK_MAX {
                cur.push(' ');
                cur.push_str(&piece);
            } else {
                out.push(std::mem::take(&mut cur));
                cur = piece;
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// `s` in pieces of at most [`CHUNK_MAX`] characters.
fn split_long(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while rest.chars().count() > CHUNK_MAX {
        let window_end = rest
            .char_indices()
            .nth(CHUNK_MAX)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..window_end];
        let cut = last_cut(window, &[". ", "! ", "? ", "… "])
            .or_else(|| last_cut(window, &[", ", "; ", ": "]))
            .or_else(|| window.rfind(' ').filter(|&i| i > 0).map(|i| i + 1))
            .unwrap_or(window_end);
        let (head, tail) = rest.split_at(cut);
        let head = head.trim();
        if !head.is_empty() {
            out.push(head.to_string());
        }
        rest = tail.trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// The byte index just after the last of `marks` in `window` (keeping the mark's punctuation
/// with the head), if any is past its first quarter.
fn last_cut(window: &str, marks: &[&str]) -> Option<usize> {
    marks
        .iter()
        .filter_map(|m| window.rfind(m).map(|i| i + m.len() - 1))
        .filter(|&i| i >= window.len() / 4)
        .max()
}

/// About how long `chunks` take to say at `wpm` words per minute (0: [`DEFAULT_WPM`]), in
/// seconds.
pub fn estimate_secs(chunks: &[String], wpm: u32) -> u64 {
    let wpm = if wpm == 0 { DEFAULT_WPM } else { wpm } as u64;
    let words: u64 = chunks
        .iter()
        .map(|c| c.split_whitespace().count() as u64)
        .sum();
    words * 60 / wpm.max(1)
}

/// `text` cut to at most `max` bytes on a character boundary; whether it was cut.
pub fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

// ---- Wire types of `speech.*` ----

/// `[speech] read`: which of the orchestrator's texts a speaker gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReadKind {
    /// The last text of a turn.
    Replies,
    /// A text before the turn's last one (before a tool call).
    Commentary,
}

/// `[speech] turn_from`: whose turns are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnsRead {
    /// Every conversation turn: yours, the orchestrator's notices, runtime follow-ups.
    #[default]
    All,
    /// Turns you started.
    Mine,
}

/// The default of `read`: both kinds.
pub fn default_read() -> Vec<ReadKind> {
    vec![ReadKind::Replies, ReadKind::Commentary]
}

/// `speech.attach` params: a speaker attaches to the hub.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechAttachParams {
    /// The speaker's device: one per machine (1-64 of `[A-Za-z0-9_-]`). A second speaker of
    /// a device attached meanwhile is `conflict`.
    pub device: String,
    /// What the speaker is called (`/read status`, `--on`): at most 64 printable characters.
    pub name: String,
    /// Its engine (`say`, `spd`, `avspeech`, `command`, `text`).
    pub engine: String,
    /// Its voice, if one is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Whether live reading is on on this speaker for this hub.
    #[serde(default)]
    pub live: bool,
    /// Which texts it reads.
    #[serde(default = "default_read")]
    pub read: Vec<ReadKind>,
    /// Whose turns it reads.
    #[serde(default)]
    pub turn_from: TurnsRead,
    /// The last seq it read (spoken or skipped) per thread id. A thread without one starts at
    /// its head: nothing older is sent.
    #[serde(default)]
    pub watermarks: BTreeMap<String, u64>,
    /// Messages older than this many seconds after a watermark are counted (`missed`), not
    /// sent.
    #[serde(default = "default_catchup")]
    pub catchup_secs: u64,
}

/// The default catch-up span, in seconds.
pub fn default_catchup() -> u64 {
    120
}

impl SpeechAttachParams {
    /// Why these params are refused, if they are.
    pub fn error(&self) -> Option<String> {
        if self.device.is_empty()
            || self.device.len() > 64
            || !self
                .device
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Some("device: 1-64 of letters, digits, `_` and `-`".into());
        }
        if let Some(e) = name_error("name", &self.name, 64) {
            return Some(e);
        }
        if let Some(e) = name_error("engine", &self.engine, 32) {
            return Some(e);
        }
        if let Some(v) = &self.voice {
            if let Some(e) = name_error("voice", v, 128) {
                return Some(e);
            }
        }
        if self.watermarks.len() > WATERMARKS_MAX {
            return Some(format!("watermarks: at most {WATERMARKS_MAX}"));
        }
        if self.catchup_secs > CATCHUP_MAX {
            return Some(format!("catchup_secs: at most {CATCHUP_MAX}"));
        }
        if self.read.is_empty() {
            return Some("read: at least one of replies, commentary".into());
        }
        None
    }
}

/// Why `value` is no good as `what` (1 to `max` characters, none of them a control or
/// invisible one), if it isn't.
pub fn name_error(what: &str, value: &str, max: usize) -> Option<String> {
    if value.trim().is_empty() || value.chars().count() > max || value.chars().any(invisible) {
        Some(format!("{what}: 1-{max} printable characters"))
    } else {
        None
    }
}

/// The ack of `speech.attach`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeechAttachAck {
    /// The orchestrator thread the feed follows, if there is one.
    pub orchestrator: Option<String>,
    /// Its head when the feed began.
    pub head: u64,
}

/// What a message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MsgKind {
    /// The turn's last text.
    Reply,
    /// A text before the turn's last one.
    Commentary,
    /// Either (not told apart: the speaker reads both).
    Message,
}

/// Why a message was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// It was just said.
    Live,
    /// It was said while the speaker was away, after its watermark and within its catch-up.
    Catchup,
    /// The user asked for it (`speech.request`).
    Request,
}

/// A control sent to a speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CtlOp {
    /// Stop now and empty the queue.
    Stop,
    /// Pause.
    Pause,
    /// Resume.
    Resume,
    /// End the current message.
    Skip,
    /// Stop, empty the queue, then read the request messages of batch `value.batch`
    /// (`value.count` of them); those of any other batch are dropped.
    Replace,
    /// Live reading on (`value: true`) or off for this hub.
    Live,
    /// Use the voice `value`.
    Voice,
    /// Speak at `value` words per minute.
    Rate,
}

/// One item of a `speech.attach` stream (`{"stream": "speech", "item": {...}}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "lowercase")]
pub enum SpeechItem {
    /// A message to read.
    Msg {
        /// Its thread.
        thread: String,
        /// The seq of its final `agent.text`.
        seq: u64,
        /// Its turn.
        turn: Option<u64>,
        /// When it was said (RFC 3339).
        ts: String,
        /// What it is.
        kind: MsgKind,
        /// Its text, as written (Markdown), at most [`TEXT_MAX`] bytes.
        text: String,
        /// Not all of it: the rest could not be loaded, or was past [`TEXT_MAX`].
        #[serde(default)]
        truncated: bool,
        /// Why it was sent.
        origin: Origin,
        /// The `speech.request` batch it belongs to (request messages only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        batch: Option<u64>,
    },
    /// A control.
    Ctl {
        /// What to do.
        op: CtlOp,
        /// Its value (`replace`: `{batch, count}`; `live`: a bool; `voice`: a name; `rate`: words per
        /// minute).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
    },
    /// Messages of `thread` older than the catch-up span, after the speaker's watermark: not
    /// sent, counted.
    Missed {
        /// The thread.
        thread: String,
        /// How many.
        count: u64,
        /// The newest one's seq (what the watermark moves to once this is handled).
        upto: u64,
    },
    /// The feed now follows another orchestrator thread (`/orch`, `/clear`).
    Orchestrator {
        /// The new orchestrator thread.
        thread: String,
    },
}

/// What a speaker is playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Playing {
    /// The message's thread.
    pub thread: String,
    /// The message's seq.
    pub seq: u64,
    /// The utterance being said, from 1.
    pub chunk: u32,
    /// Of how many.
    pub of: u32,
}

/// `speech.state` params: what a speaker reports about itself, on the connection it attached
/// on.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechStateParams {
    /// What it plays from this hub, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<Playing>,
    /// Messages of this hub waiting.
    #[serde(default)]
    pub queued: u32,
    /// The position in the batch `/read N` asked for, if it plays one: (n, of).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<(u32, u32)>,
    /// Paused.
    #[serde(default)]
    pub paused: bool,
    /// Live reading on for this hub.
    #[serde(default)]
    pub live: bool,
    /// Its voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    /// Its rate, words per minute (0: the engine's default).
    #[serde(default)]
    pub rate: u32,
    /// What went wrong, one line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `speech.request` params: read messages now.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechRequestParams {
    /// The thread (id, name or alias); the orchestrator by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// The last this many messages (1 to [`REQUEST_MAX`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u32>,
    /// The message whose final `agent.text` has this seq.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Only the speaker named so (its name or device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

/// `speech.request`'s answer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeechRequestResult {
    /// The messages sent.
    pub count: u32,
    /// The speakers they were sent to.
    pub speakers: u32,
    /// The thread read.
    pub thread: String,
}

/// `speech.control` params.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechControlParams {
    /// What to do (not `replace`: that is `speech.request`'s).
    pub op: CtlOp,
    /// Its value (`live`: a bool; `voice`: a name; `rate`: words per minute, 80-500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    /// Only the speaker named so (its name or device).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speaker: Option<String>,
}

impl SpeechControlParams {
    /// Why these params are refused, if they are.
    pub fn error(&self) -> Option<String> {
        match (self.op, &self.value) {
            (CtlOp::Replace, _) => Some("op: replace is speech.request's".into()),
            (CtlOp::Live, Some(Value::Bool(_))) => None,
            (CtlOp::Live, _) => Some("live takes value true or false".into()),
            (CtlOp::Voice, Some(Value::String(v))) => name_error("voice", v, 128),
            (CtlOp::Voice, _) => Some("voice takes a name".into()),
            (CtlOp::Rate, Some(v)) if v.as_u64().is_some_and(|r| (80..=500).contains(&r)) => None,
            (CtlOp::Rate, _) => Some("rate takes 80 to 500 words per minute".into()),
            (_, None) => None,
            (_, Some(_)) => Some("this op takes no value".into()),
        }
    }
}

/// `speech.control`'s answer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeechControlResult {
    /// The speakers it was sent to.
    pub speakers: u32,
}

/// One attached speaker, as `speech.status` lists it.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeakerStatus {
    /// Its device.
    pub device: String,
    /// Its name.
    pub name: String,
    /// Its engine.
    pub engine: String,
    /// When it attached (RFC 3339).
    pub since: String,
    /// What it last reported (live as it attached until then).
    #[serde(flatten)]
    pub state: SpeechStateParams,
}

/// `speech.status`'s answer.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SpeechStatus {
    /// The attached speakers, in the order they attached.
    pub speakers: Vec<SpeakerStatus>,
}

/// A line of `twr speech feed --proto` on its stdout (what `twr speak --ssh` reads).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProxyOut {
    /// The first line: the proxy's version, its host and its workspaces.
    Hello {
        /// The proxy protocol (1).
        proxy: u32,
        /// The host's name.
        host: String,
        /// The workspaces it serves.
        workspaces: Vec<String>,
    },
    /// A workspace's hub is connected (again): attach to it now (a connection made before
    /// is gone).
    Up {
        /// The workspace.
        ws: String,
        /// Always true.
        up: bool,
    },
    /// A workspace's hub is not running (or went away): retried.
    Down {
        /// The workspace.
        ws: String,
        /// Always true.
        down: bool,
    },
    /// A line from a workspace's hub (a response or a stream item), as the hub sent it.
    Line {
        /// The workspace.
        ws: String,
        /// The line.
        msg: Value,
    },
}

/// A line `twr speak --ssh` writes to `twr speech feed --proto`'s stdin: a request for a
/// workspace's hub. Only `speech.attach` and `speech.state` are passed on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyIn {
    /// The workspace.
    pub ws: String,
    /// The request id.
    pub id: u64,
    /// The method.
    pub method: String,
    /// Its params.
    #[serde(default)]
    pub params: Value,
}

/// The methods the proxy passes on.
pub const PROXY_METHODS: &[&str] = &["speech.attach", "speech.state"];

/// The proxy protocol version.
pub const PROXY_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn says(md: &str) -> Vec<String> {
        sentences(md, &SpeakOpts::default())
    }

    fn turn(from: TurnFrom, twr: Option<Value>) -> TurnStarted {
        TurnStarted {
            prompt: "p".into(),
            from,
            ext: twr.map(|t| {
                let mut m = crate::event::JsonMap::new();
                m.insert("twr".into(), t);
                m
            }),
            ..Default::default()
        }
    }

    #[test]
    fn maintenance_turns_never_speak() {
        assert!(speakable_turn(&turn(TurnFrom::User, None)));
        assert!(speakable_turn(&turn(TurnFrom::Runtime, None)));
        assert!(speakable_turn(&turn(
            TurnFrom::Orchestrator,
            Some(json!({"notice": true, "watches_fired": ["w1"]}))
        )));
        for k in SILENT_MARKERS {
            let t = turn(
                TurnFrom::Orchestrator,
                Some(json!({ *k: true, "notice": true })),
            );
            assert!(!speakable_turn(&t), "{k}");
            let t = turn(TurnFrom::User, Some(json!({ *k: {"x": 1} })));
            assert!(!speakable_turn(&t), "{k} object");
        }
        // A marker present but null or false says nothing.
        assert!(speakable_turn(&turn(
            TurnFrom::User,
            Some(json!({"handoff": null, "compact": false}))
        )));
        // A moved thread's next message: its handoff goes before its prompt; it is a message.
        assert!(speakable_turn(&turn(
            TurnFrom::User,
            Some(json!({"runtime_handoff": "<twr-handoff>…</twr-handoff>", "resumes": "s1"}))
        )));
        // Not hydrated: a part of `ext` in the blob store could hide a marker.
        assert!(!speakable_turn(&turn(
            TurnFrom::User,
            Some(json!({"handoff_ref": "sha256:00"}))
        )));
        let mut t = turn(TurnFrom::User, None);
        let mut ext = crate::event::JsonMap::new();
        ext.insert("twr_ref".into(), json!("sha256:00"));
        t.ext = Some(ext);
        assert!(!speakable_turn(&t));
        assert!(turn_is_users(&turn(TurnFrom::User, None)));
        assert!(!turn_is_users(&turn(TurnFrom::Orchestrator, None)));
    }

    #[test]
    fn markup_goes_and_words_stay() {
        assert_eq!(
            says("## Done\n\nI **fixed** the _bug_ in `parse()`.\n\n> quoted text"),
            vec!["Done.", "I fixed the bug in parse().", "quoted text."]
        );
        assert_eq!(
            says("- one\n- two\n\n1. first\n2. second"),
            vec!["one.", "two.", "1. first.", "2. second."]
        );
        assert_eq!(
            says("- [x] build\n- [ ] test"),
            vec!["Done: build.", "To do: test."]
        );
    }

    #[test]
    fn links_urls_and_paths_read_short() {
        assert_eq!(
            says("See [the docs](https://example.com/a/b) or https://www.github.com/x/y?q=1."),
            vec!["See the docs or link to github.com."]
        );
        assert_eq!(says("<https://mosh.org/#faq>"), vec!["link to mosh.org."]);
        assert_eq!(
            says("Edited /home/me/code/twr/crates/twr-tui/src/app.rs:1301 and src/lib.rs."),
            vec!["Edited src/app.rs, line 1301 and src/lib.rs."]
        );
        assert_eq!(
            says("in crates/twr-core/src/speech.rs:12:5"),
            vec!["in src/speech.rs, line 12."]
        );
        let full = SpeakOpts {
            paths: PathMode::Full,
            ..Default::default()
        };
        assert_eq!(sentences("in /a/b/c/d.rs", &full), vec!["in /a/b/c/d.rs."]);
        assert_eq!(says("api → web"), vec!["api to web."]);
        // An image reads as "image" and its alt text, in a link too.
        assert_eq!(says("![logo](x.png) Done"), vec!["image logo Done."]);
        assert_eq!(
            says("see [![CI](b.svg)](https://u.dev)"),
            vec!["see image CI."]
        );
        // A label with words and a URL keeps its words.
        assert_eq!(
            says("[Retry https://api.example first](https://docs.example)"),
            vec!["Retry link to api.example first."]
        );
        // A link's label is cleaned like any text: no controls, invisibles or pictographs.
        let s = says("[ok\u{202E}evil\u{200B} 🚀 \u{7}go](https://x.dev)");
        assert_eq!(s, vec!["okevil go."]);
    }

    #[test]
    fn code_is_announced_read_or_skipped() {
        let md = "Run this:\n\n```rust\nfn main() {}\n\nlet x = 1;\n```\n\nThen `cargo test --workspace --locked --no-fail-fast -- something`.";
        assert_eq!(
            says(md),
            vec!["Run this:", "Code block, 2 lines, rust.", "Then code."]
        );
        let read = SpeakOpts {
            code: CodeMode::Read,
            ..Default::default()
        };
        assert_eq!(
            sentences(md, &read),
            vec!["Run this:", "fn main() {}.", "let x = 1;", "Then code."]
        );
        let skip = SpeakOpts {
            code: CodeMode::Skip,
            ..Default::default()
        };
        assert_eq!(sentences(md, &skip), vec!["Run this:", "Then code."]);
    }

    #[test]
    fn thread_sections_read_with_their_names() {
        let md = "Two updates.\n<thread name=\"api\">\nThe build passed.\nAll green.\n</thread>\n<thread name=\"web\">\n\nDeployed.\n\n</thread>\nThat's all.";
        assert_eq!(
            says(md),
            vec![
                "Two updates.",
                "api:",
                "The build passed. All green.",
                "web:",
                "Deployed.",
                "That's all."
            ]
        );
        // Only a tag alone on its line is one.
        assert_eq!(
            says("it said <thread name=\"x\"> inline"),
            vec!["it said inline."]
        );
        // A `<` that starts no tag is text.
        assert_eq!(
            says("<details>\n<summary>Why</summary>\nlatency < 5 ms when warm\n</details>"),
            vec!["Why latency < 5 ms when warm."]
        );
        // Any other HTML block is read without its tags, the text after it too.
        assert_eq!(
            says("<details>\nThe summary <!-- hidden --> stays.\n</details>\n\nNext."),
            vec!["The summary stays.", "Next."]
        );
    }

    #[test]
    fn tables_read_row_by_row() {
        let md = "| thread | state |\n|---|---|\n| api | done |\n| web | running |";
        assert_eq!(
            says(md),
            vec![
                "Table, 2 rows.",
                "thread, state.",
                "api, done.",
                "web, running."
            ]
        );
    }

    #[test]
    fn invisible_and_pictographs_are_dropped() {
        let md = "ok\u{202E}evil\u{200B} ✅ done ━━━ \u{1b}[31mred\u{7} 🚀";
        let s = says(md);
        assert_eq!(s.len(), 1);
        let s = &s[0];
        assert!(
            !s.chars()
                .any(|c| c.is_control() || invisible(c) || pictograph(c)),
            "{s:?}"
        );
        assert!(
            s.contains("evil") && s.contains("done") && s.contains("red"),
            "{s:?}"
        );
        assert_eq!(clean("a\tb\nc\u{0}d\u{9b}e"), "a b cde");
        assert!(says("━━━ 🚀 ✅").is_empty());
    }

    #[test]
    fn chunks_are_bounded_and_keep_every_word() {
        let long = "word ".repeat(400) + "end.";
        let sentence = format!("First sentence. {long} Second, with a comma; and more.");
        let chunks = utterances(&sentence, &SpeakOpts::default());
        assert!(chunks.len() > 1);
        for c in &chunks {
            assert!(c.chars().count() <= CHUNK_MAX, "{}", c.len());
            assert!(!c.is_empty());
        }
        let words: Vec<&str> = chunks.iter().flat_map(|c| c.split_whitespace()).collect();
        let want: Vec<&str> = sentence.split_whitespace().collect();
        assert_eq!(words, want);
        // A word longer than a chunk is cut anywhere, on a char boundary.
        let huge = "é".repeat(CHUNK_MAX * 2 + 7);
        let c = chunk(std::slice::from_ref(&huge));
        assert_eq!(c.concat(), huge);
        assert!(c.iter().all(|x| x.chars().count() <= CHUNK_MAX));
        // Short sentences share a chunk.
        assert_eq!(
            chunk(&["One.".into(), "Two.".into()]),
            vec!["One. Two.".to_string()]
        );
    }

    #[test]
    fn faithful_words_in_order() {
        let md = "The **api** thread finished: tests pass (12/12), and I pushed `main`.";
        assert_eq!(
            says(md),
            vec!["The api thread finished: tests pass (12/12), and I pushed main."]
        );
    }

    #[test]
    fn cap_and_estimate() {
        assert_eq!(cap_text("abc", 5), ("abc".into(), false));
        assert_eq!(cap_text("aé", 2), ("a".into(), true));
        let chunks = vec!["word ".repeat(350)];
        assert_eq!(estimate_secs(&chunks, 0), 120);
        assert_eq!(estimate_secs(&chunks, 350), 60);
    }

    #[test]
    fn params_are_checked() {
        let mut p = SpeechAttachParams {
            device: "ab-12".into(),
            name: "macbook".into(),
            engine: "say".into(),
            voice: None,
            live: true,
            read: default_read(),
            turn_from: TurnsRead::All,
            watermarks: BTreeMap::new(),
            catchup_secs: 120,
        };
        assert_eq!(p.error(), None);
        p.device = "a b".into();
        assert!(p.error().is_some());
        p.device = "ab".into();
        p.name = "x\u{202E}".into();
        assert!(p.error().is_some());
        p.name = "x".into();
        p.read.clear();
        assert!(p.error().is_some());
        let v: SpeechAttachParams =
            serde_json::from_value(json!({"device": "d", "name": "n", "engine": "say"})).unwrap();
        assert_eq!(v.read, default_read());
        assert_eq!(v.catchup_secs, 120);
        assert!(serde_json::from_value::<SpeechAttachParams>(
            json!({"device": "d", "name": "n", "engine": "say", "extra": 1})
        )
        .is_err());

        let c = |op, value: Option<Value>| SpeechControlParams {
            op,
            value,
            speaker: None,
        };
        assert_eq!(c(CtlOp::Stop, None).error(), None);
        assert!(c(CtlOp::Stop, Some(json!(1))).error().is_some());
        assert!(c(CtlOp::Replace, Some(json!(1))).error().is_some());
        assert_eq!(c(CtlOp::Live, Some(json!(true))).error(), None);
        assert!(c(CtlOp::Live, None).error().is_some());
        assert_eq!(c(CtlOp::Rate, Some(json!(200))).error(), None);
        assert!(c(CtlOp::Rate, Some(json!(20))).error().is_some());
        assert_eq!(c(CtlOp::Voice, Some(json!("Ava"))).error(), None);
    }

    #[test]
    fn items_round_trip() {
        let items = vec![
            SpeechItem::Msg {
                thread: "t".into(),
                seq: 4,
                turn: Some(2),
                ts: "2026-10-27T12:00:00.000Z".into(),
                kind: MsgKind::Reply,
                text: "hi".into(),
                truncated: false,
                origin: Origin::Live,
                batch: None,
            },
            SpeechItem::Ctl {
                op: CtlOp::Replace,
                value: Some(json!({"batch": 1, "count": 3})),
            },
            SpeechItem::Missed {
                thread: "t".into(),
                count: 2,
                upto: 9,
            },
            SpeechItem::Orchestrator { thread: "o".into() },
        ];
        for it in items {
            let v = serde_json::to_value(&it).unwrap();
            assert!(v["item"].is_string());
            assert_eq!(serde_json::from_value::<SpeechItem>(v).unwrap(), it);
        }
        let hello: ProxyOut =
            serde_json::from_value(json!({"proxy": 1, "host": "h", "workspaces": ["default"]}))
                .unwrap();
        assert!(matches!(hello, ProxyOut::Hello { .. }));
        let up: ProxyOut = serde_json::from_value(json!({"ws": "w", "up": true})).unwrap();
        assert!(matches!(up, ProxyOut::Up { .. }));
        let down: ProxyOut = serde_json::from_value(json!({"ws": "w", "down": true})).unwrap();
        assert!(matches!(down, ProxyOut::Down { .. }));
        let line: ProxyOut =
            serde_json::from_value(json!({"ws": "w", "msg": {"id": 1, "result": {}}})).unwrap();
        assert!(matches!(line, ProxyOut::Line { .. }));
    }
}

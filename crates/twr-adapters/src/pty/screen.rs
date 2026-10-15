//! DEGRADED terminal-output handling for the PTY adapter: streaming UTF-8 decoding, ANSI
//! stripping, line assembly with carriage-return overwrites, spinner filtering and
//! prompt-tail classification. Pure, no IO.
//!
//! This is not a terminal emulator: cursor movement, screen clearing and full-screen TUIs
//! are not rendered, only removed. What is left is the text a line-oriented CLI printed.

/// Decodes UTF-8 across chunk boundaries (an incomplete trailing sequence waits for the next
/// chunk; invalid bytes become U+FFFD).
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    buf: Vec<u8>,
}

impl Utf8Decoder {
    /// Decodes `bytes` after whatever was left over.
    pub fn decode(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.buf) {
                Ok(s) => {
                    out.push_str(s);
                    self.buf.clear();
                    return out;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    out.push_str(std::str::from_utf8(&self.buf[..valid]).unwrap_or_default());
                    match e.error_len() {
                        Some(n) => {
                            out.push('\u{FFFD}');
                            self.buf.drain(..valid + n);
                        }
                        None => {
                            self.buf.drain(..valid);
                            return out;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Parse {
    #[default]
    Text,
    Escape,
    Csi,
    Str,
    StrEscape,
    Nf,
}

/// Streaming ANSI stripper: removes CSI, OSC / DCS / SOS / PM / APC strings, nF and two-byte
/// escapes, and C0 controls except `\n`, `\r`, `\t` and backspace (kept for the line buffer),
/// across chunk boundaries.
#[derive(Debug, Default)]
pub struct AnsiStripper {
    state: Parse,
}

impl AnsiStripper {
    /// Strips `s`, continuing any escape sequence the previous chunk ended in.
    pub fn strip(&mut self, s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            self.step(c, &mut out);
        }
        out
    }

    fn step(&mut self, c: char, out: &mut String) {
        match self.state {
            Parse::Text => match c {
                '\x1b' => self.state = Parse::Escape,
                '\u{9b}' => self.state = Parse::Csi,
                '\n' | '\r' | '\t' | '\x08' => out.push(c),
                c if c.is_control() => {}
                c => out.push(c),
            },
            Parse::Escape => {
                self.state = match c {
                    '[' => Parse::Csi,
                    ']' | 'P' | 'X' | '^' | '_' => Parse::Str,
                    '\x20'..='\x2f' => Parse::Nf,
                    _ => Parse::Text,
                }
            }
            Parse::Csi => {
                if ('\x40'..='\x7e').contains(&c) {
                    self.state = Parse::Text;
                }
            }
            Parse::Str => match c {
                '\x07' => self.state = Parse::Text,
                '\x1b' => self.state = Parse::StrEscape,
                _ => {}
            },
            Parse::StrEscape => {
                self.state = Parse::Text;
                if c != '\\' {
                    self.step(c, out);
                }
            }
            Parse::Nf => {
                if !('\x20'..='\x2f').contains(&c) {
                    self.state = Parse::Text;
                }
            }
        }
    }
}

/// Longest private-mode sequence (`ESC [ ? params h|l`) kept across a chunk boundary.
const MODE_SEQ_MAX: usize = 64;

/// Tracks whether the program enabled bracketed paste: DECSET 2004 (`ESC [ ? 2004 h`, also
/// inside a parameter list such as `ESC [ ? 1049;2004 h`) turns it on, DECRST 2004
/// (`ESC [ ? 2004 l`) or a full reset (`ESC c`) turns it off. Sequences split across chunks
/// are completed with the next chunk. Off until the program says otherwise.
#[derive(Debug, Default)]
pub struct PasteMode {
    enabled: bool,
    pending: String,
}

impl PasteMode {
    /// Whether the program has bracketed paste enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Scans decoded (not yet stripped) output.
    pub fn feed(&mut self, s: &str) {
        let mut buf = std::mem::take(&mut self.pending);
        buf.push_str(s);
        let b = buf.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] != 0x1b {
                i += 1;
                continue;
            }
            // ESC at `i`.
            let Some(&next) = b.get(i + 1) else {
                self.keep(&buf[i..]);
                return;
            };
            if next == b'c' {
                self.enabled = false;
                i += 2;
                continue;
            }
            if next != b'[' {
                i += 1;
                continue;
            }
            let Some(&q) = b.get(i + 2) else {
                self.keep(&buf[i..]);
                return;
            };
            if q != b'?' {
                i += 2;
                continue;
            }
            let params_start = i + 3;
            let mut j = params_start;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b';') {
                j += 1;
            }
            if j == b.len() {
                self.keep(&buf[i..]);
                return;
            }
            let params = &buf[params_start..j];
            let on = match b[j] {
                b'h' => Some(true),
                b'l' => Some(false),
                _ => None,
            };
            if let Some(on) = on {
                if params.split(';').any(|p| p == "2004") {
                    self.enabled = on;
                }
            }
            i = j + 1;
        }
    }

    fn keep(&mut self, rest: &str) {
        if rest.len() <= MODE_SEQ_MAX {
            self.pending = rest.to_string();
        }
    }
}

/// Maximum chars kept in the unfinished line.
const PARTIAL_MAX: usize = 16 * 1024;

/// What a terminal shows of one line: the text after its last carriage return.
pub fn visible(line: &str) -> &str {
    let l = line.trim_end_matches('\r');
    l.rsplit('\r').next().unwrap_or(l)
}

/// Assembles stripped text into lines. Remembers how much of the unfinished line (the
/// "tail") was already emitted, so a line that was shown as a prompt and then completed (by
/// the echo of an answer) is not shown twice.
#[derive(Debug, Default)]
pub struct LineBuffer {
    partial: String,
    emitted: String,
}

impl LineBuffer {
    /// Adds stripped text; returns the completed lines, each without the part already emitted
    /// as a tail, trailing whitespace trimmed.
    pub fn feed(&mut self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for c in text.chars() {
            match c {
                '\n' => {
                    let line = visible(&self.partial).trim_end().to_string();
                    let shown = self.emitted.trim_end();
                    let rest = match line.strip_prefix(shown) {
                        Some(rest) if !shown.is_empty() => rest.trim_start().to_string(),
                        _ => line,
                    };
                    out.push(rest);
                    self.partial.clear();
                    self.emitted.clear();
                }
                '\x08' => {
                    self.partial.pop();
                }
                c => self.partial.push(c),
            }
        }
        // Keep only what can still show: the text after the last carriage return (a trailing
        // `\r` stays, it may be the first half of `\r\n`).
        let search = self.partial.strip_suffix('\r').unwrap_or(&self.partial);
        if let Some(i) = search.rfind('\r') {
            self.partial.drain(..=i);
        }
        if self.partial.len() > PARTIAL_MAX {
            let cut = self.partial.len() - PARTIAL_MAX;
            let cut = (cut..self.partial.len())
                .find(|i| self.partial.is_char_boundary(*i))
                .unwrap_or(self.partial.len());
            self.partial.drain(..cut);
            self.emitted.clear();
        }
        out
    }

    /// The unfinished line as a terminal would show it.
    pub fn tail(&self) -> String {
        visible(&self.partial).to_string()
    }

    /// The part of the tail not emitted yet, marking the whole tail emitted. Returns the
    /// whole tail when it was rewritten since (it no longer starts with what was emitted).
    pub fn take_tail(&mut self) -> String {
        let t = self.tail();
        let rest = match t.strip_prefix(self.emitted.as_str()) {
            Some(rest) => rest.to_string(),
            None => t.clone(),
        };
        self.emitted = t;
        rest
    }
}

/// Braille spinner frames (⠋⠙⠹...) and quarter-circle spinners.
fn is_spinner_lead(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
        || matches!(c, '◐' | '◓' | '◑' | '◒' | '◴' | '◷' | '◶' | '◵')
}

/// Whether a line is a spinner frame: only spinner glyphs, or a braille / quarter-circle
/// frame followed by a status ("⠋ Thinking...").
pub fn is_spinner_line(line: &str) -> bool {
    let t = line.trim();
    let Some(first) = t.chars().next() else {
        return false;
    };
    is_spinner_lead(first)
        || t.chars().all(|c| {
            c.is_whitespace()
                || is_spinner_lead(c)
                || matches!(
                    c,
                    '|' | '/' | '-' | '\\' | '✻' | '✶' | '✳' | '✢' | '✽' | '·' | '*'
                )
        })
}

/// Whether a line carries nothing worth showing (blank or a spinner frame).
pub fn is_noise(line: &str) -> bool {
    line.trim().is_empty() || is_spinner_line(line)
}

/// What an unfinished last line, left alone while output is idle, looks like.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tail {
    /// A question waiting for an answer (`?`, `:`, `[y/N]`, `(y/n)`...). `options` are
    /// `["y", "n"]` for yes/no prompts, else empty (free text).
    Question {
        /// Suggested answers.
        options: Vec<String>,
    },
    /// The program's own input prompt (`>`, `$`, `#`, `%`, `❯`, `›`, `»`): it is waiting for
    /// the next instruction, so the turn is over.
    InputPrompt,
    /// Nothing prompt-like (or no unfinished line).
    Other,
}

/// Yes/no markers recognized at the end of a prompt (lowercase).
const YES_NO: &[&str] = &[
    "[y/n]",
    "(y/n)",
    "[yes/no]",
    "(yes/no)",
    "y/n",
    "(y)es/(n)o",
];

/// Classifies an unfinished last line.
pub fn classify_tail(tail: &str) -> Tail {
    let t = tail.trim();
    if t.is_empty() || is_spinner_line(t) {
        return Tail::Other;
    }
    let lower = t.to_lowercase();
    let core = lower.trim_end_matches(|c: char| c == ':' || c == '?' || c.is_whitespace());
    // `[Y/n]`-style markers, possibly followed by a default in brackets ("[Yes]").
    let core_no_default = match core.rfind(" [") {
        Some(i) if core.ends_with(']') && !YES_NO.iter().any(|m| core.ends_with(m)) => {
            core[..i].trim_end()
        }
        _ => core,
    };
    if YES_NO
        .iter()
        .any(|m| core.ends_with(m) || core_no_default.ends_with(m))
    {
        return Tail::Question {
            options: vec!["y".into(), "n".into()],
        };
    }
    if t.ends_with('?') || t.ends_with(':') {
        return Tail::Question {
            options: Vec::new(),
        };
    }
    match t.chars().last() {
        Some('>' | '$' | '#' | '%' | '❯' | '›' | '»' | '➜') => Tail::InputPrompt,
        _ => Tail::Other,
    }
}

/// Texts (matched lowercase) that mark a selection menu or a confirmation dialog.
const MENU_HINTS: &[&str] = &[
    "enter to confirm",
    "enter to select",
    "esc to cancel",
    "esc to exit",
    "esc to go back",
    "(use arrow keys)",
    "↑/↓",
    "↑↓",
    "press enter to continue",
];

/// Selection cursors drawn before the highlighted menu entry.
const CURSORS: &[char] = &['❯', '›', '>', '▸', '▶', '→', '●', '◉', '*'];

/// `1. text` / `2) text` (after an optional selection cursor): an entry of a numbered menu.
pub fn is_numbered_option(line: &str) -> bool {
    let t = line.trim_start().trim_start_matches(CURSORS).trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 2 {
        return false;
    }
    let rest = &t[digits..];
    let Some(rest) = rest.strip_prefix(['.', ')']) else {
        return false;
    };
    rest.starts_with(' ') && !rest.trim().is_empty()
}

/// A numbered entry with a selection cursor in front of it (`❯ 1. Yes`).
fn is_cursor_option(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with(CURSORS) && is_numbered_option(t)
}

/// Strong signs of a menu or dialog in `lines` (the completed lines since twr last typed) or
/// the unfinished `tail`: a hint such as "Enter to confirm" / "Esc to cancel", or a numbered
/// entry under a selection cursor (`❯ 1. Yes`).
pub fn menu_hint(lines: &[String], tail: &str) -> bool {
    lines
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(tail))
        .any(|l| {
            let lower = l.to_lowercase();
            MENU_HINTS.iter().any(|h| lower.contains(h)) || is_cursor_option(l)
        })
}

/// Whether the last two or more completed lines are numbered entries (`1. a` / `2. b`): a
/// numbered menu may be waiting for a choice, whatever the unfinished line shows.
pub fn numbered_block(lines: &[String]) -> bool {
    lines
        .iter()
        .rev()
        .filter(|l| !l.trim().is_empty())
        .take_while(|l| is_numbered_option(l))
        .count()
        >= 2
}

/// [`menu_hint`] or [`numbered_block`]: typing a task here could pick a menu entry.
pub fn is_menu(lines: &[String], tail: &str) -> bool {
    menu_hint(lines, tail) || numbered_block(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_across_chunks() {
        let mut d = Utf8Decoder::default();
        let bytes = "héllo ✓".as_bytes();
        let (a, b) = bytes.split_at(2); // splits "é"
        assert_eq!(d.decode(a), "h");
        assert_eq!(d.decode(b), "éllo ✓");
        assert_eq!(d.decode(b"a\xffb"), "a\u{FFFD}b");
    }

    #[test]
    fn ansi_across_chunks() {
        let mut s = AnsiStripper::default();
        assert_eq!(s.strip("\x1b[3"), "");
        assert_eq!(s.strip("1mred\x1b[0m ok"), "red ok");
        assert_eq!(s.strip("\x1b]0;title"), "");
        assert_eq!(s.strip("\x07text"), "text");
        assert_eq!(s.strip("\x1b]8;;u\x1b\\link"), "link");
        assert_eq!(s.strip("\x1b(Bplain\x1b7"), "plain");
        assert_eq!(s.strip("bell\x07\r\n\tx\x08"), "bell\r\n\tx\x08");
    }

    #[test]
    fn lines_and_carriage_returns() {
        let mut l = LineBuffer::default();
        assert_eq!(l.feed("a\r\nb"), ["a"]);
        assert_eq!(l.feed("c\r"), Vec::<String>::new());
        assert_eq!(l.feed("\n"), ["bc"]);
        assert_eq!(l.feed("50%\r100%\rdone  \n"), ["done"]);
        assert_eq!(l.feed("abx\x08c\n"), ["abc"]);
        assert_eq!(l.feed("⠋ work\r⠙ work\rOK"), Vec::<String>::new());
        assert_eq!(l.tail(), "OK");
    }

    #[test]
    fn emitted_tails_are_not_repeated() {
        let mut l = LineBuffer::default();
        l.feed("Continue? [y/N] ");
        assert_eq!(l.take_tail(), "Continue? [y/N] ");
        assert_eq!(l.take_tail(), "");
        assert_eq!(l.feed("y\n"), ["y"]);
        l.feed("⠋ a");
        assert_eq!(l.take_tail(), "⠋ a");
        l.feed("\r⠙ b");
        assert_eq!(l.take_tail(), "⠙ b", "a rewritten tail is taken whole");
        assert_eq!(l.feed("\rfinal\n"), ["final"]);
    }

    #[test]
    fn tails() {
        let yn = Tail::Question {
            options: vec!["y".into(), "n".into()],
        };
        let open = Tail::Question {
            options: Vec::new(),
        };
        for t in [
            "Continue? [y/N] ",
            "Proceed (y/n)",
            "Overwrite [Y/n]: ",
            "Add file to the chat? (Y)es/(N)o [Yes]: ",
            "Delete? [yes/no]",
        ] {
            assert_eq!(classify_tail(t), yn, "{t}");
        }
        for t in ["What is your name? ", "Password:", "Enter a value: "] {
            assert_eq!(classify_tail(t), open, "{t}");
        }
        for t in ["> ", "$ ", "aider> ", "❯", "user@host:~# "] {
            assert_eq!(classify_tail(t), Tail::InputPrompt, "{t}");
        }
        for t in ["", "   ", "Downloading", "⠋ Thinking..."] {
            assert_eq!(classify_tail(t), Tail::Other, "{t}");
        }
    }

    #[test]
    fn menus() {
        let v = |ls: &[&str]| ls.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Claude Code's trust dialog.
        let trust = v(&[
            "Do you trust the files in this folder?",
            "❯ 1. Yes, proceed",
            "  2. No, exit",
        ]);
        assert!(menu_hint(&trust, "Enter to confirm · Esc to exit"));
        assert!(menu_hint(&trust, ""));
        assert!(numbered_block(&trust));
        assert!(is_menu(&v(&["Pick one"]), "❯ 1. Yes"));
        assert!(is_menu(&v(&["? Choose (Use arrow keys)"]), "> "));
        // A numbered list right before the prompt may be a menu: not typed into.
        assert!(is_menu(&v(&["Choose:", "1. a", "2) b"]), "> "));
        assert!(!menu_hint(&v(&["Choose:", "1. a", "2) b"]), "> "));
        // Not menus.
        assert!(!is_menu(&v(&["1. a", "done"]), "> "));
        assert!(!is_menu(&v(&["version 1.2 ready"]), "> "));
        assert!(!is_menu(&[], "> "));
        assert!(!is_numbered_option("123. x"));
        assert!(!is_numbered_option("1.5 GB"));
        assert!(is_numbered_option("  › 3. Maybe"));
    }

    #[test]
    fn noise() {
        assert!(is_noise("  "));
        assert!(is_noise("⠋ Thinking"));
        assert!(is_noise("|"));
        assert!(!is_noise("- item"));
        assert!(!is_noise("done"));
    }

    #[test]
    fn paste_mode_tracking() {
        let mut m = PasteMode::default();
        assert!(!m.enabled());
        m.feed("hello \x1b[?25l> ");
        assert!(!m.enabled(), "another private mode");
        m.feed("\x1b[?1049;2004h");
        assert!(m.enabled(), "in a parameter list");
        m.feed("x\x1b[?2004l");
        assert!(!m.enabled());
        // Split across chunks at every position.
        let seq = "ab\x1b[?2004hcd";
        for cut in 0..=seq.len() {
            let mut m = PasteMode::default();
            m.feed(&seq[..cut]);
            m.feed(&seq[cut..]);
            assert!(m.enabled(), "cut at {cut}");
        }
        m.feed("\x1b[?2004h");
        m.feed("\x1bc");
        assert!(!m.enabled(), "a full reset turns it off");
        // Not DECSET: a plain CSI with the same number.
        m.feed("\x1b[2004h");
        assert!(!m.enabled());
    }
}

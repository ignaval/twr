//! Output noise filtering for agent text before display (TUI transcript, report builder).

/// Removes ANSI escape sequences (CSI `ESC [ ... final`, OSC `ESC ] ... BEL|ESC \`, DCS / SOS /
/// PM / APC strings, and two-byte `ESC x` sequences) and other C0 control characters except
/// `\n`, `\r` and `\t`.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            if !c.is_control() || matches!(c, '\n' | '\r' | '\t') {
                out.push(c);
            }
            continue;
        }
        match chars.next() {
            // CSI: parameters and intermediates, then one final byte in 0x40..=0x7E.
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC / DCS / SOS / PM / APC: until BEL or ST (ESC \).
            Some(']' | 'P' | 'X' | '^' | '_') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // nF sequences (e.g. `ESC ( B`): intermediates 0x20..=0x2F, then a final byte.
            Some(c) if ('\x20'..='\x2f').contains(&c) => {
                for c in chars.by_ref() {
                    if !('\x20'..='\x2f').contains(&c) {
                        break;
                    }
                }
            }
            // Two-byte sequences (`ESC 7`, `ESC M`, ...) or a trailing lone ESC.
            Some(_) | None => {}
        }
    }
    out
}

/// Braille spinner frames (⠋⠙⠹...) and quarter-circle spinners.
fn is_spinner_lead(c: char) -> bool {
    ('\u{2800}'..='\u{28ff}').contains(&c)
        || matches!(c, '◐' | '◓' | '◑' | '◒' | '◴' | '◷' | '◶' | '◵')
}

/// Glyphs that are spinner frames when a line holds nothing else.
fn is_spinner_glyph(c: char) -> bool {
    is_spinner_lead(c)
        || matches!(
            c,
            '|' | '/' | '-' | '\\' | '✻' | '✶' | '✳' | '✢' | '✽' | '·' | '*'
        )
}

/// Whether a line (ANSI already stripped) is a spinner frame: only spinner glyphs, or a
/// braille / quarter-circle frame followed by a status ("⠋ Thinking...").
pub fn is_spinner_line(line: &str) -> bool {
    let t = line.trim();
    let Some(first) = t.chars().next() else {
        return false;
    };
    is_spinner_lead(first) || t.chars().all(|c| c.is_whitespace() || is_spinner_glyph(c))
}

/// Cleans agent text for display: strips ANSI escapes, keeps only what a terminal would show
/// after carriage returns (the text after a line's last `\r`), drops whitespace-only and
/// spinner lines, and trims trailing whitespace. `None` when nothing is left.
pub fn clean_text(s: &str) -> Option<String> {
    let stripped = strip_ansi(s);
    let lines: Vec<&str> = stripped
        .split('\n')
        .map(|l| {
            let l = l.trim_end_matches('\r');
            l.rsplit('\r').next().unwrap_or(l).trim_end()
        })
        .filter(|l| !l.trim().is_empty() && !is_spinner_line(l))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;38;5;208mbold\x1b[m!"), "bold!");
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
        assert_eq!(
            strip_ansi("\x1b]8;;https://x.y\x1b\\link\x1b]8;;\x1b\\"),
            "link"
        );
        assert_eq!(strip_ansi("\x1b(Bplain"), "plain");
        assert_eq!(strip_ansi("a\x1b7b\x1b8c"), "abc");
        assert_eq!(strip_ansi("bell\x07 and \x08bs"), "bell and bs");
        assert_eq!(strip_ansi("tab\tnl\n"), "tab\tnl\n");
        assert_eq!(strip_ansi("trailing\x1b"), "trailing");
        assert_eq!(strip_ansi("\x1b[2Kcleared"), "cleared");
        assert_eq!(strip_ansi("héllo ✓"), "héllo ✓");
    }

    #[test]
    fn spinner_lines() {
        for l in [
            "⠋",
            "⠙ Thinking...",
            "  ⣾ working  ",
            "|",
            "/",
            "-",
            "\\",
            "✻",
            "◐ loading",
            "· ✢ *",
        ] {
            assert!(is_spinner_line(l), "{l:?}");
        }
        for l in [
            "- item",
            "* bullet point",
            "a | b",
            "done",
            "",
            "   ",
            "✓ tests pass",
        ] {
            assert!(!is_spinner_line(l), "{l:?}");
        }
    }

    #[test]
    fn clean_text_drops_noise() {
        assert_eq!(clean_text(""), None);
        assert_eq!(clean_text("  \n\t\n"), None);
        assert_eq!(clean_text("\x1b[2K\x1b[1G⠋ Working\n\x1b[0m"), None);
        assert_eq!(clean_text("hello").as_deref(), Some("hello"));
        assert_eq!(
            clean_text("\x1b[32m✓\x1b[0m done\n\n⠙ Thinking\n  indented  \r\n- item\n").as_deref(),
            Some("✓ done\n  indented\n- item")
        );
        // Carriage-return progress: only the last frame survives.
        assert_eq!(
            clean_text("⠋ 10%\r⠙ 50%\rFinished\n").as_deref(),
            Some("Finished")
        );
        assert_eq!(clean_text("⠋ 10%\r⠙ 50%\r\n"), None);
        assert_eq!(
            clean_text("line one\r\nline two\r\n").as_deref(),
            Some("line one\nline two")
        );
    }
}

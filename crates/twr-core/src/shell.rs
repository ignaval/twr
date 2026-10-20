//! Shell commands the user runs from the TUI (`!`, `shell.run` in the hub): what both the hub
//! and the TUI make of their output.

/// Removes from `text` what would move a terminal's cursor or change its colours: escape
/// sequences (CSI, OSC and two-byte ones), a carriage return's overwritten start of a line
/// (what a progress bar redrew), and every other control character but tabs (kept as spaces)
/// and line breaks.
pub fn plain_output(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut line_start = 0;
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    // Parameters and intermediates, up to the final byte.
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    // Up to BEL or ST (ESC \).
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            '\r' if chars.peek() == Some(&'\n') => {}
            '\r' => out.truncate(line_start),
            '\n' => {
                out.push('\n');
                line_start = out.len();
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_keeps_text_and_drops_what_moves_the_terminal() {
        assert_eq!(plain_output("plain\nlines\n"), "plain\nlines\n");
        assert_eq!(plain_output("\x1b[31mred\x1b[0m ok"), "red ok");
        assert_eq!(plain_output("\x1b]0;title\x07after"), "after");
        assert_eq!(plain_output("\x1b]8;;http://x\x1b\\link"), "link");
        // A progress bar redrawn with CR: what it showed last.
        assert_eq!(plain_output("10%\r50%\r100%\ndone"), "100%\ndone");
        assert_eq!(plain_output("crlf\r\nline"), "crlf\nline");
        assert_eq!(plain_output("a\tb\x07\x08c"), "a    bc");
    }
}

//! What a recognizer's output becomes: the text of one message.

/// The longest transcript kept, in bytes: two minutes of speech is well under it.
pub const MAX_BYTES: usize = 8 * 1024;

/// `raw` (a recognizer's stdout) as one line of text: whisper's markers for what is no speech
/// (`[BLANK_AUDIO]`, `(silence)`, `[Music]`, `*coughs*`) dropped, control characters and runs
/// of whitespace made single spaces, at most [`MAX_BYTES`]. Empty: nothing was said.
pub fn clean(raw: &str) -> String {
    let mut out = String::new();
    let mut skip_until: Option<char> = None;
    for c in raw.chars() {
        if let Some(end) = skip_until {
            if c == end {
                skip_until = None;
            }
            continue;
        }
        match c {
            '[' => skip_until = Some(']'),
            '(' if raw.contains(')') => skip_until = Some(')'),
            c if c.is_control() || c.is_whitespace() => {
                if !out.ends_with(' ') && !out.is_empty() {
                    out.push(' ');
                }
            }
            c => out.push(c),
        }
    }
    let mut out = out.trim().to_string();
    // A marker whisper writes as `*music*`.
    if out.starts_with('*') && out.ends_with('*') && !out[1..out.len() - 1].contains('*') {
        out.clear();
    }
    if out.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    // Nothing but punctuation ("." for a breath) is nothing said.
    if !out.chars().any(char::is_alphanumeric) {
        out.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_and_breaks_go() {
        assert_eq!(
            clean(" Check the api thread.\n And the docs.\n"),
            "Check the api thread. And the docs."
        );
        assert_eq!(clean("[BLANK_AUDIO]\n"), "");
        assert_eq!(clean(" (silence) "), "");
        assert_eq!(clean("*music*"), "");
        assert_eq!(clean(" . "), "");
        assert_eq!(
            clean("Stop [inaudible] the api worker"),
            "Stop the api worker"
        );
        assert_eq!(clean("a\u{7}b\tc"), "a b c");
    }

    #[test]
    fn a_long_transcript_is_cut_on_a_character() {
        let long = "é".repeat(MAX_BYTES);
        let c = clean(&long);
        assert!(c.len() <= MAX_BYTES);
        assert!(c.chars().all(|ch| ch == 'é'));
    }
}

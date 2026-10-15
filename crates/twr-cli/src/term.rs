//! Terminal-safe output. What the CLI prints includes text agents control (attention
//! summaries and options, thread names, prompts, replies, reports, hub errors quoting them):
//! control characters in it are escaped, so it cannot move the cursor, clear the screen, set
//! the window title or otherwise drive the terminal. Every `print!`-style output of the CLI
//! goes through [`out!`] / [`outln!`] / [`err!`] / [`errln!`] (or [`clean`] directly).

use std::borrow::Cow;

/// Whether `c` is escaped by [`clean`]: C0 controls except newline and tab, DEL, and C1
/// controls.
fn escaped(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || ('\u{80}'..='\u{9f}').contains(&c)
}

/// `s` with every control character but newline and tab written as `\uXXXX` (also valid
/// inside a JSON string, so JSON output stays JSON and means the same).
pub fn clean(s: &str) -> Cow<'_, str> {
    if !s.chars().any(escaped) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if escaped(c) {
            out.push_str(&format!("\\u{:04x}", u32::from(c)));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// `print!` through [`clean`].
macro_rules! out {
    ($($arg:tt)*) => {
        ::std::print!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `println!` through [`clean`].
macro_rules! outln {
    () => {
        ::std::println!()
    };
    ($($arg:tt)*) => {
        ::std::println!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `eprint!` through [`clean`].
macro_rules! err {
    ($($arg:tt)*) => {
        ::std::eprint!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

/// `eprintln!` through [`clean`].
macro_rules! errln {
    ($($arg:tt)*) => {
        ::std::eprintln!("{}", $crate::term::clean(&::std::format!($($arg)*)))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_are_escaped_and_text_kept() {
        assert!(matches!(clean("plain\ttext\nok ü"), Cow::Borrowed(_)));
        assert_eq!(
            clean("a\u{1b}]0;x\u{7}b\u{1b}[2J\r\u{7f}\u{9b}31m\u{0}"),
            "a\\u001b]0;x\\u0007b\\u001b[2J\\u000d\\u007f\\u009b31m\\u0000"
        );
        // JSON stays JSON, with the same meaning.
        let j = serde_json::json!({ "s": "x\u{9b}y\u{1b}" }).to_string();
        let back: serde_json::Value = serde_json::from_str(&clean(&j)).unwrap();
        assert_eq!(back["s"], "x\u{9b}y\u{1b}");
    }
}

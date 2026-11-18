//! The text of a confirm card: a faithful, lossless rendering of the actions the card holds,
//! and its inverse.
//!
//! The hub (`twr-hub` `hub::proposal`) writes a confirm card's summary with [`render`], and
//! Confirm applies exactly the actions stored with it, so what the user reads must say exactly
//! what those actions are. Thread names, option labels, mode ids and item summaries come from
//! users and agents, so every one of them is written with [`escape`]: control, format and
//! other invisible characters (and combining marks with no base) become visible `\u{200B}`
//! escapes, before the hub's terminal sanitizing (which would drop control characters
//! silently). A mode id the thread's runtime does not know as one of its own (a generic ACP
//! agent's id, kept as given) is marked `custom mode "..."`. [`parse`] reads a summary back,
//! so a summary that does not parse to its actions is not their rendering.
//!
//! Format: `From your message: ` then the actions joined with ` · `:
//!
//! - `allow <thread>` / `deny <thread>` (a permission's bare answer), or
//!   `answer <thread>: "<label>"`, then ` (<item summary>)` when the item is known;
//! - `set <thread> → <mode>`, or `set <thread> → custom mode "<mode>"`;
//! - `move <thread> → <runtime>`, then `, model "<model>"` and `, effort "<effort>"` for each
//!   one given (a thread moved to another runtime, `hub::switch`);
//! - `config <key> = <value> (was <value>)`, or `(was unset)`: a key of twr's `config.toml`
//!   set (`hub::config_set`). The key's parts are joined with `.`, each bare when it is only
//!   ASCII letters, digits, `_` and `-`, else quoted; a string value is quoted, a boolean or a
//!   number written as TOML writes it.

/// What a card's summary starts with.
pub const HEAD: &str = "From your message: ";

/// What separates two actions on a card.
pub const SEP: &str = " · ";

/// One action as a card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardLine {
    /// Answer an item of thread `thread` (its name) with option `answer` (as listed); `item`:
    /// the item's summary as shown (cut by the hub), when known.
    Answer {
        thread: String,
        answer: String,
        item: Option<String>,
    },
    /// Set thread `thread` (its name) to mode `mode` (the id stored); `custom`: an id the
    /// thread's runtime does not know as one of its own (twr's or the runtime's documented ids).
    Mode {
        thread: String,
        mode: String,
        custom: bool,
    },
    /// Move thread `thread` (its name) to runtime `runtime` (an agent runtime or a configured
    /// preset, as named), with `model` and `effort` when given (else the runtime's defaults).
    Move {
        thread: String,
        runtime: String,
        model: Option<String>,
        effort: Option<String>,
    },
    /// Set key `key` (its parts: `["tui", "suggest"]`) of twr's config file to `value`;
    /// `before`: what the file had (`None`: unset).
    Config {
        key: Vec<String>,
        value: CardValue,
        before: Option<CardValue>,
    },
}

/// A config value as a card shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardValue {
    /// A string, shown quoted.
    Str(String),
    /// A boolean or a number as TOML writes it (`false`, `3`, `0.8`); never empty.
    Literal(String),
}

/// Whether `c` is hidden wherever it stands: what Rust's `Debug` escapes after a letter
/// (controls, format characters such as U+200B, separators other than the space, variation
/// selectors), except the quotes and the backslash.
fn always_hidden(c: char) -> bool {
    if matches!(c, '\'' | '"' | '\\') {
        return false;
    }
    let s: String = ['a', c].iter().collect();
    s.escape_debug().to_string() != s
}

/// Whether `c` is a combining mark (a grapheme extender Rust's `Debug` escapes on its own).
fn mark(c: char) -> bool {
    !matches!(c, '\'' | '"' | '\\') && !always_hidden(c) && c.escape_debug().nth(1).is_some()
}

/// `\u{X}` for `c`, uppercase hex.
fn escaped(c: char) -> String {
    format!("\\u{{{:X}}}", c as u32)
}

/// `s` written losslessly and visibly: `\` and `"` as `\\` and `\"`; control, format and
/// other invisible characters, a combining mark with no base (first, after a space or after
/// an escape) and the separator `·` as `\u{X}`; every other character as itself.
pub fn escape(s: &str) -> String {
    escape_with(s, &[])
}

/// [`escape`], with the characters of `extra` written as `\u{X}` too.
fn escape_with(s: &str, extra: &[char]) -> String {
    let mut out = String::new();
    let mut based = false;
    for c in s.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
            out.push(c);
            based = false;
        } else if always_hidden(c) || (mark(c) && !based) || c == '·' || extra.contains(&c) {
            out.push_str(&escaped(c));
            based = false;
        } else {
            out.push(c);
            based = !c.is_whitespace();
        }
    }
    out
}

/// The inverse of [`escape`]; `None` for a backslash not starting `\\`, `\"` or `\u{X}`.
pub fn unescape(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next()? {
            e @ ('\\' | '"') => out.push(e),
            'u' => {
                if it.next()? != '{' {
                    return None;
                }
                let hex: String = it.by_ref().take_while(|c| *c != '}').collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// A thread name as a card writes it: [`escape`], with spaces, `:`, `(`, `)` and `→` escaped
/// too, so it reads back unambiguously (twr's names hold none of them).
fn name(s: &str) -> String {
    escape_with(s, &[' ', ':', '(', ')', '→'])
}

/// Whether a key part is written without quotes.
fn bare(part: &str) -> bool {
    !part.is_empty()
        && part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A config key as a card writes it: its parts joined with `.`, each bare or quoted.
pub fn key_text(key: &[String]) -> String {
    key.iter()
        .map(|p| {
            if bare(p) {
                p.clone()
            } else {
                format!("\"{}\"", escape(p))
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// A config value as a card writes it: a string quoted; a literal with spaces, quotes and
/// parentheses escaped (and `unset`, which says the key had no value, with its first letter).
fn value_text(v: &CardValue) -> String {
    match v {
        CardValue::Str(s) => format!("\"{}\"", escape(s)),
        CardValue::Literal(l) => {
            let t = escape_with(l, &[' ', '(', ')']);
            match t.strip_prefix('u') {
                Some(rest) if t == "unset" => format!("{}{rest}", escaped('u')),
                _ => t,
            }
        }
    }
}

/// A config value read back at the start of `s` ([`value_text`]): the value and the rest.
fn value_at(s: &str) -> Option<(CardValue, &str)> {
    if let Some(q) = s.strip_prefix('"') {
        let (v, rest) = quoted(q)?;
        return Some((CardValue::Str(v), rest));
    }
    let (token, rest) = s.split_at(s.find(' ').unwrap_or(s.len()));
    if token.is_empty() {
        return None;
    }
    Some((CardValue::Literal(unescape(token)?), rest))
}

/// A config key read back at the start of `s` ([`key_text`]), up to ` = `: its parts and what
/// follows ` = `.
fn key_at(s: &str) -> Option<(Vec<String>, &str)> {
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        let (part, after) = if let Some(q) = rest.strip_prefix('"') {
            let (p, after) = quoted(q)?;
            if bare(&p) {
                // A bare part is never quoted: one reading only.
                return None;
            }
            (p, after)
        } else {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .unwrap_or(rest.len());
            if end == 0 {
                return None;
            }
            (rest[..end].to_string(), &rest[end..])
        };
        parts.push(part);
        if let Some(after) = after.strip_prefix('.') {
            rest = after;
        } else {
            return Some((parts, after.strip_prefix(" = ")?));
        }
    }
}

/// One action as a card writes it.
pub fn line(l: &CardLine) -> String {
    match l {
        CardLine::Answer {
            thread,
            answer,
            item,
        } => {
            let what = if matches!(answer.as_str(), "allow" | "deny") {
                format!("{answer} {}", name(thread))
            } else {
                format!("answer {}: \"{}\"", name(thread), escape(answer))
            };
            match item {
                Some(i) => format!("{what} ({})", escape(i)),
                None => what,
            }
        }
        CardLine::Mode {
            thread,
            mode,
            custom: true,
        } => format!("set {} → custom mode \"{}\"", name(thread), escape(mode)),
        CardLine::Mode {
            thread,
            mode,
            custom: false,
        } => format!("set {} → {}", name(thread), name(mode)),
        CardLine::Move {
            thread,
            runtime,
            model,
            effort,
        } => {
            let mut s = format!(
                "move {} → {}",
                name(thread),
                escape_with(runtime, &[' ', ':', '(', ')', '→', ','])
            );
            for (what, v) in [("model", model), ("effort", effort)] {
                if let Some(v) = v {
                    s.push_str(&format!(", {what} \"{}\"", escape(v)));
                }
            }
            s
        }
        CardLine::Config { key, value, before } => format!(
            "config {} = {} (was {})",
            key_text(key),
            value_text(value),
            before.as_ref().map_or("unset".to_string(), value_text)
        ),
    }
}

/// A confirm card's summary for `lines`.
pub fn render(lines: &[CardLine]) -> String {
    format!(
        "{HEAD}{}",
        lines.iter().map(line).collect::<Vec<_>>().join(SEP)
    )
}

/// Reads a quoted string at the start of `s` (after its opening `"`): its text unescaped and
/// what follows the closing `"`.
fn quoted(s: &str) -> Option<(String, &str)> {
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some((unescape(&s[..i])?, &s[i + 1..])),
            _ => {}
        }
    }
    None
}

/// An item summary written after an answer: nothing, or ` (<summary>)`.
fn item(rest: &str) -> Option<Option<String>> {
    if rest.is_empty() {
        return Some(None);
    }
    let inner = rest.strip_prefix(" (")?.strip_suffix(')')?;
    Some(Some(unescape(inner)?))
}

/// One action read back from a card ([`line`]).
pub fn parse_line(s: &str) -> Option<CardLine> {
    for bare in ["allow", "deny"] {
        if let Some(rest) = s.strip_prefix(bare).and_then(|r| r.strip_prefix(' ')) {
            let (thread, rest) = rest.split_at(rest.find(' ').unwrap_or(rest.len()));
            return Some(CardLine::Answer {
                thread: unescape(thread)?,
                answer: bare.to_string(),
                item: item(rest)?,
            });
        }
    }
    if let Some(rest) = s.strip_prefix("answer ") {
        let (thread, rest) = rest.split_once(": \"")?;
        let (answer, rest) = quoted(rest)?;
        if matches!(answer.as_str(), "allow" | "deny") {
            return None;
        }
        return Some(CardLine::Answer {
            thread: unescape(thread)?,
            answer,
            item: item(rest)?,
        });
    }
    if let Some(rest) = s.strip_prefix("config ") {
        let (key, rest) = key_at(rest)?;
        let (value, rest) = value_at(rest)?;
        let was = rest.strip_prefix(" (was ")?.strip_suffix(')')?;
        let before = if was == "unset" {
            None
        } else {
            match value_at(was)? {
                (v, "") => Some(v),
                _ => return None,
            }
        };
        return Some(CardLine::Config { key, value, before });
    }
    if let Some(rest) = s.strip_prefix("move ") {
        let (thread, rest) = rest.split_once(" → ")?;
        let (runtime, mut rest) = rest.split_at(rest.find(", ").unwrap_or(rest.len()));
        let mut setting = |what: &str| -> Option<Option<String>> {
            let Some(q) = rest.strip_prefix(&format!(", {what} \"")) else {
                return Some(None);
            };
            let (v, after) = quoted(q)?;
            rest = after;
            Some(Some(v))
        };
        let model = setting("model")?;
        let effort = setting("effort")?;
        if !rest.is_empty() || runtime.is_empty() {
            return None;
        }
        return Some(CardLine::Move {
            thread: unescape(thread)?,
            runtime: unescape(runtime)?,
            model,
            effort,
        });
    }
    let rest = s.strip_prefix("set ")?;
    let (thread, mode) = rest.split_once(" → ")?;
    let thread = unescape(thread)?;
    if let Some(q) = mode.strip_prefix("custom mode \"") {
        let (mode, rest) = quoted(q)?;
        return rest.is_empty().then_some(CardLine::Mode {
            thread,
            mode,
            custom: true,
        });
    }
    if mode.contains(' ') {
        return None;
    }
    Some(CardLine::Mode {
        thread,
        mode: unescape(mode)?,
        custom: false,
    })
}

/// The actions a card's summary lists ([`render`]), or `None` when it is no such summary.
pub fn parse(summary: &str) -> Option<Vec<CardLine>> {
    summary
        .strip_prefix(HEAD)?
        .split(SEP)
        .map(parse_line)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(thread: &str, mode: &str, custom: bool) -> CardLine {
        CardLine::Mode {
            thread: thread.into(),
            mode: mode.into(),
            custom,
        }
    }

    fn answer(thread: &str, answer: &str, item: Option<&str>) -> CardLine {
        CardLine::Answer {
            thread: thread.into(),
            answer: answer.into(),
            item: item.map(str::to_string),
        }
    }

    #[test]
    fn invisible_characters_show_as_escapes() {
        assert_eq!(escape("plan\u{200B}"), "plan\\u{200B}");
        assert_eq!(escape("a\u{202E}b"), "a\\u{202E}b");
        assert_eq!(escape("x\u{7}\n\t"), "x\\u{7}\\u{A}\\u{9}");
        assert_eq!(escape("\u{301}e"), "\\u{301}e");
        assert_eq!(escape("e\u{301}"), "e\u{301}");
        assert_eq!(escape("a \u{301}"), "a \\u{301}");
        assert_eq!(escape("no\u{A0}break"), "no\\u{A0}break");
        assert_eq!(escape("say \"hi\" \\ ok"), "say \\\"hi\\\" \\\\ ok");
        assert_eq!(escape("a · b"), "a \\u{B7} b");
        assert_eq!(escape("Use SQLite 👍"), "Use SQLite 👍");
        for s in [
            "plan\u{200B}",
            "a\u{202E}b",
            "x\u{7}\n\t",
            "\u{301}e",
            "say \"hi\" \\ ok",
            "a · b",
            "\u{FEFF}\u{2060}\u{AD}",
            "\\u{200B}",
        ] {
            assert_eq!(unescape(&escape(s)).as_deref(), Some(s), "{s:?}");
        }
        assert_eq!(unescape("\\x"), None);
        assert_eq!(unescape("\\u{110000}"), None);
    }

    #[test]
    fn a_card_reads_back_as_its_actions() {
        let lines = vec![
            answer("api", "allow", Some("Bash: git fetch (origin) · x")),
            answer("web", "deny", None),
            answer("db", "Use \"SQLite\" \u{200B}", Some("Which store?")),
            answer("q", "allow", None),
            mode("gem", "plan", false),
            mode("gem", "plan\u{200B}", true),
            mode("w eb", "p'lan → x", true),
            mode("a:b", "acceptEdits", false),
            CardLine::Move {
                thread: "api".into(),
                runtime: "codex".into(),
                model: None,
                effort: None,
            },
            CardLine::Move {
                thread: "w eb".into(),
                runtime: "my preset, x".into(),
                model: Some("gpt \"5\", x".into()),
                effort: Some("high".into()),
            },
            CardLine::Move {
                thread: "db".into(),
                runtime: "claude-code".into(),
                model: None,
                effort: Some("low".into()),
            },
        ];
        let s = render(&lines);
        assert!(
            s.contains("set gem → plan · set gem → custom mode \"plan\\u{200B}\""),
            "{s}"
        );
        assert!(
            !s.chars().any(|c| always_hidden(c) || c == '\u{200B}'),
            "{s}"
        );
        assert_eq!(parse(&s), Some(lines));
        // The distinct id never renders like the canonical one.
        assert_ne!(
            line(&mode("gem", "plan\u{200B}", true)),
            line(&mode("gem", "plan", false))
        );
        assert_ne!(
            line(&mode("gem", "plan\u{200B}", false)),
            line(&mode("gem", "plan", false))
        );
        assert_eq!(parse("From your message: allow"), None);
        assert_eq!(parse("allow api"), None);
    }

    fn config(key: &[&str], value: CardValue, before: Option<CardValue>) -> CardLine {
        CardLine::Config {
            key: key.iter().map(|k| k.to_string()).collect(),
            value,
            before,
        }
    }

    fn lit(s: &str) -> CardValue {
        CardValue::Literal(s.into())
    }

    fn st(s: &str) -> CardValue {
        CardValue::Str(s.into())
    }

    #[test]
    fn config_lines_read_back() {
        assert_eq!(
            line(&config(
                &["tui", "suggest"],
                lit("false"),
                Some(lit("true"))
            )),
            "config tui.suggest = false (was true)"
        );
        assert_eq!(
            line(&config(&["threads", "default_mode"], st("plan"), None)),
            "config threads.default_mode = \"plan\" (was unset)"
        );
        assert_eq!(
            line(&config(
                &["projects", "my.api", "codex_auto"],
                st("ask"),
                Some(st("never"))
            )),
            "config projects.\"my.api\".codex_auto = \"ask\" (was \"never\")"
        );
        let lines = vec![
            config(&["tui", "suggest"], lit("false"), Some(lit("true"))),
            config(&["speech", "volume"], lit("0.8"), Some(lit("1.0"))),
            config(&["a b", "x = y", "\"q\".z"], st("say \"hi\" · (x)"), None),
            config(&["k"], lit("unset"), Some(lit("unset"))),
            config(&["k"], st("unset"), Some(st(""))),
            config(&["k"], lit("a (was b)"), Some(lit("x\u{200B} y"))),
            config(&["\u{202E}", ""], lit("·"), Some(st("\\"))),
            mode("config", "plan", false),
        ];
        let s = render(&lines);
        assert!(!s.chars().any(always_hidden), "{s}");
        assert_eq!(parse(&s), Some(lines));
        // `unset` the literal never reads as no value.
        assert_ne!(
            line(&config(&["k"], lit("v"), Some(lit("unset")))),
            line(&config(&["k"], lit("v"), None))
        );
        for bad in [
            "config tui.suggest = false",
            "config = false (was true)",
            "config tui. = false (was true)",
            "config \"tui\".suggest = false (was true)",
            "config tui.suggest = false (was true) x",
            "config tui.suggest = false (was a b)",
        ] {
            assert_eq!(parse_line(bad), None, "{bad}");
        }
    }
}

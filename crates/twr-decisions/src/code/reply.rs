//! Reply lexicon: recognizing short confirmations ("Yes!", "go ahead.", "LGTM", "2").
//!
//! Two readings:
//!
//! - [`strict`] / [`strict_phrase`]: the strict whole-message grammar. The only reading by
//!   which `say` routing answers a permission or question for the user (see
//!   [`super::route::explicit_answer`]; the orchestrator's relays are read by
//!   [`super::command`]).
//! - [`classify`] and [`mentions_answer`]: loose readings that only decide where a message
//!   goes (a reply-shaped message goes to the focused thread) or that it looks like an attempt
//!   to answer (so routing abstains instead of sending it as text). They never answer an item.

/// Longest message (in whitespace-separated words) that can be a reply.
pub const MAX_REPLY_WORDS: usize = 6;

/// Phrases that approve / confirm (lowercase, punctuation stripped).
const ALLOW: &[&str] = &[
    "yes",
    "y",
    "yeah",
    "yep",
    "yup",
    "sure",
    "ok",
    "okay",
    "approve",
    "approved",
    "allow",
    "go ahead",
    "continue",
    "proceed",
    "do it",
    "sounds good",
    "looks good",
    "lgtm",
    "ship it",
];

/// Phrases that refuse.
const DENY: &[&str] = &[
    "no", "n", "nope", "nah", "deny", "denied", "reject", "rejected", "decline", "don't", "do not",
];

/// Words that may accompany a reply without changing it.
const FILLER: &[&str] = &[
    "please",
    "pls",
    "thanks",
    "thx",
    "thank you",
    "option",
    "it",
    "that",
    "this",
];

/// Strict answer phrases that confirm (lowercase, whitespace-normalized). The whole message
/// (after courtesy) must be one of these.
pub const STRICT_ALLOW: &[&str] = &[
    "yes",
    "y",
    "yeah",
    "yep",
    "ok",
    "okay",
    "sure",
    "approve",
    "approved",
    "allow",
    "go ahead",
    "do it",
    "lgtm",
    "ship it",
    "approve it",
    "allow it",
];

/// Strict answer phrases that refuse.
pub const STRICT_DENY: &[&str] = &[
    "no",
    "n",
    "nope",
    "deny",
    "denied",
    "reject",
    "don't",
    "stop",
    "deny it",
    "reject it",
];

/// Courtesy that may trail a strict answer ("yes please", "ok, thanks!", "no thank you").
pub const COURTESY: &[&str] = &["please", "thanks", "thank you", "thx", "ty"];

/// A message read with the strict grammar ([`strict`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Strict {
    /// The whole message without one terminal `.`/`!`, lowercase, whitespace collapsed, curly
    /// apostrophes straightened. Compared with option labels typed in full ("yes, proceed").
    pub whole: String,
    /// `whole` without trailing courtesy ([`COURTESY`], optionally after a comma), or `None`
    /// when `whole` has a `?`, a quote, or punctuation other than an apostrophe inside a word
    /// or a leading `#`: then only a full option label can match.
    pub core: Option<String>,
}

/// Lowercase, whitespace collapsed, curly apostrophes straightened.
pub fn fold(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(['\u{2019}', '\u{2018}'], "'")
        .to_lowercase()
}

/// Reads `message` with the strict grammar: `None` for an empty or multi-line message;
/// otherwise [`Strict`]. One terminal `.` or `!` is dropped (a second one stays and fails).
pub fn strict(message: &str) -> Option<Strict> {
    let m = message.trim();
    if m.is_empty() || m.contains(['\n', '\r']) {
        return None;
    }
    let m = m.strip_suffix(['.', '!']).map_or(m, str::trim_end);
    let whole = fold(m);
    if whole.is_empty() {
        return None;
    }
    let core = strip_courtesy(&whole);
    let clean = !core.is_empty()
        && core.char_indices().all(|(i, c)| {
            c.is_alphanumeric()
                || c == ' '
                || (c == '#' && i == 0)
                || (c == '\'' && {
                    let before = core[..i].chars().next_back();
                    let after = core[i + 1..].chars().next();
                    before.is_some_and(char::is_alphanumeric)
                        && after.is_some_and(char::is_alphanumeric)
                })
        });
    Some(Strict {
        core: clean.then(|| core.to_string()),
        whole,
    })
}

/// `s` (folded) without one trailing courtesy word, which must follow whitespace or a comma.
fn strip_courtesy(s: &str) -> &str {
    for c in COURTESY {
        let Some(rest) = s.strip_suffix(c) else {
            continue;
        };
        if !rest.ends_with([' ', ',']) {
            continue;
        }
        let rest = rest.trim_end();
        let rest = rest.strip_suffix(',').unwrap_or(rest).trim_end();
        if !rest.is_empty() {
            return rest;
        }
    }
    s
}

/// The strict answer a [`Strict::core`] is: exactly one phrase of [`STRICT_ALLOW`] /
/// [`STRICT_DENY`], or an option number `N` / `#N` / `option N` ([`Reply::Number`], 1-based,
/// 0 kept so callers can reject it). Never [`Reply::Mixed`].
pub fn strict_phrase(core: &str) -> Option<Reply> {
    if STRICT_ALLOW.contains(&core) {
        return Some(Reply::Allow);
    }
    if STRICT_DENY.contains(&core) {
        return Some(Reply::Deny);
    }
    let digits = core
        .strip_prefix('#')
        .or_else(|| core.strip_prefix("option "))
        .unwrap_or(core);
    if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
        return digits.parse().ok().map(Reply::Number);
    }
    None
}

/// Whether `message` mentions an answer anywhere: an answer word or phrase (loose lexicon or
/// strict), or a number. Used to tell an attempt at answering ("yes?", "if tests pass,
/// approve") from an instruction; never to answer.
pub fn mentions_answer(message: &str) -> bool {
    let norm = normalize(message);
    let words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    let lexicon = |p: &str| {
        ALLOW.contains(&p)
            || DENY.contains(&p)
            || STRICT_ALLOW.contains(&p)
            || STRICT_DENY.contains(&p)
    };
    words
        .iter()
        .any(|w| lexicon(w) || w.chars().all(|c| c.is_ascii_digit()))
        || words.windows(2).any(|p| lexicon(&p.join(" ")))
}

/// A recognized reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    /// Confirmation (yes / ok / approve / go ahead / lgtm ...).
    Allow,
    /// Refusal (no / deny / reject ...).
    Deny,
    /// A number as typed (1-based); 0 is kept so callers can reject it.
    Number(usize),
    /// Only lexicon words, but conflicting ("yes no", "1 yes").
    Mixed,
}

/// Lowercases, maps curly apostrophes to `'`, turns every other non-alphanumeric character
/// into a space and collapses whitespace.
pub fn normalize(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\'' => '\'',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    mapped
        .split_whitespace()
        .map(|w| w.trim_matches('\'').to_lowercase())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Classifies `message` as a reply: at most [`MAX_REPLY_WORDS`] words, made only of lexicon
/// phrases, fillers and at most one number, with at least one non-filler. Case and
/// punctuation are ignored.
pub fn classify(message: &str) -> Option<Reply> {
    if message.split_whitespace().count() > MAX_REPLY_WORDS {
        return None;
    }
    let norm = normalize(message);
    let words: Vec<&str> = norm.split(' ').filter(|w| !w.is_empty()).collect();
    let (mut allow, mut deny, mut numbers) = (0, 0, Vec::new());
    let mut i = 0;
    'outer: while i < words.len() {
        let w = words[i];
        if !w.is_empty() && w.chars().all(|c| c.is_ascii_digit()) {
            numbers.push(w.parse::<usize>().ok()?);
            i += 1;
            continue;
        }
        // Longest phrase first (all phrases are at most two words).
        for len in [2, 1] {
            if i + len > words.len() {
                continue;
            }
            let phrase = words[i..i + len].join(" ");
            let hit = if ALLOW.contains(&phrase.as_str()) {
                allow += 1;
                true
            } else if DENY.contains(&phrase.as_str()) {
                deny += 1;
                true
            } else {
                FILLER.contains(&phrase.as_str())
            };
            if hit {
                i += len;
                continue 'outer;
            }
        }
        return None;
    }
    match (allow, deny, numbers.as_slice()) {
        (0, 0, []) => None,
        (_, 0, []) => Some(Reply::Allow),
        (0, _, []) => Some(Reply::Deny),
        (0, 0, [n]) => Some(Reply::Number(*n)),
        _ => Some(Reply::Mixed),
    }
}

/// Whether a (normalized) option label reads as a confirmation or refusal on its own, e.g. a
/// question option "Yes, proceed" is [`Reply::Allow`].
pub fn label_polarity(label: &str) -> Option<Reply> {
    match classify(label)? {
        r @ (Reply::Allow | Reply::Deny) => Some(r),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexicon_with_case_and_punctuation() {
        for m in [
            "Yes!",
            "yes",
            "Y",
            "go ahead.",
            "Go ahead!!",
            "LGTM",
            "ok",
            "Okay.",
            "sounds good",
            "ship it 🚀",
            "Do it",
            "continue",
            "approve",
            "yes, go ahead",
            "yes please",
            "ok thanks!",
            "  yes  ",
            "approve it",
            "allow that",
        ] {
            assert_eq!(classify(m), Some(Reply::Allow), "{m}");
        }
        for m in [
            "no",
            "No.",
            "nope",
            "deny",
            "Reject!",
            "don't",
            "don\u{2019}t",
            "no thanks",
            "deny it",
        ] {
            assert_eq!(classify(m), Some(Reply::Deny), "{m}");
        }
    }

    #[test]
    fn numbers() {
        assert_eq!(classify("1"), Some(Reply::Number(1)));
        assert_eq!(classify("2."), Some(Reply::Number(2)));
        assert_eq!(classify("#3"), Some(Reply::Number(3)));
        assert_eq!(classify("option 2"), Some(Reply::Number(2)));
        assert_eq!(classify("12"), Some(Reply::Number(12)));
        assert_eq!(classify("0"), Some(Reply::Number(0)));
        assert_eq!(classify("1 2"), Some(Reply::Mixed));
        assert_eq!(classify("yes 1"), Some(Reply::Mixed));
        assert_eq!(classify("99999999999999999999999999"), None);
    }

    #[test]
    fn not_replies() {
        // Seven words, all lexicon: too long.
        assert_eq!(classify("yes yes yes yes yes yes yes"), None);
        assert_eq!(classify("yes yes yes yes yes yes"), Some(Reply::Allow));
        // A seven-word sentence.
        assert_eq!(classify("yes go ahead and do it now"), None);
        assert_eq!(classify("no, use postgres instead"), None);
        assert_eq!(classify("ok so what's next"), None);
        assert_eq!(classify("fix the login bug"), None);
        assert_eq!(classify(""), None);
        assert_eq!(classify("!!!"), None);
        assert_eq!(classify("please"), None);
        assert_eq!(classify("thanks"), None);
        assert_eq!(classify("it"), None);
        assert_eq!(classify("yes no"), Some(Reply::Mixed));
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize("  Go   AHEAD. "), "go ahead");
        assert_eq!(normalize("'yes'"), "yes");
        assert_eq!(normalize("Don\u{2019}t!"), "don't");
        assert_eq!(normalize("Yes, proceed"), "yes proceed");
    }

    #[test]
    fn strict_reading() {
        let core = |m: &str| strict(m).and_then(|s| s.core);
        assert_eq!(core("Yes!"), Some("yes".into()));
        assert_eq!(core("  ok,  thanks! "), Some("ok".into()));
        assert_eq!(core("no thank you."), Some("no".into()));
        assert_eq!(core("Don\u{2019}t"), Some("don't".into()));
        assert_eq!(core("#2"), Some("#2".into()));
        // Courtesy needs a word boundary and something before it.
        assert_eq!(core("pretty"), Some("pretty".into()));
        assert_eq!(core("thanks"), Some("thanks".into()));
        // Not clean: only a full option label could still match `whole`.
        for m in [
            "yes?", "'yes'", "yes, go", "yes!!", "yes. no", "a#1", "\"yes\"",
        ] {
            assert_eq!(core(m), None, "{m:?}");
        }
        assert_eq!(
            strict("yes, but only staging?").unwrap().whole,
            "yes, but only staging?"
        );
        assert_eq!(strict("yes\nno"), None);
        assert_eq!(strict(" . "), None);
        assert_eq!(strict_phrase("yes"), Some(Reply::Allow));
        assert_eq!(strict_phrase("go ahead"), Some(Reply::Allow));
        assert_eq!(strict_phrase("stop"), Some(Reply::Deny));
        assert_eq!(strict_phrase("option 2"), Some(Reply::Number(2)));
        assert_eq!(strict_phrase("#3"), Some(Reply::Number(3)));
        assert_eq!(strict_phrase("12"), Some(Reply::Number(12)));
        for m in [
            "yes no",
            "sounds good",
            "continue",
            "yes go ahead",
            "option",
            "#",
            "go",
        ] {
            assert_eq!(strict_phrase(m), None, "{m:?}");
        }
    }

    #[test]
    fn answer_mentions() {
        for m in [
            "yes?",
            "If tests pass, approve.",
            "wait, no",
            "3",
            "go ahead when ready",
        ] {
            assert!(mentions_answer(m), "{m:?}");
        }
        for m in ["fix the login bug", "why?", "what is it doing", ""] {
            assert!(!mentions_answer(m), "{m:?}");
        }
    }

    #[test]
    fn option_label_polarity() {
        assert_eq!(label_polarity("Yes"), Some(Reply::Allow));
        assert_eq!(label_polarity("Yes, proceed"), Some(Reply::Allow));
        assert_eq!(label_polarity("No"), Some(Reply::Deny));
        assert_eq!(label_polarity("Postgres"), None);
        assert_eq!(label_polarity("1"), None);
    }
}

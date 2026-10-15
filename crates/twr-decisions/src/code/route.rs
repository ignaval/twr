//! Code routing rules, in order:
//!
//! 1. `new <project>: <message>` → [`RouteOutcome::NewThreadRequest`].
//! 2. Explicit addressing → [`RouteOutcome::SendTo`] the target with the prefix stripped:
//!    `on <x>, ...` / `on <x>: ...` and `@<x> ...` resolve `<x>` like [`twr_core::reduce::Registry::resolve`]
//!    (exact id, name, alias, then unique prefix); `<x>: ...` needs an exact id, name or
//!    alias. `@` only addresses at the start of the message. An ambiguous target, or an empty
//!    message after the prefix, abstains; an unknown target is not addressing. When the target
//!    has an open permission/question, the rest of the message is read like rule 3: a strict
//!    answer answers it, an [`answer_shaped`] one abstains, anything else is sent as text.
//!
//! Invariant: [`RouteOutcome::RespondAttention`] is produced only from [`explicit_answer`],
//! the strict whole-message grammar for the user's own replies, and only for an item the user
//! had seen ([`AttentionInfo::answerable`]); a strict answer to an item they had not seen
//! abstains. (The orchestrator's relayed answers are read by [`super::command`].)
//! 3. Reply routing to the focused thread. If its top item is an open permission/question:
//!    a message that is a strict answer ([`explicit_answer`]) answers it
//!    ([`RouteOutcome::RespondAttention`]); a message that is not, but looks like an attempt at
//!    one ([`answer_shaped`]: "yes?", "if tests pass, approve"), abstains, so it is neither
//!    taken as an answer nor sent as text to a thread blocked on the item (the next rungs, the
//!    orchestrator or the user, decide; the item stays open for the user to answer); anything
//!    else is not a reply (rule 4 may still apply). Without such an item, a reply-shaped
//!    message ([`reply::classify`]) is sent to the focused thread as text.
//! 4. Single-thread routing: exactly one worker thread (not the orchestrator, not stopped) →
//!    send it the message, unless that thread has an open permission/question and the message
//!    is [`answer_shaped`] (then abstain: only rules 2 and 3 answer items).
//!
//! Anything else abstains (the orchestrator, then the user decide).

use twr_core::event::AttentionReason;

use super::command;
use super::reply::{self, Reply};
use crate::context::{AttentionInfo, RouteContext, ThreadInfo, Unresolved};
use crate::{ReplyAnswer, RouteOutcome};

/// Result of a routing rule: `None` = rule does not apply (try the next), `Some(None)` =
/// abstain from the whole route decision, `Some(Some(outcome))` = decided.
type RuleResult = Option<Option<RouteOutcome>>;

/// Applies the code routing rules. `None` = abstain.
pub fn route(ctx: &RouteContext) -> Option<RouteOutcome> {
    let msg = ctx.message.trim();
    if msg.is_empty() {
        return None;
    }
    if let Some(r) = new_thread(msg) {
        return r;
    }
    if let Some(r) = addressed(ctx, msg) {
        return r;
    }
    if let Some(r) = reply_to_focused(ctx, msg) {
        return r;
    }
    match ctx.workers().as_slice() {
        [only]
            if !only
                .blocking_attention()
                .is_some_and(|attn| answer_shaped(attn, msg)) =>
        {
            Some(RouteOutcome::SendTo {
                thread: only.id.clone(),
                message: msg.to_string(),
            })
        }
        _ => None,
    }
}

/// `keyword<whitespace>rest` (keyword case-insensitive) → rest.
fn strip_keyword<'a>(msg: &'a str, keyword: &str) -> Option<&'a str> {
    let head = msg.get(..keyword.len())?;
    let rest = &msg[keyword.len()..];
    (head.eq_ignore_ascii_case(keyword) && rest.starts_with(char::is_whitespace))
        .then(|| rest.trim_start())
}

/// Rule 1: `new <project>: <message>`.
fn new_thread(msg: &str) -> RuleResult {
    let rest = strip_keyword(msg, "new")?;
    let (project, body) = rest.split_once(':')?;
    let project = project.trim();
    if project.is_empty() || project.contains(char::is_whitespace) {
        return None;
    }
    Some(Some(RouteOutcome::NewThreadRequest {
        project: project.to_string(),
        message: body.trim().to_string(),
    }))
}

/// Rule 2: explicit addressing.
fn addressed(ctx: &RouteContext, msg: &str) -> RuleResult {
    match addressee(ctx, msg)? {
        Addressee::Ambiguous => Some(None),
        Addressee::Thread { thread, body, .. } => Some(send_addressed(thread, body)),
    }
}

/// The thread a message explicitly addresses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Addressee<'a> {
    /// That thread, with the message after the address.
    Thread {
        /// The addressed thread.
        thread: &'a ThreadInfo,
        /// The rest of the message (not trimmed).
        body: &'a str,
        /// The address as written, without `@`, `on` or the trailing `,` / `:`.
        target: &'a str,
        /// Written `@<x>`.
        at: bool,
        /// The form allows a unique prefix (`@<x>`, `on <x>,`).
        prefix: bool,
    },
    /// The address matches several threads.
    Ambiguous,
}

/// The address forms of `msg`, in the order they are tried: `(target, body, prefix, at)` where
/// `prefix` allows a unique id/name prefix as the target and `at` is the `@<x>` form.
fn address_forms(msg: &str) -> Vec<(&str, &str, bool, bool)> {
    let mut forms = Vec::new();
    // `on <x>, ...` / `on <x>: ...`
    if let Some(rest) = strip_keyword(msg, "on") {
        if let Some(i) = rest.find([',', ':']) {
            let target = rest[..i].trim();
            if !target.is_empty() {
                forms.push((target, &rest[i + 1..], true, false));
            }
        }
    }
    // `@<x> ...`
    if let Some(rest) = msg.strip_prefix('@') {
        let end = rest
            .find(|c: char| c.is_whitespace() || c == ',' || c == ':')
            .unwrap_or(rest.len());
        let target = &rest[..end];
        if !target.is_empty() {
            let body = rest[end..].trim_start();
            let body = body.strip_prefix([',', ':']).unwrap_or(body);
            forms.push((target, body, true, true));
        }
    }
    // `<x>: ...` with an exact target.
    if let Some(first) = msg.split_whitespace().next() {
        if let Some(target) = first.strip_suffix(':').filter(|t| !t.is_empty()) {
            let at = msg.find(first).unwrap_or(0) + first.len();
            forms.push((target, &msg[at..], false, false));
        }
    }
    forms
}

/// The thread `msg` explicitly addresses (`on <x>, ...`, `on <x>: ...`, `@<x> ...`, `<x>: ...`,
/// see the module docs), if any. An unknown target is not an address; the first form whose
/// target resolves (or is ambiguous) wins.
pub fn addressee<'a>(ctx: &'a RouteContext, msg: &'a str) -> Option<Addressee<'a>> {
    let msg = msg.trim();
    for (target, body, prefix, at) in address_forms(msg) {
        match ctx.resolve(target, prefix) {
            Ok(thread) => {
                return Some(Addressee::Thread {
                    thread,
                    body,
                    target,
                    at,
                    prefix,
                });
            }
            Err(Unresolved::Ambiguous) => return Some(Addressee::Ambiguous),
            Err(Unresolved::NotFound) => continue,
        }
    }
    None
}

/// Sends to an addressed thread: an empty body abstains; a strict answer to the thread's open
/// item answers it; an attempt at one that is not strict abstains.
fn send_addressed(thread: &ThreadInfo, body: &str) -> Option<RouteOutcome> {
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    if let Some(attn) = thread.blocking_attention() {
        if let Some(answer) = explicit_answer(attn, body) {
            return answerable(attn).then(|| respond(thread, attn, answer));
        }
        if answer_shaped(attn, body) {
            return None;
        }
    }
    Some(RouteOutcome::SendTo {
        thread: thread.id.clone(),
        message: body.to_string(),
    })
}

/// Rule 3: reply to the focused thread.
fn reply_to_focused(ctx: &RouteContext, msg: &str) -> RuleResult {
    let focused = ctx.focused_thread()?;
    if let Some(attn) = focused.blocking_attention() {
        if let Some(answer) = explicit_answer(attn, msg) {
            return Some(answerable(attn).then(|| respond(focused, attn, answer)));
        }
        return answer_shaped(attn, msg).then_some(None);
    }
    reply::classify(msg)?;
    Some(Some(RouteOutcome::SendTo {
        thread: focused.id.clone(),
        message: msg.to_string(),
    }))
}

/// Whether a strict answer may answer `attn` ([`AttentionInfo::answerable`]: the user had seen
/// it); when not, routing abstains (the answer is neither given nor sent as text).
fn answerable(attn: &AttentionInfo) -> bool {
    attn.answerable
}

fn respond(thread: &ThreadInfo, attn: &AttentionInfo, answer: ReplyAnswer) -> RouteOutcome {
    RouteOutcome::RespondAttention {
        thread: thread.id.clone(),
        attn_id: attn.attn_id.clone(),
        answer,
    }
}

/// Which open permission/question a message said with nothing on screen (`twr say`, no
/// `seen_gseq`) is trying to answer, when it looks like an answer at all. Such a message can
/// never answer an item, and routing it would only raise a confusing "which
/// thread?" question (or send "yes" as a prompt), so `say` refuses it and points at
/// `twr respond` instead.
///
/// - `None`: not an answer attempt (a `new <project>: ...` request, an ambiguous address, an
///   addressed thread with no blocking item, or a message that is neither a strict answer
///   ([`explicit_answer`]) nor [`answer_shaped`] for any thread's blocking item): route it.
/// - `Some(Some(thread))`: addressed to `thread` (`@api yes`), whose blocking item it answers.
/// - `Some(None)`: unaddressed, and it answers some thread's blocking item; or, whatever its
///   length, it looks like an answer to several items or to a named thread
///   ([`command::answer_attempt`]: "allow both", "@api yes, @web no", "allow api, and then
///   write a summary"), while any thread has one.
pub fn unseen_answer(ctx: &RouteContext, msg: &str) -> Option<Option<String>> {
    let msg = msg.trim();
    if msg.is_empty() || new_thread(msg).is_some() {
        return None;
    }
    let waiting = ctx.threads.iter().any(|t| t.blocking_attention().is_some());
    // Answers for several items (or next to other requests): list every pending item.
    if waiting && command::answer_attempt(ctx, msg) {
        return Some(None);
    }
    let attempts = |attn: &AttentionInfo, body: &str| {
        explicit_answer(attn, body).is_some() || answer_shaped(attn, body)
    };
    match addressee(ctx, msg) {
        Some(Addressee::Ambiguous) => None,
        Some(Addressee::Thread { thread, body, .. }) => {
            let attn = thread.blocking_attention()?;
            attempts(attn, body.trim()).then(|| Some(thread.id.clone()))
        }
        None => ctx
            .threads
            .iter()
            .filter_map(ThreadInfo::blocking_attention)
            .any(|attn| attempts(attn, msg))
            .then_some(None),
    }
}

/// Longest message (in words) that [`answer_shaped`] treats as an attempt at answering; a
/// longer one is an instruction even if it mentions "yes" or a number.
pub const ANSWER_SHAPED_MAX_WORDS: usize = 12;

/// Whether `msg`, which is not a strict answer to `attn`, still looks like an attempt at one:
/// at most [`ANSWER_SHAPED_MAX_WORDS`] words that mention an answer word or phrase, a number
/// ([`reply::mentions_answer`]) or one of `attn`'s option labels. Routing abstains on such a
/// message rather than answering or sending it as text.
pub fn answer_shaped(attn: &AttentionInfo, msg: &str) -> bool {
    let norm = reply::normalize(msg);
    let words = norm.split(' ').filter(|w| !w.is_empty()).count();
    if words == 0 || words > ANSWER_SHAPED_MAX_WORDS {
        return false;
    }
    if reply::mentions_answer(msg) {
        return true;
    }
    let padded = format!(" {norm} ");
    attn.options.iter().any(|o| {
        let label = reply::normalize(o);
        !label.is_empty() && padded.contains(&format!(" {label} "))
    })
}

/// The one explicit answer `message` gives to the open permission/question `attn`, if any:
/// the strict whole-message grammar. It is the only way `say` routing answers an item on the
/// user's behalf ([`RouteOutcome::RespondAttention`]); the orchestrator's relays are read by
/// [`super::command`] instead. It errs towards `None`: the user is then asked.
///
/// `message` must already be without its address: callers strip `@x` / `on x,` / `x:` with
/// [`addressee`], which resolves the target against the registry; an unresolved address stays
/// in the message and makes it no answer.
///
/// Grammar ([`reply::strict`]): the message is one line; one terminal `.` or `!` is dropped;
/// then
///
/// 1. the whole message equals an option label (case-insensitive, whitespace-normalized) →
///    that label, as listed;
/// 2. otherwise, after dropping trailing courtesy (`please`, `thanks`, `thank you`, `thx`,
///    `ty`, optionally after a comma), what is left is exactly one of:
///    - an option label → that label;
///    - [`reply::STRICT_ALLOW`] (`yes`, `ok`, `approve`, `go ahead`, `lgtm`, ...) /
///      [`reply::STRICT_DENY`] (`no`, `deny`, `reject`, `don't`, `stop`, ...): on a
///      permission, `allow` / `deny` (if the item lists options, they must include it); on a
///      question, the single option whose label reads that way ("Yes, proceed" / "No");
///    - `N`, `#N` or `option N` → [`ReplyAnswer::OptionIndex`] `N - 1` when `1 <= N <=`
///      the number of options.
///
/// Anything else gives `None`: a `?` anywhere (unless it is part of an option label typed in
/// full), quotes, other punctuation, a second sentence, extra words (conditions and hedges
/// like "if", "once", "but", "only", "wait", "maybe"), several answers.
pub fn explicit_answer(attn: &AttentionInfo, message: &str) -> Option<ReplyAnswer> {
    let s = reply::strict(message)?;
    let by_label = |text: &str| {
        let mut hits = attn.options.iter().filter(|o| reply::fold(o) == text);
        match (hits.next(), hits.next()) {
            (Some(o), None) => Some(ReplyAnswer::Label(o.clone())),
            _ => None,
        }
    };
    if let Some(answer) = by_label(&s.whole) {
        return Some(answer);
    }
    let core = s.core?;
    if let Some(answer) = by_label(&core) {
        return Some(answer);
    }
    let kind = reply::strict_phrase(&core)?;
    match kind {
        Reply::Mixed => None,
        Reply::Number(n) => {
            (n >= 1 && n <= attn.options.len()).then(|| ReplyAnswer::OptionIndex(n - 1))
        }
        Reply::Allow | Reply::Deny => match attn.reason {
            AttentionReason::Permission => {
                let label = if kind == Reply::Allow {
                    "allow"
                } else {
                    "deny"
                };
                (attn.options.is_empty() || attn.options.iter().any(|o| o == label))
                    .then(|| ReplyAnswer::Label(label.to_string()))
            }
            _ => {
                let mut matching = attn
                    .options
                    .iter()
                    .filter(|o| reply::label_polarity(o) == Some(kind));
                match (matching.next(), matching.next()) {
                    (Some(o), None) => Some(ReplyAnswer::Label(o.clone())),
                    _ => None,
                }
            }
        },
    }
}

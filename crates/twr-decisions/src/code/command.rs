//! Short commands: whether the user's message to the orchestrator is, as a whole, one short
//! command asking for the answer or the mode change the orchestrator relays.
//!
//! The orchestrator, an LLM, reads the user's message freely. When it calls
//! `respond_attention` or `set_mode`, the hub (`twr-hub` `hub::orch`, after its provenance
//! checks) reads that message itself and applies the call directly only when the WHOLE
//! message is a command of the tiny grammar below that covers the call. Everything else that
//! passes provenance becomes a proposal on the confirm card, which costs the user one key and
//! is always safe. So the grammar is kept small enough to enumerate: anything outside it, a
//! second sentence, a question, courtesy, an option label or number, a noun, a typo of a name
//! included, is no command, and the call goes on the card.
//!
//! # The message
//!
//! - It is trimmed, and may end with one `.` or `!`, which is dropped.
//! - Any other `.`, `!`, `?`, `¿`, `¡`, `;`, `:`, `…` (full-width forms too), a line break or a
//!   control character anywhere makes it no command: a command is exactly one sentence.
//! - At most [`MAX_MESSAGE_BYTES`] bytes and [`MAX_TOKENS`] tokens.
//! - It is lowercased with curly quotes and apostrophes straightened ([`normalize`]) and split
//!   at whitespace; each `,` is a token of its own. Nothing else is trimmed: "'api'", "(api)"
//!   or "*api*" is no vocabulary. Every token must be consumed by the grammar.
//!
//! # Grammar (`[x]` optional, `|` alternatives, `*` repetition)
//!
//! ```text
//! MESSAGE := CLAUSE (JOIN CLAUSE)*                  at most MAX_CLAUSES clauses
//! JOIN    := , | and | y | , and | , y
//! CLAUSE  := ANSWER | MODE
//!
//! ANSWER  := AW [PREP] TARGETS
//!          | AW | AW it                             only as the whole message
//! PREP    := on | for | to | a
//!
//! MODE    := [VERB] TARGETS to [the] MODENAME (mode | modo)
//!          | [VERB] TARGETS to [the] STRONG
//!          | VERB_ES TARGETS (a | en) (modo MODENAME | MODENAME (mode | modo) | STRONG)
//! VERB    := set | switch | change | put | turn
//! VERB_ES := pon | pasa | cambia
//!
//! TARGETS := GROUP (TJOIN GROUP)*                   at most MAX_GROUPS groups
//! TJOIN   := , | and | y | & | , and | , y | , &
//! GROUP   := [the | el | la] REF [one]
//!          | both | ambos | ambas | them both       exactly two candidates
//!          | all | todos | todas | every | everything | them all
//! ```
//!
//! - AW is one word or phrase of [`APPROVE`] or [`DENY`]. "no" and "nope" ([`LONE_ONLY`])
//!   answer only as the whole message. The unaccented "si" is no answer word (it is also "if").
//! - MODENAME is a mode of [`MODES`], its words apart ("read only", "don't ask") or joined by
//!   nothing, `-` or `_` ("read-only", "dontAsk"), or a one-edit typo of a mode of 4+ letters
//!   directly before "mode" / "modo" ("audo mode"; never a word of [`NOT_TYPOS`]). STRONG is a
//!   mode spelled exactly that is not weak: a weak mode ([`WEAK_MODES`]: auto, plan, manual,
//!   default) always needs "mode" / "modo" next to it.
//! - REF is a thread reference ([`Names::reference`]): a token spelled as a plain identifier
//!   ([`plain_identifier`]), or `@` and one, that is exactly a thread's name, alias or id (any
//!   case), or exactly a name it had before a rename (`former_names`), or a whole `-` / `_`
//!   part of 3+ characters of its name ("webapp" for `orbit-webapp-1`). No typos. A word two
//!   threads match in any of these ways is no reference: `api` beside threads `api` and
//!   `x-api-y`, or `API` beside a thread once named `api`, names neither. Nor is a word an
//!   orchestrator thread (current or retired) or the word "orchestrator" matches in any of
//!   these ways. A bare word of [`STOPLIST`], [`RESERVED`], [`NOT_NAMES`], [`COURTESY`],
//!   [`QUALIFIERS`] or the vocabulary, or holding one as a `-` / `_` / `.` part ("no-one",
//!   "never-1", "thank-you", "either-1"), or made only of digits ("2", "1-2"), is never a
//!   reference: such a thread is named only as `@x` ("allow api, cheers" never approves a
//!   worker named `cheers`, nor "allow api, conditionally" one named `conditionally-1`).
//!
//! # Meaning
//!
//! Answer candidates are the items waiting and seen when the message arrived, no
//! orchestrator's item and no confirm card. Mode candidates are the workers then: not stopped,
//! not an orchestrator, not the hub thread. Every group must pick something out, else the whole
//! message is no command:
//!
//! - for an answer, a REF covers its thread's waiting item when it has exactly one (a thread
//!   with none or several is no command); for a mode, a REF covers its worker, even stopped;
//! - "both" (ambos, ambas, them both) covers every candidate when there are exactly two;
//! - "all" (todos, todas, every, everything, them all) covers every candidate, at least one;
//! - a lone AW (or "AW it") covers the only answer candidate when there is exactly one.
//!
//! A message that parses two ways is no command (no real wording does).
//!
//! A call is accepted directly ([`Coverage::Direct`]) when an action of the command covers its
//! target and every action covering it gives the same answer or mode, and the command is not a
//! lone answer. A lone answer ("yes", "ok", "allow it") names nothing: in the orchestrator's
//! conversation the user may be answering the orchestrator's own question, so it is never
//! applied directly ([`Coverage::Lone`]: the call goes on the confirm card, which names the
//! item, and [`direct_actions`] gives nothing). An answer is given only
//! to a permission: approve picks its allow option, deny its deny option (a label spelled as
//! the answer word, else the only option of that polarity); a permission listing no options
//! takes `allow` / `deny`. A question is never answered directly (its options are labels), and
//! a deny-only permission is never allowed: such a covered call is a [`Coverage::Conflict`]. A
//! call's mode is the hub's canonical form for the thread's runtime, compared exactly with the
//! mode the command names ([`mode_coverage`]).
//!
//! [`answer_attempt`] is a separate, loose reading for `twr say` without `seen_gseq`: it
//! only tells that a message tries to answer items, so `say` lists them instead of routing it.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use twr_core::event::{AttentionReason, HUB_THREAD};

use super::reply::{self, Reply};
use crate::context::{RouteContext, ThreadInfo};

/// Words and phrases that approve.
pub const APPROVE: &[&str] = &[
    "allow", "approve", "accept", "yes", "ok", "okay", "sure", "go ahead", "proceed", "confirm",
    "lgtm", "sí", "dale", "adelante", "aprueba", "apruebo", "permite", "acepta", "yep", "yeah",
    "yup",
];

/// Words that refuse.
pub const DENY: &[&str] = &[
    "deny", "reject", "decline", "no", "nope", "block", "rechaza", "deniega",
];

/// Refusal words that answer only as the whole message ("no block api" is no command).
pub const LONE_ONLY: &[&str] = &["no", "nope"];

/// Prepositions before an answer's targets ("allow on the webapp", "dale a ambos").
pub const ANSWER_PREPOSITIONS: &[&str] = &["on", "for", "to", "a"];

/// The English verbs of a mode change.
pub const VERBS: &[&str] = &["set", "switch", "change", "put", "turn"];

/// The Spanish verbs of a mode change.
pub const SPANISH_VERBS: &[&str] = &["pon", "pasa", "cambia"];

/// The Spanish prepositions before a mode.
pub const SPANISH_PREPOSITIONS: &[&str] = &["a", "en"];

/// Words for "mode".
pub const MODE_WORDS: &[&str] = &["mode", "modo"];

/// Permission modes: `(canonical id, its words)`. A mode is written as its words apart
/// ("read only", "don't ask"), or as one word joined by nothing, `-` or `_` ("read-only",
/// "dontAsk", "accept_edits"), any case.
pub const MODES: &[(&str, &[&str])] = &[
    ("auto", &["auto"]),
    ("plan", &["plan"]),
    ("manual", &["manual"]),
    ("default", &["default"]),
    ("read-only", &["read", "only"]),
    ("dontAsk", &["don't", "ask"]),
    ("dontAsk", &["dont", "ask"]),
    ("acceptEdits", &["accept", "edits"]),
    ("bypassPermissions", &["bypass", "permissions"]),
    ("bypassPermissions", &["bypass"]),
];

/// The weak modes: ordinary words too ("make a plan"), so only next to "mode" / "modo".
pub const WEAK_MODES: &[&str] = &["auto", "plan", "manual", "default"];

/// Real words one edit away from a mode word: never read as its typo ("auth mode").
pub const NOT_TYPOS: &[&str] = &[
    "alto", "auth", "autos", "play", "plat", "plain", "plane", "plant", "plans", "clan", "flan",
];

/// Quantifiers meaning exactly two candidates.
pub const BOTH: &[&str] = &["both", "ambos", "ambas"];

/// Quantifiers meaning every candidate.
pub const ALL: &[&str] = &["all", "todos", "todas", "every", "everything"];

/// Articles before a reference ("the webapp one").
pub const ARTICLES: &[&str] = &["the", "el", "la"];

/// Words joining two clauses (a comma does too, alone or before "and" / "y").
pub const CLAUSE_JOINERS: &[&str] = &["and", "y"];

/// Words joining two target groups (a comma does too, alone or before one of these).
pub const TARGET_JOINERS: &[&str] = &["and", "y", "&"];

/// The longest message read as a command, in bytes.
pub const MAX_MESSAGE_BYTES: usize = 4096;

/// The most tokens a command may have.
pub const MAX_TOKENS: usize = 48;

/// The most clauses a command may hold.
pub const MAX_CLAUSES: usize = 8;

/// The most groups one target list may hold.
pub const MAX_GROUPS: usize = 16;

/// The word "orchestrator": never a reference, nor a word it matches as a part.
pub const ORCHESTRATOR_WORD: &str = "orchestrator";

/// Words that are no vocabulary of the grammar but that a user writes for threads or items
/// ("set agents to auto mode", "allow each"): never a bare thread reference either.
pub const NOT_NAMES: &str = "\
    request requests permission permissions prompt prompts thread threads agent agents worker \
    workers item items one ones each option opción opcion thank thanks gracias favor use make \
    move into al cambiar permitir orchestrator";

/// Courtesy words, closing remarks and forms of address of English, Spanish and a few other
/// languages: never a bare thread reference, so "allow api, cheers" never approves a worker
/// named `cheers` (or aliased `thx`, or named `cheers-1`); such a thread is named only as
/// `@cheers`. A compound ("okay-thanks", "thank-you") is caught by its parts.
pub const COURTESY: &str = "\
    please pls plz plez pleeze porfa porfis porfavor xfa pf thank thanks thx thnx thanx tnx tks \
    thks tx ty tysm tyvm thankyou thanku tq cheers ta kudos appreciated appreciate gracias \
    graciass merci danke grazie obrigado obrigada arigato cool great perfect nice awesome \
    excellent brilliant lovely sweet amazing wonderful fantastic superb neat fine good done \
    np genial perfecto perfecta excelente bueno buenisimo buenísimo vale listo chao chau ciao \
    bye adios adiós saludos lol haha yay mate bro buddy dude";

/// Hedging, conditional and temporal qualifiers, manner adverbs and indefinite choices of
/// English and Spanish: never a bare thread reference, so "allow api, conditionally" never
/// approves a worker named `conditionally-1` and "set either to bypass mode" never changes a
/// worker named `either-1`; such a thread is named only as `@x`. A compound
/// ("maybe-later", "either-1") is caught by its parts.
pub const QUALIFIERS: &str = "\
    conditionally conditional tentatively tentative provisionally provisional temporarily \
    temporary maybe perhaps possibly possible probably probable likely unlikely hopefully \
    ideally eventually later soon sometime sometimes someday once twice first last next again \
    also only just still yet already anyway anyhow otherwise instead rather quickly carefully \
    partially partly mostly fully almost nearly somehow somewhat kinda sorta presumably \
    supposedly apparently briefly momentarily meanwhile afterwards afterward today tonight \
    tomorrow whenever wherever however either neither whichever whatever whoever whomever any \
    anyone anything some someone something each every other others another one ones few \
    several many most quizás quizas quizá quiza tal vez acaso capaz luego después despues \
    pronto mañana manana temporalmente provisionalmente condicionalmente tentativamente \
    probablemente posiblemente seguramente cualquiera cualquier algún algun alguno alguna \
    algunos algunas otro otra otros otras cada demás demas uno varios varias muchos muchas \
    pocos pocas";

/// Common function words of English and Spanish that never name a thread on their own
/// (only as `@x`), so a worker named `never-1` cannot turn "never allow api" into a command.
/// Every word of the vocabulary is never a bare reference either ([`stop_word`]). Negations,
/// conditions and exceptions first, then pronouns, articles, auxiliaries, prepositions,
/// conjunctions and common verbs.
pub const STOPLIST: &str = "\
    no not never don't dont nor neither none nothing if unless until when only except but \
    without si sin nunca ni tampoco salvo excepto menos cuando solo sólo whether though \
    although once while before after instead isn't aren't won't can't cannot shouldn't \
    wouldn't didn't doesn't haven't maybe perhaps wait later first aunque mientras antes \
    después despues hasta pero porque quizás quizas todavía todavia aún aun ya i you we they \
    he she me my your our their his her its it this that these those them what which who how \
    why where there here do does did done is are was were be been have has had can could will \
    would should must may might shall please the a an and or then also plus to in on at of for \
    with from by as well so now just yet still again any some other another more most less \
    very too go get let run stop keep más mas que qué de del el la lo le les los las un una \
    unos unas y e o u con por para es son se su sus mi tu yo esto eso este esta ese esa hay \
    haz hacer luego también tambien";

/// Negative pronouns and quantity words of English and Spanish (with their joined forms; the
/// hyphen and underscore forms hold "no" or another word of [`STOPLIST`] as a part): never a
/// bare thread reference, nor a part of one, nor what a bare typo resolves to, so a worker
/// named `nobody-1` cannot turn "allow nobody" into an approval.
pub const RESERVED: &str = "\
    nobody noone noones nonone none nothing nowhere neither nor nadie ninguno ninguna ningunos \
    ningunas ningun ningún nada nunca ni everyone everybody anyone anybody someone somebody \
    anything something alguien alguno alguna algunos algunas algun algún cualquiera cualquier \
    todo toda";

/// A blocking item that was waiting (and seen) when the user's message arrived.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Waiting {
    /// Attention id.
    pub attn_id: String,
    /// Its thread (id).
    pub thread: String,
    /// Permission or question.
    pub reason: AttentionReason,
    /// Its options as listed.
    pub options: Vec<String>,
}

/// What a relayed action is read against: the user's message for the orchestrator's turn,
/// the threads as they were when it arrived (orchestrators included: they are never targets),
/// and the blocking items that were waiting and seen then (none of an orchestrator, no confirm
/// card).
#[derive(Debug, Clone, Copy)]
pub struct Scene<'a> {
    /// The user's message, verbatim.
    pub message: &'a str,
    /// Every thread as of the message.
    pub threads: &'a [ThreadInfo],
    /// The blocking items waiting and seen when it arrived.
    pub waiting: &'a [Waiting],
}

/// How a message covers one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Coverage {
    /// The message is a command, and every action of it covering the target gives exactly
    /// this.
    Direct,
    /// The message is a lone answer ("yes", "allow it") that would give the target exactly
    /// this, but names nothing: it may answer the orchestrator's own question, so the call is
    /// confirmed on a card, never applied directly.
    Lone,
    /// The message is no command, or no action of it covers the target.
    Uncovered,
    /// Actions of the command cover the target, but not with this answer or mode alone
    /// (another answer or mode, or one the item cannot take).
    Conflict,
}

// ---------------------------------------------------------------------------------------------
// Words
// ---------------------------------------------------------------------------------------------

/// Lowercase, curly quotes and apostrophes straightened.
pub fn normalize(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '\u{B4}' | '`' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{AB}' | '\u{BB}' => '"',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// What ends or breaks a sentence (full-width forms included): no command may hold one, its
/// one trailing `.` or `!` aside, nor a control character but a tab.
const SENTENCE_MARKS: &[char] = &[
    '.', '!', '?', '¿', '¡', ';', ':', '…', '\u{3002}', '\u{FF01}', '\u{FF0E}', '\u{FF1A}',
    '\u{FF1B}', '\u{FF1F}', '\u{2028}', '\u{2029}',
];

/// The tokens of `message` when it can be a command at all (module docs, "The message"):
/// one sentence, at most [`MAX_MESSAGE_BYTES`] bytes and [`MAX_TOKENS`] tokens.
fn tokens(message: &str) -> Option<Vec<String>> {
    if message.len() > MAX_MESSAGE_BYTES {
        return None;
    }
    let t = message.trim();
    let t = t.strip_suffix(['.', '!']).unwrap_or(t);
    if t.chars()
        .any(|c| SENTENCE_MARKS.contains(&c) || (c.is_control() && c != '\t'))
    {
        return None;
    }
    let mut out = Vec::new();
    for chunk in normalize(t).split_whitespace() {
        for (i, piece) in chunk.split(',').enumerate() {
            if i > 0 {
                out.push(",".to_string());
            }
            if !piece.is_empty() {
                out.push(piece.to_string());
            }
        }
        if out.len() > MAX_TOKENS {
            return None;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The mode one word spells ([`MODES`]: its words joined by nothing, `-` or `_`).
fn mode_word(w: &str) -> Option<&'static str> {
    static SPELLINGS: std::sync::OnceLock<BTreeMap<String, &'static str>> =
        std::sync::OnceLock::new();
    SPELLINGS
        .get_or_init(|| {
            let mut m = BTreeMap::new();
            for (id, words) in MODES {
                for sep in ["", "-", "_"] {
                    m.entry(words.join(sep)).or_insert(*id);
                }
            }
            m
        })
        .get(w)
        .copied()
}

/// The mode two words spell apart ("read only").
fn mode_words(a: &str, b: &str) -> Option<&'static str> {
    MODES
        .iter()
        .find(|(_, words)| *words == [a, b])
        .map(|(id, _)| *id)
}

/// Whether `a` and `b` are at most one edit apart: one character inserted, deleted or
/// replaced, or two neighbours swapped ("paln").
fn within_one_edit(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if l.len() - s.len() > 1 {
        return false;
    }
    let p = s.iter().zip(&l).take_while(|(x, y)| x == y).count();
    if s.len() == l.len() {
        let swapped = p + 1 < s.len() && s[p] == l[p + 1] && s[p + 1] == l[p];
        s[p..].iter().skip(1).eq(l[p..].iter().skip(1)) || (swapped && s[p + 2..] == l[p + 2..])
    } else {
        s[p..].iter().eq(l[p + 1..].iter())
    }
}

/// The mode word `w` is a typo of ([`NOT_TYPOS`] aside): one edit from the one-word spelling
/// (joined by nothing) of exactly one mode of 4+ letters. Read only directly before "mode".
fn mode_typo(w: &str) -> Option<&'static str> {
    if NOT_TYPOS.contains(&w) || w.chars().count() < 3 || !w.chars().all(char::is_alphabetic) {
        return None;
    }
    let hits: BTreeSet<&str> = MODES
        .iter()
        .map(|(id, words)| (*id, words.concat()))
        .filter(|(_, k)| k.chars().count() >= 4 && k != w && within_one_edit(w, k))
        .map(|(id, _)| id)
        .collect();
    match hits.len() {
        1 => hits.into_iter().next(),
        _ => None,
    }
}

/// Whether bare word `w` can never be a thread reference: a word of [`STOPLIST`],
/// [`RESERVED`], [`NOT_NAMES`], [`COURTESY`] or [`QUALIFIERS`], or a word of the vocabulary (of
/// any of its phrases too: "go", "ahead", "read", "don't").
pub fn stop_word(w: &str) -> bool {
    static WORDS: std::sync::OnceLock<BTreeSet<&'static str>> = std::sync::OnceLock::new();
    WORDS
        .get_or_init(|| {
            let lists: [&[&'static str]; 14] = [
                APPROVE,
                DENY,
                ANSWER_PREPOSITIONS,
                VERBS,
                SPANISH_VERBS,
                SPANISH_PREPOSITIONS,
                MODE_WORDS,
                WEAK_MODES,
                BOTH,
                ALL,
                ARTICLES,
                CLAUSE_JOINERS,
                TARGET_JOINERS,
                &["it", "them", "to", "the", "si"],
            ];
            lists
                .iter()
                .flat_map(|l| l.iter())
                .chain(MODES.iter().flat_map(|(_, words)| words.iter()))
                .flat_map(|p| std::iter::once(*p).chain(p.split(' ')))
                .chain(
                    [STOPLIST, RESERVED, NOT_NAMES, COURTESY, QUALIFIERS]
                        .iter()
                        .flat_map(|l| l.split_whitespace()),
                )
                .collect()
        })
        .contains(w)
        || mode_word(w).is_some()
}

// ---------------------------------------------------------------------------------------------
// Thread references
// ---------------------------------------------------------------------------------------------

/// Whether `c` is a letter or digit shown as itself: alphanumeric, and neither a combining
/// mark nor a character Rust's `Debug` escapes (`char::escape_debug`: controls, format and
/// invisible characters, grapheme extenders).
fn plain_char(c: char) -> bool {
    c.is_alphanumeric() && c.escape_debug().nth(1).is_none()
}

/// Whether word `w` (without its `@`) is spelled as a plain identifier: letters, digits, `-`,
/// `_` and `.`, with a letter or digit at both ends ([`plain_char`]). Only such a word can be a
/// thread reference: a sign, a symbol, an emoji, a combining mark or an invisible
/// character in or attached to it ("-web", "!web", "web👍", "w\u{200B}eb") makes it none.
pub fn plain_identifier(w: &str) -> bool {
    w.chars().next().is_some_and(plain_char)
        && w.chars().next_back().is_some_and(plain_char)
        && w.chars()
            .all(|c| plain_char(c) || matches!(c, '-' | '_' | '.'))
}

/// The `-` / `_` / `.` parts of `w`.
fn parts(w: &str) -> impl Iterator<Item = &str> {
    w.split(['-', '_', '.']).filter(|p| !p.is_empty())
}

/// Whether bare word `w` can never name a thread: a [`stop_word`] whole or as a part
/// ("no-one", "never-1"), or digits only, whole or in every part ("2", "1-2": a quantity).
fn never_bare(w: &str) -> bool {
    stop_word(w) || parts(w).any(stop_word) || parts(w).all(|p| p.chars().all(char::is_numeric))
}

/// The threads as references are matched against, each normalized once per reading: its
/// id, whether it is an orchestrator, its exact words (name, aliases, id), its former names
/// and its name's `-` / `_` parts of 3+ characters.
pub struct Names {
    threads: Vec<Named>,
}

/// One thread of [`Names`].
struct Named {
    id: String,
    orchestrator: bool,
    exact: Vec<String>,
    former: Vec<String>,
    parts: Vec<String>,
}

impl Names {
    /// The names of `threads`.
    pub fn new(threads: &[ThreadInfo]) -> Self {
        let threads = threads
            .iter()
            .map(|t| {
                let name = normalize(&t.name);
                let parts = name
                    .split(['-', '_'])
                    .filter(|p| p.chars().count() >= 3)
                    .map(str::to_string)
                    .collect();
                let mut exact = vec![name, normalize(&t.id)];
                exact.extend(t.aliases.iter().map(|a| normalize(a)));
                Named {
                    id: t.id.clone(),
                    orchestrator: t.is_orchestrator,
                    exact,
                    former: t.former_names.iter().map(|n| normalize(n)).collect(),
                    parts,
                }
            })
            .collect();
        Names { threads }
    }

    /// The thread token `w` names (module docs, REF): the one thread it is exactly a name,
    /// alias or id of, a former name of, or a whole name part (3+ characters) of; never one
    /// another thread matches in any of these ways too (`api` beside `api` and `x-api-y`: words
    /// are read without case, and a thread can be renamed or made with a name that captures a
    /// word), never one an orchestrator thread (or the word "orchestrator") matches in any of
    /// them. A bare word is never a [`never_bare`] word; `@x` may be any plain identifier.
    pub fn reference(&self, w: &str) -> Option<String> {
        let mut hits = self.matches(w)?.into_iter();
        match (hits.next(), hits.next()) {
            (Some(id), None) => Some(id),
            _ => None,
        }
    }

    /// Every thread token `w` matches as a reference could (exactly a name, alias or id, a
    /// former name, or a whole name part), the uniqueness rule aside; `None` when it can never
    /// be a reference ([`Names::reference`]: not a plain identifier, a [`never_bare`] bare word,
    /// or one an orchestrator thread or the word "orchestrator" matches).
    fn matches(&self, w: &str) -> Option<BTreeSet<String>> {
        let (bare, w) = match w.strip_prefix('@') {
            Some(rest) => (false, rest),
            None => (true, w),
        };
        if !plain_identifier(w) || (bare && never_bare(w)) {
            return None;
        }
        let w = normalize(w);
        let hit = |t: &Named| t.exact.contains(&w) || t.former.contains(&w) || t.parts.contains(&w);
        if w == ORCHESTRATOR_WORD || self.threads.iter().any(|t| t.orchestrator && hit(t)) {
            return None;
        }
        Some(
            self.threads
                .iter()
                .filter(|t| hit(t))
                .map(|t| t.id.clone())
                .collect(),
        )
    }
}

/// Every thread a word of `scene`'s message could name ([`Names`]: exactly a name, alias or
/// id, a former name or a name part; bare or as `@x`), whether or not it names it alone. The
/// hub refuses to apply a command directly when one of these threads was made or renamed
/// after what the user had seen: the message was written against the names the user saw, and
/// a thread made or renamed meanwhile (by the orchestrator, say) could capture a word of it.
/// Empty when the message can be no command.
pub fn named_threads(scene: &Scene) -> BTreeSet<String> {
    let Some(toks) = tokens(scene.message) else {
        return BTreeSet::new();
    };
    let names = Names::new(scene.threads);
    toks.iter()
        .filter_map(|t| names.matches(t))
        .flatten()
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The grammar
// ---------------------------------------------------------------------------------------------

/// An answer as the command says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// An approval word or phrase, as written.
    Approve(String),
    /// A refusal word, as written.
    Deny(String),
}

/// What one clause asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Act {
    /// An answer.
    Answer(Answer),
    /// A permission mode (canonical id).
    Mode(&'static str),
}

/// One target group.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Group {
    /// `[the] REF [one]`: the thread (id).
    Ref(String),
    /// both, ambos, ambas, "them both".
    Both,
    /// all, todos, todas, every, everything, "them all".
    All,
}

/// One clause as parsed (`groups` empty: a lone answer, the whole message).
#[derive(Debug, Clone)]
struct Clause {
    act: Act,
    groups: Vec<Group>,
}

/// The complete parses from a position: how many (0, 1, or 2 for two or more) and the first.
type Parses = (u8, Option<Vec<Clause>>);

/// The parser of one message's tokens.
struct Parser<'a> {
    toks: &'a [String],
    /// Each token read as a thread reference, once.
    refs: Vec<Option<String>>,
    /// The clauses from each position, once.
    clauses: HashMap<usize, Vec<(Clause, usize)>>,
    /// The complete parses from each position with so many clauses left: how many (0, 1, or 2
    /// for two or more) and the first one.
    memo: HashMap<(usize, usize), Parses>,
}

impl<'a> Parser<'a> {
    fn new(toks: &'a [String], threads: &'a [ThreadInfo]) -> Self {
        Parser {
            toks,
            refs: {
                let names = Names::new(threads);
                let mut seen: HashMap<&str, Option<String>> = HashMap::new();
                toks.iter()
                    .map(|t| {
                        seen.entry(t.as_str())
                            .or_insert_with(|| names.reference(t))
                            .clone()
                    })
                    .collect()
            },
            clauses: HashMap::new(),
            memo: HashMap::new(),
        }
    }

    /// The token at `i`.
    fn w(&self, i: usize) -> Option<&'a str> {
        self.toks.get(i).map(String::as_str)
    }

    /// Whether the token at `i` is one of `list`.
    fn is(&self, i: usize, list: &[&str]) -> bool {
        self.w(i).is_some_and(|w| list.contains(&w))
    }

    /// The answer word or phrase at `i`: the answer and where it ends.
    fn answer_word(&self, i: usize) -> Option<(Answer, usize)> {
        let w = self.w(i)?;
        if let Some(b) = self.w(i + 1) {
            let phrase = format!("{w} {b}");
            if APPROVE.contains(&phrase.as_str()) {
                return Some((Answer::Approve(phrase), i + 2));
            }
        }
        if APPROVE.contains(&w) {
            Some((Answer::Approve(w.to_string()), i + 1))
        } else if DENY.contains(&w) {
            Some((Answer::Deny(w.to_string()), i + 1))
        } else {
            None
        }
    }

    /// The GROUPs at `i`, each with where it ends.
    fn groups(&self, i: usize) -> Vec<(Group, usize)> {
        let mut out = Vec::new();
        match self.w(i) {
            Some(w) if BOTH.contains(&w) => out.push((Group::Both, i + 1)),
            Some(w) if ALL.contains(&w) => out.push((Group::All, i + 1)),
            Some("them") => match self.w(i + 1) {
                Some("both") => out.push((Group::Both, i + 2)),
                Some("all") => out.push((Group::All, i + 2)),
                _ => {}
            },
            _ => {}
        }
        let s = if self.is(i, ARTICLES) { i + 1 } else { i };
        if let Some(Some(t)) = self.refs.get(s) {
            out.push((Group::Ref(t.clone()), s + 1));
            if self.w(s + 1) == Some("one") {
                out.push((Group::Ref(t.clone()), s + 2));
            }
        }
        out
    }

    /// Where a joiner at `j` ends, for each reading: `,` alone, `,` and a word of `words`, or
    /// a word of `words`.
    fn joiners(&self, j: usize, words: &[&str]) -> Vec<usize> {
        if self.w(j) == Some(",") {
            let mut out = vec![j + 1];
            if self.is(j + 1, words) {
                out.push(j + 2);
            }
            out
        } else if self.is(j, words) {
            vec![j + 1]
        } else {
            Vec::new()
        }
    }

    /// Every TARGETS reading at `i`, each with where it ends.
    /// A reading past [`MAX_GROUPS`] groups is dropped on its own; the shorter readings of the
    /// same list stay (the tokens after them may start the next clause).
    fn targets(&self, i: usize) -> Vec<(Vec<Group>, usize)> {
        let mut out = Vec::new();
        let mut stack: Vec<(Vec<Group>, usize)> = vec![(Vec::new(), i)];
        while let Some((list, at)) = stack.pop() {
            if list.len() == MAX_GROUPS {
                continue;
            }
            for (g, e) in self.groups(at) {
                let mut l = list.clone();
                l.push(g);
                for k in self.joiners(e, TARGET_JOINERS) {
                    stack.push((l.clone(), k));
                }
                out.push((l, e));
            }
        }
        out
    }

    /// The MODENAMEs at `j`: `(mode, end, needs "mode" after it)`.
    fn mode_names(&self, j: usize) -> Vec<(&'static str, usize, bool)> {
        let mut out = Vec::new();
        if let (Some(a), Some(b)) = (self.w(j), self.w(j + 1)) {
            if let Some(m) = mode_words(a, b) {
                out.push((m, j + 2, false));
            }
        }
        if let Some(a) = self.w(j) {
            if let Some(m) = mode_word(a) {
                out.push((m, j + 1, WEAK_MODES.contains(&m)));
            } else if self.is(j + 1, MODE_WORDS) {
                if let Some(m) = mode_typo(a) {
                    out.push((m, j + 1, true));
                }
            }
        }
        out
    }

    /// The modes after the preposition, from `j`: `MODENAME (mode | modo)` or `STRONG`; with
    /// `modo_first`, also `modo MODENAME` (no typo there).
    fn modes_at(&self, j: usize, modo_first: bool) -> Vec<(&'static str, usize)> {
        let mut out = Vec::new();
        for (m, e, needs_word) in self.mode_names(j) {
            if self.is(e, MODE_WORDS) {
                out.push((m, e + 1));
            }
            if !needs_word {
                out.push((m, e));
            }
        }
        if modo_first && self.w(j) == Some("modo") {
            if let Some(m) = self.w(j + 1).and_then(mode_word) {
                out.push((m, j + 2));
            }
            if let Some(m) = self
                .w(j + 1)
                .zip(self.w(j + 2))
                .and_then(|(a, b)| mode_words(a, b))
            {
                out.push((m, j + 3));
            }
        }
        out
    }

    /// The CLAUSEs at `i`, each with where it ends.
    fn clauses_at(&mut self, i: usize) -> Vec<(Clause, usize)> {
        if let Some(c) = self.clauses.get(&i) {
            return c.clone();
        }
        let n = self.toks.len();
        let mut out = Vec::new();
        // ANSWER
        if let Some((a, k)) = self.answer_word(i) {
            let lone_only = matches!(&a, Answer::Deny(w) if LONE_ONLY.contains(&w.as_str()));
            if i == 0 && (k == n || (!lone_only && k + 1 == n && self.w(k) == Some("it"))) {
                out.push((
                    Clause {
                        act: Act::Answer(a.clone()),
                        groups: Vec::new(),
                    },
                    n,
                ));
            }
            if !lone_only {
                let mut starts = vec![k];
                if self.is(k, ANSWER_PREPOSITIONS) {
                    starts.push(k + 1);
                }
                for s in starts {
                    for (groups, e) in self.targets(s) {
                        let act = Act::Answer(a.clone());
                        out.push((Clause { act, groups }, e));
                    }
                }
            }
        }
        // MODE, English
        let s = if self.is(i, VERBS) { i + 1 } else { i };
        for (groups, e) in self.targets(s) {
            if self.w(e) == Some("to") {
                let m0 = if self.w(e + 1) == Some("the") {
                    e + 2
                } else {
                    e + 1
                };
                for (m, end) in self.modes_at(m0, false) {
                    let groups = groups.clone();
                    out.push((
                        Clause {
                            act: Act::Mode(m),
                            groups,
                        },
                        end,
                    ));
                }
            }
        }
        // MODE, Spanish
        if self.is(i, SPANISH_VERBS) {
            for (groups, e) in self.targets(i + 1) {
                if self.is(e, SPANISH_PREPOSITIONS) {
                    for (m, end) in self.modes_at(e + 1, true) {
                        let groups = groups.clone();
                        out.push((
                            Clause {
                                act: Act::Mode(m),
                                groups,
                            },
                            end,
                        ));
                    }
                }
            }
        }
        self.clauses.insert(i, out.clone());
        out
    }

    /// The complete parses of the tokens from `i` with at most `left` clauses: how many (2 for
    /// two or more) and the first.
    fn parses(&mut self, i: usize, left: usize) -> Parses {
        if left == 0 {
            return (0, None);
        }
        if let Some(r) = self.memo.get(&(i, left)) {
            return r.clone();
        }
        let n = self.toks.len();
        let mut count = 0u8;
        let mut first = None;
        for (c, e) in self.clauses_at(i) {
            if e == n {
                count = count.saturating_add(1);
                first.get_or_insert_with(|| vec![c.clone()]);
                continue;
            }
            for k in self.joiners(e, CLAUSE_JOINERS) {
                if k >= n {
                    continue;
                }
                let (more, rest) = self.parses(k, left - 1);
                count = count.saturating_add(more);
                if let Some(rest) = rest {
                    first.get_or_insert_with(|| {
                        let mut v = vec![c.clone()];
                        v.extend(rest);
                        v
                    });
                }
            }
            if count > 1 {
                break;
            }
        }
        let r = (count.min(2), first);
        self.memo.insert((i, left), r.clone());
        r
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a message
// ---------------------------------------------------------------------------------------------

/// One action of a command, its targets resolved: item ids for an answer, thread ids for a
/// mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// What it asks.
    pub act: Act,
    /// What it covers.
    pub targets: BTreeSet<String>,
    /// A lone answer (module docs, "Meaning"): the whole message is an answer word, or one and
    /// "it", naming nothing.
    pub lone: bool,
}

/// The answer candidates of `scene`: the items waiting and seen, none of an orchestrator.
fn answer_candidates<'a>(scene: &Scene<'a>) -> Vec<&'a Waiting> {
    scene
        .waiting
        .iter()
        .filter(|w| {
            !scene
                .threads
                .iter()
                .any(|t| t.is_orchestrator && t.id == w.thread)
        })
        .collect()
}

/// The mode candidates of `scene`: the workers not stopped, not an orchestrator, not the hub
/// thread.
fn mode_candidates(scene: &Scene) -> BTreeSet<String> {
    scene
        .threads
        .iter()
        .filter(|t| !t.is_orchestrator && !t.stopped && t.id != HUB_THREAD)
        .map(|t| t.id.clone())
        .collect()
}

/// What clause `c` covers, or `None` when a group of it picks out nothing (module docs,
/// "Meaning").
fn resolve_clause(scene: &Scene, c: &Clause) -> Option<BTreeSet<String>> {
    let cover =
        |all: BTreeSet<String>, g: &Group, of: &dyn Fn(&str) -> Option<BTreeSet<String>>| match g {
            Group::Ref(t) => of(t),
            Group::Both => (all.len() == 2).then_some(all),
            Group::All => (!all.is_empty()).then_some(all),
        };
    let mut out = BTreeSet::new();
    match &c.act {
        Act::Answer(_) => {
            let cands = answer_candidates(scene);
            let all: BTreeSet<String> = cands.iter().map(|w| w.attn_id.clone()).collect();
            if c.groups.is_empty() {
                return (all.len() == 1).then_some(all);
            }
            let of = |t: &str| -> Option<BTreeSet<String>> {
                let items: BTreeSet<String> = cands
                    .iter()
                    .filter(|w| w.thread == t)
                    .map(|w| w.attn_id.clone())
                    .collect();
                (items.len() == 1).then_some(items)
            };
            for g in &c.groups {
                out.extend(cover(all.clone(), g, &of)?);
            }
        }
        Act::Mode(_) => {
            let all = mode_candidates(scene);
            let of = |t: &str| -> Option<BTreeSet<String>> {
                // A named worker counts even when stopped; never the hub thread.
                let ok = t != HUB_THREAD
                    && scene
                        .threads
                        .iter()
                        .any(|x| x.id == t && !x.is_orchestrator);
                ok.then(|| BTreeSet::from([t.to_string()]))
            };
            for g in &c.groups {
                out.extend(cover(all.clone(), g, &of)?);
            }
        }
    }
    Some(out)
}

/// Reads `scene.message`: the actions of the command it is, or `None` when it is no command
/// (outside the grammar, read two ways, past a limit, or a group picking out nothing).
pub fn read(scene: &Scene) -> Option<Vec<Action>> {
    let toks = tokens(scene.message)?;
    let mut p = Parser::new(&toks, scene.threads);
    let (count, parse) = p.parses(0, MAX_CLAUSES);
    if count != 1 {
        return None;
    }
    parse?
        .into_iter()
        .map(|c| {
            Some(Action {
                targets: resolve_clause(scene, &c)?,
                lone: c.groups.is_empty(),
                act: c.act,
            })
        })
        .collect()
}

/// Whether `message` reads as a command of the grammar ([`read`]), one way or several, however
/// its targets would resolve: an answer or a mode change for threads by name, quantity or
/// mode. For text that must never be taken for one (a next message a model suggested).
pub fn is_command(message: &str, threads: &[ThreadInfo]) -> bool {
    let Some(toks) = tokens(message) else {
        return false;
    };
    let mut p = Parser::new(&toks, threads);
    p.parses(0, MAX_CLAUSES).0 > 0
}

#[cfg(test)]
mod is_command_tests {
    use super::*;

    #[test]
    fn commands_are_told_from_messages() {
        let threads = vec![ThreadInfo {
            id: "t-api".into(),
            name: "api".into(),
            aliases: Vec::new(),
            former_names: Vec::new(),
            project: None,
            status: twr_core::reduce::ThreadStatus::Idle,
            runtime: "claude-code".into(),
            is_orchestrator: false,
            stopped: false,
            top_attention: None,
            last_report: None,
        }];
        for c in [
            "set all to bypass",
            "dale a ambos",
            "allow api",
            "approve both",
            "set api to auto mode",
        ] {
            assert!(is_command(c, &threads), "{c}");
        }
        for m in [
            "now run the integration tests",
            "check the api logs",
            "what changed in api?",
        ] {
            assert!(!is_command(m, &threads), "{m}");
        }
    }
}

/// Fully resolved actions of an unambiguous command, suitable for direct user execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectAction {
    /// A permission's canonical option label.
    Answer { attn_id: String, label: String },
    /// A worker's canonical permission mode.
    Mode { thread: String, mode: String },
}

/// Resolve the whole command before applying anything. Conflicts, questions and a lone answer
/// ([`Coverage::Lone`]) fall back.
pub fn direct_actions(scene: &Scene) -> Option<Vec<DirectAction>> {
    let mut out = BTreeMap::new();
    for action in read(scene)? {
        if action.lone {
            return None;
        }
        for target in action.targets {
            let (key, resolved) = match &action.act {
                Act::Answer(a) => {
                    let item = scene.waiting.iter().find(|w| w.attn_id == target)?;
                    let label = resolve(a, item)?;
                    (
                        format!("answer:{target}"),
                        DirectAction::Answer {
                            attn_id: target,
                            label,
                        },
                    )
                }
                Act::Mode(mode) => (
                    format!("mode:{target}"),
                    DirectAction::Mode {
                        thread: target,
                        mode: mode.to_string(),
                    },
                ),
            };
            if out.get(&key).is_some_and(|previous| previous != &resolved) {
                return None;
            }
            out.insert(key, resolved);
        }
    }
    (!out.is_empty()).then(|| out.into_values().collect())
}

// ---------------------------------------------------------------------------------------------
// Answers
// ---------------------------------------------------------------------------------------------

/// The polarity of a permission's option `label`: `allow` / `deny` (trimmed, any case), else
/// [`reply::label_polarity`] for a short label.
fn permission_polarity(label: &str) -> Option<Reply> {
    let t = label.trim();
    if t.len() > 64 || t.split_whitespace().nth(reply::MAX_REPLY_WORDS).is_some() {
        return None;
    }
    match t.to_lowercase().as_str() {
        "allow" => Some(Reply::Allow),
        "deny" => Some(Reply::Deny),
        _ => reply::label_polarity(t),
    }
}

/// The option label answer `a` gives item `w`, if exactly one: only a permission takes an
/// answer (module docs, "Meaning").
fn resolve(a: &Answer, w: &Waiting) -> Option<String> {
    if w.reason != AttentionReason::Permission {
        return None;
    }
    let (word, want, bare) = match a {
        Answer::Approve(p) => (p.as_str(), Reply::Allow, "allow"),
        Answer::Deny(p) => (p.as_str(), Reply::Deny, "deny"),
    };
    if w.options.is_empty() {
        return Some(bare.to_string());
    }
    let only = |hits: BTreeSet<&String>| -> Option<String> {
        let mut it = hits.into_iter();
        match (it.next(), it.next()) {
            (Some(l), None) => Some(l.clone()),
            _ => None,
        }
    };
    // A label spelled as the answer word, of its polarity or none: "no" for ["Yes", "No"].
    let spelled: BTreeSet<&String> = w
        .options
        .iter()
        .filter(|o| o.trim().len() <= 16 && normalize(o.trim()) == word)
        .collect();
    if !spelled.is_empty() {
        let l = only(spelled)?;
        return permission_polarity(&l)
            .is_none_or(|p| p == want)
            .then_some(l);
    }
    only(
        w.options
            .iter()
            .filter(|o| permission_polarity(o) == Some(want))
            .collect(),
    )
}

/// The item's option `decision` names (trimmed; spelled exactly, else case-insensitive and
/// unique): its label as listed and its index (`None` for a permission listing no options:
/// `allow` / `deny`).
pub fn canonical_label(item: &Waiting, decision: &str) -> Option<(String, Option<usize>)> {
    let d = decision.trim();
    let mut exact = item
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.trim() == d);
    if let (Some((i, o)), None) = (exact.next(), exact.next()) {
        return Some((o.clone(), Some(i)));
    }
    let mut hits = item
        .options
        .iter()
        .enumerate()
        .filter(|(_, o)| o.trim().eq_ignore_ascii_case(d));
    if let Some((i, o)) = hits.next() {
        return hits.next().is_none().then(|| (o.clone(), Some(i)));
    }
    if item.options.is_empty() && item.reason == AttentionReason::Permission {
        let d = d.to_lowercase();
        if d == "allow" || d == "deny" {
            return Some((d, None));
        }
    }
    None
}

/// Direct when every action covering the target says `want`; uncovered when none covers it;
/// a conflict when one says something else (or nothing the target can take).
fn verdict(said: &[Option<String>], want: &str) -> Coverage {
    if said.is_empty() {
        Coverage::Uncovered
    } else if said.iter().all(|s| s.as_deref() == Some(want)) {
        Coverage::Direct
    } else {
        Coverage::Conflict
    }
}

/// How the message of `scene` covers answering item `attn_id` with option `label` (as
/// listed: [`canonical_label`]). A lone answer that would give exactly that is
/// [`Coverage::Lone`].
pub fn answer_coverage(scene: &Scene, attn_id: &str, label: &str) -> Coverage {
    let Some(item) = scene.waiting.iter().find(|w| w.attn_id == attn_id) else {
        return Coverage::Uncovered;
    };
    let actions = read(scene).unwrap_or_default();
    let covering: Vec<&Action> = actions
        .iter()
        .filter(|a| a.targets.contains(attn_id))
        .collect();
    let said: Vec<Option<String>> = covering
        .iter()
        .filter_map(|a| match &a.act {
            Act::Answer(ans) => Some(resolve(ans, item)),
            Act::Mode(_) => None,
        })
        .collect();
    match verdict(&said, label) {
        Coverage::Direct if covering.iter().any(|a| a.lone) => Coverage::Lone,
        v => v,
    }
}

/// How the message of `scene` covers setting thread `thread` to mode `mode`: the mode as the
/// hub canonicalized it for the thread's runtime (`twr-hub` `runtime_spec::canonical_mode`),
/// compared exactly, with no normalization of its own, with the mode the command names (a
/// [`MODES`] id: twr's own spelling, which the hub's canonical form of that mode is on every
/// runtime). A runtime that takes any vocabulary keeps an id the hub does not know as given
/// ("p'lan" for a generic ACP agent), and such an id is never the mode the command names.
pub fn mode_coverage(scene: &Scene, thread: &str, mode: &str) -> Coverage {
    let said: Vec<Option<String>> = read(scene)
        .unwrap_or_default()
        .iter()
        .filter(|a| a.targets.contains(thread))
        .filter_map(|a| match &a.act {
            Act::Mode(m) => Some(Some(m.to_string())),
            Act::Answer(_) => None,
        })
        .collect();
    verdict(&said, mode)
}

// ---------------------------------------------------------------------------------------------
// The loose reading for `twr say`
// ---------------------------------------------------------------------------------------------

/// Whether `message` looks like an answer to items by name or quantity (`twr say` without
/// `seen_gseq`: such a message is never routed; `say` lists the pending items): an answer
/// word followed within three words by a quantifier or a thread reference ("allow both",
/// "no to all", "allow api, and then write a summary"), or an answer word in a message that
/// names two threads ("@api yes, @web no"). Loose on purpose: it reads every word it knows and
/// skips the rest; it only keeps a message from being sent as a prompt and never answers
/// anything.
pub fn answer_attempt(ctx: &RouteContext, message: &str) -> bool {
    const QUANTIFIERS: &[&str] = &[
        "both",
        "ambos",
        "ambas",
        "all",
        "every",
        "each",
        "everything",
        "todos",
        "todas",
        "them",
        "these",
        "those",
    ];
    #[derive(PartialEq)]
    enum K {
        Answer,
        Target(Option<String>),
        Other,
    }
    let words: Vec<String> = normalize(message)
        .split(|c: char| c.is_whitespace() || c == ',' || c == '&')
        .map(|w| {
            w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '@'))
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect();
    let answer = |w: &str| APPROVE.contains(&w) || DENY.contains(&w) || w == "si";
    let quantifier = |w: &str| {
        QUANTIFIERS.contains(&w)
            || QUANTIFIERS.iter().any(|q| {
                w.strip_prefix(q)
                    .is_some_and(|r| NOT_NAMES.split_whitespace().any(|n| n == r))
            })
    };
    let names = Names::new(&ctx.threads);
    let mut kinds = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if words
            .get(i + 1)
            .is_some_and(|b| APPROVE.contains(&format!("{} {b}", words[i]).as_str()))
        {
            kinds.push(K::Answer);
            i += 2;
            continue;
        }
        let w = words[i].as_str();
        kinds.push(if answer(w) {
            K::Answer
        } else if quantifier(w) {
            K::Target(None)
        } else if let Some(id) = names.reference(w) {
            K::Target(Some(id))
        } else {
            K::Other
        });
        i += 1;
    }
    let near = kinds.iter().enumerate().any(|(i, k)| {
        *k == K::Answer
            && kinds
                .iter()
                .skip(i + 1)
                .take(3)
                .any(|k| matches!(k, K::Target(_)))
    });
    let named: BTreeSet<&String> = kinds
        .iter()
        .filter_map(|k| match k {
            K::Target(Some(id)) => Some(id),
            _ => None,
        })
        .collect();
    near || (kinds.contains(&K::Answer) && named.len() >= 2)
}

#[cfg(test)]
mod tests;

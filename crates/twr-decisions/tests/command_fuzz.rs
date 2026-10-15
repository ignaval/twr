//! Invariant fuzz for short commands (`twr_decisions::code::command`): the hub
//! applies the orchestrator's relayed answer or mode change directly only when the WHOLE user
//! message is one short command of the tiny grammar that covers it; everything else that passes
//! provenance becomes a proposal the user confirms on a card.
//!
//! The reference here is the same grammar written independently, in another style: the module
//! parses left to right with memoized alternatives from each position; the reference cuts the
//! token list at every candidate joiner, matches each clause span exactly against each shape
//! (from the right: "to" / "a" / "en" first, then the targets before it), and reads each target
//! span against every split into groups. Tokens, references, group meaning, answer resolution
//! and the verdicts are written from the spec, not from the module's helpers (the stoplist,
//! reserved, not-names, courtesy and qualifier word lists are data and are imported).
//!
//! Every case draws a registry (twr-shaped names, vocabulary names, names holding function
//! words, negative pronouns, courtesy or qualifier words, numeric names, names near an
//! orchestrator's, aliases (courtesy and qualifier words too), former names (shared now and
//! then, or another thread's name), stopped threads, the hub thread now and then, the
//! orchestrator and a retired one), the items waiting
//! and seen (permissions with allow / deny, no options, deny only, Claude-style Yes / Yes and
//! don't ask / No, Allow once / Allow always / Deny; questions with plain, polar, numeric and
//! command-word labels; an orchestrator's item now and then; up to two per thread), and a
//! message that is one of:
//!
//! - valid: built from the grammar (lone answers, every ANSWER and MODE shape, Spanish forms,
//!   typos before "mode", strong modes with and without "mode", articles, "one", `@x`, "them
//!   both" / "them all", clause and target joiners, upper case, a trailing `.` or `!`);
//! - a mutation that breaks it for sure (a `?`, `;` or `:`, a second sentence, a line break,
//!   courtesy, a noun after a quantifier, "si", a quoted reference, a misspelt reference);
//! - a mutation that may or may not break it (a word dropped, two words swapped, a joiner or a
//!   verb swapped);
//! - soup: random vocabulary, names, junk and punctuation.
//!
//! Then a call per waiting item (a random option, or most of the time what the reference reads
//! for it) and per worker (a random mode or the reference's; a quarter of the time written
//! otherwise and canonicalized as the hub does for a generic ACP agent, which keeps "p'lan" as
//! written). Invariants:
//!
//! - (a) every call's verdict (direct, uncovered, conflict) equals the reference's;
//! - (b) valid messages parse under the reference (unless past a cap), and sure-breaking
//!   mutations do not;
//! - (c) a hub model: a call that passes provenance is applied when direct and proposed
//!   otherwise; nothing is applied without a covering command or the user's Confirm, and
//!   nothing that failed provenance is applied at all;
//! - (d) the confirm card for the proposals (names, labels, mode ids and item summaries
//!   now and then replaced with escapes, invisible characters and the card's own syntax) shows
//!   no invisible character as is and reads back (`card::parse`) as exactly its actions;
//! - (e) when a call is direct: the message is one sentence (no mark but one trailing `.` or
//!   `!`, no line break), of at most 48 tokens; every answer target it covers is a permission,
//!   never a question; and no token read as a reference is one an orchestrator matches
//!   (checked by a function written apart from the reference's resolution);
//! - (f) the user's fast path (`direct_actions`): `Some` only for a message
//!   the reference reads as a command, never empty; every action a permission answer of a
//!   non-orchestrator item with the canonical label `answer_coverage` calls direct, or a mode
//!   id `mode_coverage` calls direct for a worker (never an orchestrator or the hub thread);
//!   exactly the reference's targets, each once (all or nothing); and `Some` whenever every
//!   target the reference reads has one value (a permission answer or a mode).
//!
//! `TWR_FUZZ_SEED` (default: fixed) and `TWR_FUZZ_CASES` (default 10k; the ignored long run
//! does 1M) set the run; a failure prints the seed, the case and the message.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use twr_core::event::{AttentionReason, HUB_THREAD};
use twr_core::reduce::ThreadStatus;
use twr_decisions::code::card::{self, CardLine};
use twr_decisions::code::command::{
    answer_coverage, canonical_label, direct_actions, mode_coverage, Coverage, DirectAction, Scene,
    Waiting, COURTESY, NOT_NAMES, QUALIFIERS, RESERVED, STOPLIST,
};
use twr_decisions::code::reply::{label_polarity, Reply};
use twr_decisions::context::ThreadInfo;

const DEFAULT_SEED: u64 = 0x636d_645f_6675_7a7a;
const DEFAULT_CASES: u64 = 10_000;

// ---------------------------------------------------------------------------------------------
// The spec's vocabulary, written out here
// ---------------------------------------------------------------------------------------------

const APPROVE_1: &[&str] = &[
    "allow", "approve", "accept", "yes", "ok", "okay", "sure", "proceed", "confirm", "lgtm", "sí",
    "dale", "adelante", "aprueba", "apruebo", "permite", "acepta", "yep", "yeah", "yup",
];
const DENY_1: &[&str] = &[
    "deny", "reject", "decline", "no", "nope", "block", "rechaza", "deniega",
];
const ONLY_ALONE: &[&str] = &["no", "nope"];
const PREPS: &[&str] = &["on", "for", "to", "a"];
const VERBS_EN: &[&str] = &["set", "switch", "change", "put", "turn"];
const VERBS_ES: &[&str] = &["pon", "pasa", "cambia"];
const PREPS_ES: &[&str] = &["a", "en"];
const MODE_WORD: &[&str] = &["mode", "modo"];
const R_MODES: &[(&str, &[&str])] = &[
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
const MODE_IDS: &[&str] = &[
    "auto",
    "plan",
    "manual",
    "default",
    "read-only",
    "dontAsk",
    "acceptEdits",
    "bypassPermissions",
];
const WEAK: &[&str] = &["auto", "plan", "manual", "default"];
const NO_TYPO: &[&str] = &[
    "alto", "auth", "autos", "play", "plat", "plain", "plane", "plant", "plans", "clan", "flan",
];
/// Known typos and the mode each must read as, right before "mode".
const TYPOS: &[(&str, &str)] = &[
    ("audo", "auto"),
    ("atuo", "auto"),
    ("paln", "plan"),
    ("plna", "plan"),
    ("manul", "manual"),
    ("defualt", "default"),
    ("bypas", "bypassPermissions"),
    ("readonyl", "read-only"),
    ("dontaks", "dontAsk"),
    ("acceptedit", "acceptEdits"),
];
const TWO: &[&str] = &["both", "ambos", "ambas"];
const EVERY: &[&str] = &["all", "todos", "todas", "every", "everything"];
const ARTICLE: &[&str] = &["the", "el", "la"];
const JOIN_WORDS: &[&str] = &["and", "y"];
const TJOIN_WORDS: &[&str] = &["and", "y", "&"];
const MARKS: &[char] = &[
    '.', '!', '?', '¿', '¡', ';', ':', '…', '。', '！', '．', '：', '；', '？', '\u{2028}',
    '\u{2029}',
];

// ---------------------------------------------------------------------------------------------
// Random numbers
// ---------------------------------------------------------------------------------------------

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.next() % 100 < pct
    }
    fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

// ---------------------------------------------------------------------------------------------
// The reference: tokens and words
// ---------------------------------------------------------------------------------------------

fn norm(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = match c {
            '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' | '\u{B4}' | '`' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{AB}' | '\u{BB}' => '"',
            c => c,
        };
        out.extend(c.to_lowercase());
    }
    out
}

/// The message's tokens, or `None` when it cannot be a command at all.
fn r_tokens(msg: &str) -> Option<Vec<String>> {
    if msg.len() > 4096 {
        return None;
    }
    let mut t = msg.trim();
    if t.ends_with('.') || t.ends_with('!') {
        t = &t[..t.len() - 1];
    }
    if t.chars()
        .any(|c| MARKS.contains(&c) || (c.is_control() && c != '\t'))
    {
        return None;
    }
    let spaced = norm(t).replace(',', " , ");
    let toks: Vec<String> = spaced.split_whitespace().map(String::from).collect();
    (!toks.is_empty() && toks.len() <= 48).then_some(toks)
}

/// The mode one word spells, joined by nothing, `-` or `_`.
fn r_mode1(w: &str) -> Option<&'static str> {
    R_MODES
        .iter()
        .find(|(_, ws)| ["", "-", "_"].iter().any(|sep| ws.join(sep) == w))
        .map(|(id, _)| *id)
}

fn r_mode2(a: &str, b: &str) -> Option<&'static str> {
    R_MODES
        .iter()
        .find(|(_, ws)| ws.len() == 2 && ws[0] == a && ws[1] == b)
        .map(|(id, _)| *id)
}

/// Optimal string alignment distance (insert, delete, replace, swap of neighbours).
fn osa(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[a.len()][b.len()]
}

/// The mode `w` misspells, read right before "mode" / "modo".
fn r_typo(w: &str) -> Option<&'static str> {
    if NO_TYPO.contains(&w) || w.chars().count() < 3 || !w.chars().all(char::is_alphabetic) {
        return None;
    }
    if w.chars().count() > 24 {
        return None;
    }
    let ids: BTreeSet<&str> = R_MODES
        .iter()
        .filter(|(_, ws)| {
            let k = ws.concat();
            k.chars().count() >= 4 && k != w && osa(w, &k) == 1
        })
        .map(|(id, _)| *id)
        .collect();
    (ids.len() == 1).then(|| *ids.iter().next().unwrap())
}

/// The words never read as a bare name: the vocabulary (phrase words too), the imported
/// stoplist, reserved, not-names, courtesy and qualifier words, and every mode spelling.
fn r_stop(w: &str) -> bool {
    static SET: std::sync::OnceLock<BTreeSet<String>> = std::sync::OnceLock::new();
    SET.get_or_init(|| {
        let mut s: BTreeSet<String> = BTreeSet::new();
        let lists: &[&[&str]] = &[
            APPROVE_1,
            DENY_1,
            PREPS,
            VERBS_EN,
            VERBS_ES,
            PREPS_ES,
            MODE_WORD,
            WEAK,
            TWO,
            EVERY,
            ARTICLE,
            JOIN_WORDS,
            TJOIN_WORDS,
        ];
        for l in lists {
            s.extend(l.iter().map(|w| w.to_string()));
        }
        s.extend(["go", "ahead", "it", "them", "to", "the", "si"].map(String::from));
        for (_, ws) in R_MODES {
            s.extend(ws.iter().map(|w| w.to_string()));
            for sep in ["", "-", "_"] {
                s.insert(ws.join(sep));
            }
        }
        for l in [STOPLIST, RESERVED, NOT_NAMES, COURTESY, QUALIFIERS] {
            s.extend(l.split_whitespace().map(String::from));
        }
        s
    })
    .contains(w)
}

fn shown(c: char) -> bool {
    c.is_alphanumeric() && c.escape_debug().count() == 1
}

fn r_plain(w: &str) -> bool {
    let cs: Vec<char> = w.chars().collect();
    !cs.is_empty()
        && shown(cs[0])
        && shown(cs[cs.len() - 1])
        && cs.iter().all(|&c| shown(c) || "-_.".contains(c))
}

fn r_never_bare(w: &str) -> bool {
    let ps: Vec<&str> = w.split(['-', '_', '.']).filter(|p| !p.is_empty()).collect();
    r_stop(w) || ps.iter().any(|p| r_stop(p)) || ps.iter().all(|p| p.chars().all(char::is_numeric))
}

fn whole_of(t: &ThreadInfo, w: &str) -> bool {
    std::iter::once(&t.name)
        .chain(std::iter::once(&t.id))
        .chain(t.aliases.iter())
        .any(|x| norm(x) == w)
}

fn former_of(t: &ThreadInfo, w: &str) -> bool {
    t.former_names.iter().any(|x| norm(x) == w)
}

fn part_in(t: &ThreadInfo, w: &str) -> bool {
    w.chars().count() >= 3 && norm(&t.name).split(['-', '_']).any(|p| p == w)
}

/// The thread token `tok` names.
fn r_ref(tok: &str, threads: &[ThreadInfo]) -> Option<String> {
    let (bare, w) = match tok.strip_prefix('@') {
        Some(w) => (false, w),
        None => (true, tok),
    };
    if !r_plain(w) || (bare && r_never_bare(w)) || w == "orchestrator" {
        return None;
    }
    if threads
        .iter()
        .any(|t| t.is_orchestrator && (whole_of(t, w) || former_of(t, w) || part_in(t, w)))
    {
        return None;
    }
    // Two threads matching in any way (one's name and another's name part or former name):
    // no reference.
    let hits: BTreeSet<&str> = threads
        .iter()
        .filter(|t| whole_of(t, w) || former_of(t, w) || part_in(t, w))
        .map(|t| t.id.as_str())
        .collect();
    match hits.len() {
        1 => hits.into_iter().next().map(str::to_string),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// The reference: the grammar, span by span
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum RG {
    Ref(String),
    Two,
    Every,
}

#[derive(Debug, Clone, PartialEq)]
enum RAct {
    Yes(String),
    No(String),
    Mode(&'static str),
}

#[derive(Debug, Clone)]
struct RClause {
    act: RAct,
    /// Empty: a lone answer (the whole message).
    groups: Vec<RG>,
}

/// How many parses (2 for two or more) and one of them.
type Parses = (u64, Option<Vec<RClause>>);

struct R<'a> {
    toks: &'a [String],
    refs: Vec<Option<String>>,
    memo: HashMap<(usize, usize), Parses>,
}

impl<'a> R<'a> {
    fn new(toks: &'a [String], threads: &[ThreadInfo]) -> Self {
        R {
            toks,
            refs: toks.iter().map(|t| r_ref(t, threads)).collect(),
            memo: HashMap::new(),
        }
    }

    /// The groups at the start of span `lo..hi`: each with its length.
    fn group(&self, lo: usize, hi: usize) -> Vec<(RG, usize)> {
        let t = |i: usize| (i < hi).then(|| self.toks[i].as_str());
        let mut out = Vec::new();
        match (t(lo), t(lo + 1)) {
            (Some(w), _) if TWO.contains(&w) => out.push((RG::Two, 1)),
            (Some(w), _) if EVERY.contains(&w) => out.push((RG::Every, 1)),
            (Some("them"), Some("both")) => out.push((RG::Two, 2)),
            (Some("them"), Some("all")) => out.push((RG::Every, 2)),
            _ => {}
        }
        let at = lo + usize::from(t(lo).is_some_and(|w| ARTICLE.contains(&w)));
        if at < hi {
            if let Some(id) = &self.refs[at] {
                out.push((RG::Ref(id.clone()), at + 1 - lo));
                if t(at + 1) == Some("one") {
                    out.push((RG::Ref(id.clone()), at + 2 - lo));
                }
            }
        }
        out
    }

    /// Every reading of span `lo..hi` as a whole target list of at most 16 groups (a longer
    /// reading of the span is no target list; other spans keep theirs).
    fn targets(&self, lo: usize, hi: usize) -> Vec<Vec<RG>> {
        self.targets_within(lo, hi, 16)
    }

    /// Every reading of span `lo..hi` as a target list of at most `cap` groups.
    fn targets_within(&self, lo: usize, hi: usize, cap: usize) -> Vec<Vec<RG>> {
        let mut out = Vec::new();
        if lo >= hi || cap == 0 {
            return out;
        }
        for (g, n) in self.group(lo, hi) {
            let at = lo + n;
            if at == hi {
                out.push(vec![g.clone()]);
                continue;
            }
            let mut joins = Vec::new();
            if self.toks[at] == "," {
                joins.push(1);
                if at + 1 < hi && TJOIN_WORDS.contains(&self.toks[at + 1].as_str()) {
                    joins.push(2);
                }
            } else if TJOIN_WORDS.contains(&self.toks[at].as_str()) {
                joins.push(1);
            }
            for j in joins {
                for rest in self.targets_within(at + j, hi, cap - 1) {
                    let mut v = vec![g.clone()];
                    v.extend(rest);
                    out.push(v);
                }
            }
        }
        out
    }

    /// The modes span `lo..hi` reads as after the preposition: MODENAME (mode | modo), STRONG,
    /// and with `modo`, `modo` and an exact MODENAME.
    fn tail(&self, lo: usize, hi: usize, modo: bool) -> Vec<&'static str> {
        let s: Vec<&str> = self.toks[lo..hi].iter().map(String::as_str).collect();
        let mut out = Vec::new();
        if let Some((last, body)) = s.split_last() {
            if MODE_WORD.contains(last) {
                match body {
                    [a] => out.extend(r_mode1(a).or_else(|| r_typo(a))),
                    [a, b] => out.extend(r_mode2(a, b)),
                    _ => {}
                }
            }
        }
        match s.as_slice() {
            [a] => out.extend(r_mode1(a).filter(|m| !WEAK.contains(m))),
            [a, b] => out.extend(r_mode2(a, b)),
            _ => {}
        }
        if modo {
            match s.as_slice() {
                ["modo", a] => out.extend(r_mode1(a)),
                ["modo", a, b] => out.extend(r_mode2(a, b)),
                _ => {}
            }
        }
        out
    }

    /// Every reading of span `lo..hi` as one clause (`whole`: the span is the message).
    fn clause(&self, lo: usize, hi: usize, whole: bool) -> Vec<RClause> {
        let s: Vec<&str> = self.toks[lo..hi].iter().map(String::as_str).collect();
        let mut out = Vec::new();
        // ANSWER
        let aw = if s.len() >= 2 && s[0] == "go" && s[1] == "ahead" {
            Some((RAct::Yes("go ahead".into()), 2))
        } else if APPROVE_1.contains(&s[0]) {
            Some((RAct::Yes(s[0].into()), 1))
        } else if DENY_1.contains(&s[0]) {
            Some((RAct::No(s[0].into()), 1))
        } else {
            None
        };
        if let Some((act, n)) = aw {
            let alone = matches!(&act, RAct::No(w) if ONLY_ALONE.contains(&w.as_str()));
            let rest = &s[n..];
            if whole && (rest.is_empty() || (!alone && rest == ["it"])) {
                out.push(RClause {
                    act: act.clone(),
                    groups: Vec::new(),
                });
            }
            if !alone {
                let mut starts = vec![lo + n];
                if rest.first().is_some_and(|w| PREPS.contains(w)) {
                    starts.push(lo + n + 1);
                }
                for st in starts {
                    for groups in self.targets(st, hi) {
                        out.push(RClause {
                            act: act.clone(),
                            groups,
                        });
                    }
                }
            }
        }
        // MODE, English: from each "to".
        for p in lo..hi {
            if self.toks[p] != "to" {
                continue;
            }
            let mut m0 = p + 1;
            if m0 < hi && self.toks[m0] == "the" {
                m0 += 1;
            }
            let modes = self.tail(m0, hi, false);
            if modes.is_empty() {
                continue;
            }
            let mut starts = vec![lo];
            if VERBS_EN.contains(&s[0]) {
                starts.push(lo + 1);
            }
            for st in starts {
                for groups in self.targets(st, p) {
                    for m in &modes {
                        out.push(RClause {
                            act: RAct::Mode(m),
                            groups: groups.clone(),
                        });
                    }
                }
            }
        }
        // MODE, Spanish.
        if VERBS_ES.contains(&s[0]) {
            for p in lo + 1..hi {
                if !PREPS_ES.contains(&self.toks[p].as_str()) {
                    continue;
                }
                let modes = self.tail(p + 1, hi, true);
                for groups in self.targets(lo + 1, p) {
                    for m in &modes {
                        out.push(RClause {
                            act: RAct::Mode(m),
                            groups: groups.clone(),
                        });
                    }
                }
            }
        }
        out
    }

    /// The parses of the tokens from `lo` in at most `left` clauses: how many, and one.
    fn seq(&mut self, lo: usize, left: usize) -> (u64, Option<Vec<RClause>>) {
        let n = self.toks.len();
        if left == 0 || lo >= n {
            return (0, None);
        }
        if let Some(r) = self.memo.get(&(lo, left)) {
            return r.clone();
        }
        let (mut count, mut one) = (0u64, None);
        for hi in lo + 1..=n {
            let cl = self.clause(lo, hi, lo == 0 && hi == n);
            if cl.is_empty() {
                continue;
            }
            if hi == n {
                count += cl.len() as u64;
                one.get_or_insert_with(|| vec![cl[0].clone()]);
                continue;
            }
            let mut joins = Vec::new();
            if self.toks[hi] == "," {
                joins.push(1);
                if hi + 1 < n && JOIN_WORDS.contains(&self.toks[hi + 1].as_str()) {
                    joins.push(2);
                }
            } else if JOIN_WORDS.contains(&self.toks[hi].as_str()) {
                joins.push(1);
            }
            for j in joins {
                let (c, rest) = self.seq(hi + j, left - 1);
                count = count.saturating_add(c.saturating_mul(cl.len() as u64));
                if let Some(rest) = rest {
                    one.get_or_insert_with(|| {
                        let mut v = vec![cl[0].clone()];
                        v.extend(rest);
                        v
                    });
                }
            }
        }
        let r = (count.min(2), one);
        self.memo.insert((lo, left), r.clone());
        r
    }
}

/// How the reference parses `msg`: its count (2 for two or more) and one parse, or `None`
/// when it cannot be a command at all (marks, the byte and token caps).
fn r_parse(msg: &str, threads: &[ThreadInfo]) -> Option<(u64, Option<Vec<RClause>>)> {
    let toks = r_tokens(msg)?;
    let mut r = R::new(&toks, threads);
    Some(r.seq(0, 8))
}

struct World {
    threads: Vec<ThreadInfo>,
    waiting: Vec<Waiting>,
}

impl World {
    fn candidates(&self) -> Vec<&Waiting> {
        self.waiting
            .iter()
            .filter(|w| {
                !self
                    .threads
                    .iter()
                    .any(|t| t.id == w.thread && t.is_orchestrator)
            })
            .collect()
    }

    fn mode_candidates(&self) -> BTreeSet<String> {
        self.threads
            .iter()
            .filter(|t| !t.is_orchestrator && !t.stopped && t.id != HUB_THREAD)
            .map(|t| t.id.clone())
            .collect()
    }
}

/// What a reference clause covers, or `None` when a group picks out nothing.
fn r_covers(c: &RClause, w: &World) -> Option<BTreeSet<String>> {
    let answer = !matches!(c.act, RAct::Mode(_));
    let all: BTreeSet<String> = if answer {
        w.candidates().iter().map(|i| i.attn_id.clone()).collect()
    } else {
        w.mode_candidates()
    };
    if c.groups.is_empty() {
        return (all.len() == 1).then_some(all);
    }
    let mut out = BTreeSet::new();
    for g in &c.groups {
        let got: BTreeSet<String> = match g {
            RG::Two if all.len() == 2 => all.clone(),
            RG::Every if !all.is_empty() => all.clone(),
            RG::Two | RG::Every => return None,
            RG::Ref(t) if answer => {
                let items: BTreeSet<String> = w
                    .candidates()
                    .iter()
                    .filter(|i| i.thread == *t)
                    .map(|i| i.attn_id.clone())
                    .collect();
                if items.len() != 1 {
                    return None;
                }
                items
            }
            RG::Ref(t) => {
                let ok =
                    t != HUB_THREAD && w.threads.iter().any(|x| x.id == *t && !x.is_orchestrator);
                if !ok {
                    return None;
                }
                BTreeSet::from([t.clone()])
            }
        };
        out.extend(got);
    }
    Some(out)
}

/// The reference reading of a message: each target with what every action covering it says.
#[derive(Default, Debug)]
struct Reading {
    command: bool,
    /// A lone answer: never direct.
    lone: bool,
    said: BTreeMap<String, Vec<Option<String>>>,
}

fn polarity(label: &str) -> Option<Reply> {
    let t = label.trim();
    if t.len() > 64 || t.split_whitespace().count() > 6 {
        return None;
    }
    match t.to_lowercase().as_str() {
        "allow" => Some(Reply::Allow),
        "deny" => Some(Reply::Deny),
        _ => label_polarity(t),
    }
}

/// The label an answer gives an item, if exactly one: only permissions take answers.
fn r_give(act: &RAct, item: &Waiting) -> Option<String> {
    if item.reason != AttentionReason::Permission {
        return None;
    }
    let (word, want) = match act {
        RAct::Yes(w) => (w.as_str(), Reply::Allow),
        RAct::No(w) => (w.as_str(), Reply::Deny),
        RAct::Mode(_) => return None,
    };
    if item.options.is_empty() {
        return Some(
            if want == Reply::Allow {
                "allow"
            } else {
                "deny"
            }
            .into(),
        );
    }
    let distinct = |v: Vec<&String>| -> Option<String> {
        let s: BTreeSet<&String> = v.into_iter().collect();
        (s.len() == 1).then(|| (*s.iter().next().unwrap()).clone())
    };
    let spelled: Vec<&String> = item
        .options
        .iter()
        .filter(|o| o.trim().len() <= 16 && norm(o.trim()) == word)
        .collect();
    if !spelled.is_empty() {
        let l = distinct(spelled)?;
        return match polarity(&l) {
            Some(p) if p != want => None,
            _ => Some(l),
        };
    }
    distinct(
        item.options
            .iter()
            .filter(|o| polarity(o) == Some(want))
            .collect(),
    )
}

fn r_read(msg: &str, w: &World) -> Reading {
    let mut out = Reading::default();
    let Some((1, Some(parse))) = r_parse(msg, &w.threads) else {
        return out;
    };
    let mut acts = Vec::new();
    for c in &parse {
        match r_covers(c, w) {
            Some(t) => acts.push((c.act.clone(), t)),
            None => return out,
        }
    }
    out.command = true;
    out.lone = parse.len() == 1 && parse[0].groups.is_empty();
    for (act, targets) in acts {
        for t in targets {
            let say = match &act {
                RAct::Mode(m) => Some(m.to_string()),
                _ => w
                    .waiting
                    .iter()
                    .find(|i| i.attn_id == t)
                    .and_then(|i| r_give(&act, i)),
            };
            out.said.entry(t).or_default().push(say);
        }
    }
    out
}

fn r_verdict(said: Option<&Vec<Option<String>>>, want: &str) -> Coverage {
    match said {
        None => Coverage::Uncovered,
        Some(v) if v.is_empty() => Coverage::Uncovered,
        Some(v) if v.iter().all(|s| s.as_deref() == Some(want)) => Coverage::Direct,
        Some(_) => Coverage::Conflict,
    }
}

/// [`r_verdict`] for an answer: a lone answer is never direct.
fn r_answer_verdict(reading: &Reading, attn: &str, want: &str) -> Coverage {
    match r_verdict(reading.said.get(attn), want) {
        Coverage::Direct if reading.lone => Coverage::Lone,
        v => v,
    }
}

/// The mode a generic ACP agent records for a call's mode, as the hub canonicalizes it.
fn hub_canonical(raw: &str) -> String {
    let t = raw.trim();
    let key = |m: &str| -> String {
        m.chars()
            .filter(|c| !matches!(c, '-' | '_' | ' '))
            .flat_map(char::to_lowercase)
            .collect()
    };
    MODE_IDS
        .iter()
        .find(|id| key(id) == key(t))
        .map_or_else(|| t.to_string(), |id| id.to_string())
}

// ---------------------------------------------------------------------------------------------
// Worlds
// ---------------------------------------------------------------------------------------------

/// Worker names (split at whitespace).
const NAMES: &str = "\
    api-1 web-2 orbit-webapp-1 atlas-1 webapp-admin-1 billing-svc-3 api web db cache frontend \
    all auto yes both plan Api never-1 only-api now nobody-1 no-one-1 ninguno agents falla \
    pasa orchestrator-worker orch-api planner-2 2 12 1-2 api.v2 cheers thx ty pls cool-api \
    thanks-bot conditionally conditionally-1 tentatively either either-1 whichever maybe-api \
    luego quizás-1";
const ALIASES: &[&str] = &[
    "pw", "w", "dupe", "front", "7", "thx", "cheers", "either", "later",
];
const PERMISSION_SETS: &[&[&str]] = &[
    &["allow", "deny"],
    &["allow", "deny"],
    &["allow", "deny"],
    &[],
    &["deny"],
    &["Yes", "Yes, and don't ask again", "No"],
    &["Allow once", "Allow always", "Deny"],
];
const QUESTION_SETS: &[&[&str]] = &[
    &["Yes", "No"],
    &["Use Postgres", "Use SQLite"],
    &["allow api", "deny api"],
    &["Allow all", "Cancel"],
    &["1", "2"],
];

fn thread(id: &str, name: &str) -> ThreadInfo {
    ThreadInfo {
        id: id.into(),
        name: name.into(),
        aliases: Vec::new(),
        former_names: Vec::new(),
        project: None,
        status: ThreadStatus::Idle,
        runtime: "fake".into(),
        is_orchestrator: false,
        stopped: false,
        top_attention: None,
        last_report: None,
    }
}

fn world(rng: &mut Rng) -> World {
    let mut o = thread("o0", "orchestrator");
    o.aliases = vec!["orch".into()];
    o.is_orchestrator = true;
    let mut threads = vec![o];
    if rng.chance(10) {
        let mut r = thread("o1", "planner");
        r.is_orchestrator = true;
        r.stopped = true;
        threads.push(r);
    }
    if rng.chance(8) {
        threads.push(thread(HUB_THREAD, "hub"));
    }
    let n = 1 + rng.below(4);
    let all: Vec<&str> = NAMES.split_whitespace().collect();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    while names.len() < n {
        names.insert(rng.pick(&all));
    }
    for (i, name) in names.into_iter().enumerate() {
        let mut t = thread(&format!("id{i}x"), name);
        if rng.chance(15) {
            t.aliases = vec![(*rng.pick(ALIASES)).into()];
        }
        if rng.chance(15) {
            t.former_names = vec![(*rng.pick(&all)).into()];
        }
        t.stopped = rng.chance(12);
        threads.push(t);
    }
    let mut waiting = Vec::new();
    let mut k = 0;
    for t in threads.clone() {
        let items = if t.is_orchestrator {
            usize::from(rng.chance(10))
        } else {
            [0, 1, 1, 1, 2][rng.below(5)]
        };
        for _ in 0..items {
            let (reason, options) = if rng.chance(78) {
                (AttentionReason::Permission, *rng.pick(PERMISSION_SETS))
            } else {
                (AttentionReason::Question, *rng.pick(QUESTION_SETS))
            };
            waiting.push(Waiting {
                attn_id: format!("a{k}"),
                thread: t.id.clone(),
                reason,
                options: options.iter().map(|o| o.to_string()).collect(),
            });
            k += 1;
        }
    }
    World { threads, waiting }
}

// ---------------------------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Valid,
    Breaks(&'static str),
    Maybe(&'static str),
    Soup,
}

fn case(rng: &mut Rng, s: &str) -> String {
    match rng.below(9) {
        0 => s.to_uppercase(),
        1 => {
            let mut c = s.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
        _ => s.to_string(),
    }
}

struct G<'w> {
    w: &'w World,
    /// Workers: every thread but an orchestrator.
    workers: Vec<&'w ThreadInfo>,
    /// Threads with exactly one answer candidate.
    single: Vec<&'w ThreadInfo>,
}

impl<'w> G<'w> {
    fn new(w: &'w World) -> Self {
        let workers: Vec<&ThreadInfo> = w.threads.iter().filter(|t| !t.is_orchestrator).collect();
        let cands = w.candidates();
        let single = workers
            .iter()
            .copied()
            .filter(|t| cands.iter().filter(|i| i.thread == t.id).count() == 1)
            .collect();
        G { w, workers, single }
    }

    /// A token naming `t`: a form the reference reads as `t`, else `@` and its id.
    fn ref_form(&self, rng: &mut Rng, t: &ThreadInfo) -> String {
        let name = t.name.clone();
        let parts: Vec<String> = norm(&name)
            .split(['-', '_'])
            .filter(|p| p.chars().count() >= 3)
            .map(String::from)
            .collect();
        let form = match rng.below(8) {
            0 | 1 => name.clone(),
            2 => format!("@{name}"),
            3 => t.aliases.first().cloned().unwrap_or(name),
            4 => t.id.clone(),
            5 => parts.first().cloned().unwrap_or(name),
            6 => t.former_names.first().cloned().unwrap_or(name),
            _ => name.to_uppercase(),
        };
        if r_ref(&norm(&form), &self.w.threads).as_deref() == Some(t.id.as_str()) {
            form
        } else {
            format!("@{}", t.id)
        }
    }

    fn targets(&self, rng: &mut Rng, pool: &[&ThreadInfo], v: &mut Vec<String>) {
        let n = [1, 1, 1, 2, 2, 3][rng.below(6)];
        for i in 0..n {
            if i > 0 {
                let j = *rng.pick(&[",", "and", "y", "&", ", and", ", y", ", &"]);
                v.extend(j.split(' ').map(String::from));
            }
            if pool.is_empty() || rng.chance(30) {
                let q = *rng.pick(&[
                    "both",
                    "ambos",
                    "ambas",
                    "them both",
                    "all",
                    "todos",
                    "todas",
                    "every",
                    "everything",
                    "them all",
                ]);
                v.extend(q.split(' ').map(String::from));
            } else {
                if rng.chance(15) {
                    v.push(rng.pick(ARTICLE).to_string());
                }
                let t = *rng.pick(pool);
                v.push(self.ref_form(rng, t));
                if rng.chance(15) {
                    v.push("one".into());
                }
            }
        }
    }

    fn mode_spelling(rng: &mut Rng, strong: bool) -> String {
        let (_, ws) = loop {
            let m = rng.pick(R_MODES);
            if !strong || !WEAK.contains(&m.0) {
                break *m;
            }
        };
        if ws.len() == 2 && rng.chance(50) {
            ws.join(" ")
        } else {
            ws.join(rng.pick(&["", "-", "_"]))
        }
    }

    fn clause(&self, rng: &mut Rng, v: &mut Vec<String>) {
        if rng.chance(55) {
            if rng.chance(70) {
                v.extend(rng.pick(APPROVE_1).split(' ').map(String::from));
                if rng.chance(8) {
                    v.truncate(v.len() - 1);
                    v.extend(["go", "ahead"].map(String::from));
                }
            } else {
                v.push(
                    rng.pick(&["deny", "reject", "decline", "block", "rechaza", "deniega"])
                        .to_string(),
                );
            }
            if rng.chance(20) {
                v.push(rng.pick(PREPS).to_string());
            }
            let pool = if self.single.is_empty() || rng.chance(15) {
                self.workers.clone()
            } else {
                self.single.clone()
            };
            self.targets(rng, &pool, v);
        } else if rng.chance(75) {
            if rng.chance(75) {
                v.push(rng.pick(VERBS_EN).to_string());
            }
            self.targets(rng, &self.workers, v);
            v.push("to".into());
            if rng.chance(20) {
                v.push("the".into());
            }
            match rng.below(5) {
                0 => v.push(Self::mode_spelling(rng, true)),
                1 => {
                    v.push(rng.pick(TYPOS).0.into());
                    v.push(rng.pick(MODE_WORD).to_string());
                }
                _ => {
                    v.push(Self::mode_spelling(rng, false));
                    v.push(rng.pick(MODE_WORD).to_string());
                }
            }
        } else {
            v.push(rng.pick(VERBS_ES).to_string());
            self.targets(rng, &self.workers, v);
            v.push(rng.pick(PREPS_ES).to_string());
            match rng.below(4) {
                0 => {
                    v.push("modo".into());
                    v.push(Self::mode_spelling(rng, false));
                }
                1 => v.push(Self::mode_spelling(rng, true)),
                2 => {
                    v.push(rng.pick(TYPOS).0.into());
                    v.push("modo".into());
                }
                _ => {
                    v.push(Self::mode_spelling(rng, false));
                    v.push(rng.pick(MODE_WORD).to_string());
                }
            }
        }
    }

    /// A valid message, as words (`,` its own word).
    fn valid(&self, rng: &mut Rng) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        if rng.chance(10) {
            let w = *rng.pick(&[
                "yes", "allow", "ok", "go ahead", "deny", "no", "nope", "yep", "sí",
            ]);
            v.extend(w.split(' ').map(String::from));
            if !ONLY_ALONE.contains(&w) && rng.chance(40) {
                v.push("it".into());
            }
            return v;
        }
        let n = [1, 1, 1, 2, 2, 3, 4][rng.below(7)];
        for i in 0..n {
            if i > 0 {
                let j = *rng.pick(&[",", "and", "y", ", and", ", y"]);
                v.extend(j.split(' ').map(String::from));
            }
            self.clause(rng, &mut v);
        }
        v
    }
}

/// Words joined into a message: commas glued to the word before now and then, case changed,
/// a trailing mark and spaces around now and then.
fn render(rng: &mut Rng, v: &[String]) -> String {
    let mut s = String::new();
    for (i, w) in v.iter().enumerate() {
        if i > 0 && !(w == "," && rng.chance(60)) {
            s.push(' ');
        }
        s.push_str(&case(rng, w));
    }
    if rng.chance(20) {
        s.push(*rng.pick(&['.', '!']));
    }
    if rng.chance(8) {
        s = format!("  {s}\n");
    }
    s
}

/// A sure-breaking mutation of `v`.
fn breaks(rng: &mut Rng, v: &[String], g: &G) -> (String, &'static str) {
    let mut v = v.to_vec();
    let at = rng.below(v.len());
    let quant = v
        .iter()
        .position(|w| TWO.contains(&w.as_str()) || EVERY.contains(&w.as_str()));
    let refs: Vec<usize> = (0..v.len())
        .filter(|&i| r_ref(&norm(&v[i]), &g.w.threads).is_some())
        .collect();
    let kind = *rng.pick(&[
        "question",
        "semicolon",
        "colon",
        "sentence",
        "line",
        "courtesy",
        "noun",
        "si",
        "quoted",
        "typo",
    ]);
    let kind = match kind {
        "noun" if quant.is_none() => "question",
        "quoted" | "typo" if refs.is_empty() => "colon",
        "line" if v.len() < 2 => "semicolon",
        k => k,
    };
    match kind {
        "question" => v[at].push(*rng.pick(&['?', '¿', '？'])),
        "semicolon" => v[at].push(';'),
        "colon" => v[at].push(':'),
        "sentence" => {
            let s = render(rng, &v);
            let s = s.trim().trim_end_matches(['.', '!']);
            let tail = *rng.pick(&["thanks", "Then summarize.", "ok", "Why?"]);
            return (format!("{s}. {tail}"), kind);
        }
        "line" => {
            let k = 1 + rng.below(v.len() - 1);
            let head = v[..k].join(" ");
            let rest = v[k..].join(" ");
            let br = *rng.pick(&["\n", "\r\n", "\n\n", "\u{2028}"]);
            return (format!("{head}{br}{rest}"), kind);
        }
        "courtesy" => match rng.below(3) {
            0 => v.insert(0, rng.pick(&["please", "thanks", "por favor"]).to_string()),
            1 => v.push(
                rng.pick(&["please", "thanks", "gracias", "cheers", "thx"])
                    .to_string(),
            ),
            _ => v.extend([
                ",".to_string(),
                rng.pick(&[
                    "thanks",
                    "cheers",
                    "thx",
                    "ty",
                    "pls",
                    "conditionally",
                    "tentatively",
                    "either",
                ])
                .to_string(),
            ]),
        },
        "noun" => v.insert(
            quant.unwrap() + 1,
            rng.pick(&["requests", "agents", "permissions", "threads"])
                .to_string(),
        ),
        "si" => v.insert(0, "si".into()),
        "quoted" => {
            let i = *rng.pick(&refs);
            let (a, b) = *rng.pick(&[("'", "'"), ("\"", "\""), ("(", ")"), ("*", "*")]);
            v[i] = format!("{a}{}{b}", v[i]);
        }
        _ => {
            let i = *rng.pick(&refs);
            let (pre, body) = match v[i].strip_prefix('@') {
                Some(b) => ("@", b.to_string()),
                None => ("", v[i].clone()),
            };
            let mut c: Vec<char> = body.chars().collect();
            if c.len() >= 2 && rng.chance(60) {
                let k = rng.below(c.len() - 1);
                c.swap(k, k + 1);
            } else {
                c.insert(rng.below(c.len() + 1), 'q');
            }
            let m: String = c.into_iter().collect();
            let tok = norm(&format!("{pre}{m}"));
            if r_ref(&tok, &g.w.threads).is_some()
                || r_stop(&tok)
                || r_mode1(&tok).is_some()
                || r_typo(&tok).is_some()
            {
                v[at].push('?');
                return (render(rng, &v), "question");
            }
            v[i] = format!("{pre}{m}");
        }
    }
    (render(rng, &v), kind)
}

/// A mutation of `v` that may or may not break it.
fn maybe(rng: &mut Rng, v: &[String]) -> (String, &'static str) {
    let mut v = v.to_vec();
    let kind = *rng.pick(&["drop", "swap", "joiner", "verb"]);
    match kind {
        "drop" if v.len() > 1 => {
            v.remove(rng.below(v.len()));
        }
        "swap" if v.len() > 1 => {
            let k = rng.below(v.len() - 1);
            v.swap(k, k + 1);
        }
        "joiner" => {
            let js: Vec<usize> = (0..v.len())
                .filter(|&i| [",", "and", "y", "&"].contains(&v[i].as_str()))
                .collect();
            let to = rng
                .pick(&["and", ",", "or", "plus", "then", "&", "y", "also"])
                .to_string();
            match js.is_empty() {
                true => v.insert(rng.below(v.len() + 1), to),
                false => v[*rng.pick(&js)] = to,
            }
        }
        _ => {
            let to = rng
                .pick(&[
                    "set", "use", "make", "move", "put", "switch", "pon", "pasa", "turn",
                ])
                .to_string();
            match v
                .iter()
                .position(|w| VERBS_EN.contains(&w.as_str()) || VERBS_ES.contains(&w.as_str()))
            {
                Some(i) => v[i] = to,
                None => v.insert(0, to),
            }
        }
    }
    (render(rng, &v), kind)
}

fn soup(rng: &mut Rng, g: &G) -> String {
    const POOL: &[&str] = &[
        "allow",
        "yes",
        "no",
        "deny",
        "go",
        "ahead",
        "it",
        "both",
        "all",
        "them",
        "to",
        "the",
        "one",
        "and",
        "y",
        "&",
        ",",
        "set",
        "pon",
        "en",
        "a",
        "on",
        "auto",
        "plan",
        "mode",
        "modo",
        "read-only",
        "bypass",
        "audo",
        "si",
        "please",
        "requests",
        "?",
        ".",
        ":",
        "frob",
        "2",
        "'api'",
        "@orch",
        "orchestrator",
        "every",
        "todos",
    ];
    let mut v: Vec<String> = Vec::new();
    for _ in 0..1 + rng.below(8) {
        if rng.chance(25) && !g.workers.is_empty() {
            let t = *rng.pick(&g.workers);
            v.push(g.ref_form(rng, t));
        } else {
            v.push(rng.pick(POOL).to_string());
        }
    }
    render(rng, &v)
}

/// Whether word `w` (without its `@`) is one an orchestrator thread or the word
/// "orchestrator" matches: one of its names, aliases or ids, or a `-` / `_` part of its name of
/// 3+ characters.
fn orch_like(w: &str, threads: &[ThreadInfo]) -> bool {
    if w == "orchestrator" {
        return true;
    }
    threads.iter().filter(|t| t.is_orchestrator).any(|t| {
        let mut whole = vec![t.name.to_lowercase(), t.id.to_lowercase()];
        whole.extend(t.aliases.iter().map(|a| a.to_lowercase()));
        whole.iter().any(|x| x == w)
            || (w.len() >= 3 && t.name.to_lowercase().split(['-', '_']).any(|p| p == w))
    })
}

/// Text for a card's names, labels and summaries: pieces of the card's own syntax, escapes,
/// quotes, backslashes, control, format and invisible characters, combining marks, emoji.
fn card_junk(rng: &mut Rng) -> String {
    // Pieces split at `|`.
    const PIECES: &str = "plan|web|Use SQLite|\u{200B}|\u{202E}|\u{2060}|\u{FEFF}|\u{AD}|\u{A0}|\u{7}|\n|\t|\u{301}|e\u{301}|\u{FE0F}|👍|\"|\\|\\u{200B}| · |·| → |→| (|)|(|: \"|custom mode \"|allow |deny |set |answer |From your message: | |:";
    let pieces: Vec<&str> = PIECES.split('|').collect();
    (0..1 + rng.below(6)).map(|_| *rng.pick(&pieces)).collect()
}

/// Whether `c` shows nothing (or nothing like itself) on a terminal (listed here, not taken
/// from the card module).
fn invisible(c: char) -> bool {
    c.is_control()
        || matches!(c as u32,
            0xAD | 0x34F | 0x61C | 0x115F | 0x1160 | 0x17B4 | 0x17B5 | 0x180B..=0x180F
            | 0x2000..=0x200F | 0x2028..=0x202F | 0x205F..=0x206F | 0x3000 | 0x3164
            | 0xFE00..=0xFE0F | 0xFEFF | 0xFFA0 | 0xFFF9..=0xFFFB | 0xE0000..=0xE0FFF | 0xA0)
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Stats {
    calls: u64,
    direct_answers: u64,
    direct_modes: u64,
    proposals: u64,
    conflicts: u64,
    question_conflicts: u64,
    refused: u64,
    confirmed: u64,
    valid: u64,
    valid_commands: u64,
    limited: u64,
    ambiguous: u64,
    soup: u64,
    breaks: BTreeMap<&'static str, u64>,
    maybe: BTreeMap<&'static str, u64>,
    mode_tricks: u64,
    mode_tricks_kept: u64,
    cards: u64,
    direct_checks: u64,
    direct_commands: u64,
    direct_hub_answers: u64,
    direct_refused: u64,
}

#[derive(Debug, Clone)]
enum Call {
    Answer(String, String),
    Mode(String, String),
}

fn fuzz(seed: u64, cases: u64) -> Stats {
    let mut rng = Rng(seed | 1);
    let mut st = Stats::default();
    for case_no in 0..cases {
        let w = world(&mut rng);
        let g = G::new(&w);
        let words = g.valid(&mut rng);
        let (msg, kind) = match rng.below(100) {
            0..=54 => (render(&mut rng, &words), Kind::Valid),
            55..=69 => {
                let (m, k) = breaks(&mut rng, &words, &g);
                (m, Kind::Breaks(k))
            }
            70..=84 => {
                let (m, k) = maybe(&mut rng, &words);
                (m, Kind::Maybe(k))
            }
            _ => (soup(&mut rng, &g), Kind::Soup),
        };
        let fail = |what: String| -> ! {
            panic!(
                "command invariant violated (TWR_FUZZ_SEED={seed:#x}, case {case_no}): {what}\n\
                 message: {msg:?} ({kind:?})\nthreads: {:?}\nwaiting: {:?}",
                w.threads
                    .iter()
                    .map(|t| (&t.id, &t.name, &t.aliases, t.stopped, t.is_orchestrator))
                    .collect::<Vec<_>>(),
                w.waiting
            )
        };
        // (b) The generator and the reference agree on the grammar.
        let parsed = r_parse(&msg, &w.threads);
        match kind {
            Kind::Valid => {
                st.valid += 1;
                match parsed {
                    None => st.limited += 1,
                    Some((0, _)) => fail("valid message does not parse".into()),
                    Some((1, _)) => st.valid_commands += 1,
                    Some(_) => st.ambiguous += 1,
                }
            }
            Kind::Breaks(k) => {
                *st.breaks.entry(k).or_default() += 1;
                if parsed.is_some_and(|(c, _)| c > 0) {
                    fail(format!("{k} mutation still parses"));
                }
            }
            Kind::Maybe(k) => *st.maybe.entry(k).or_default() += 1,
            Kind::Soup => st.soup += 1,
        }
        let reading = r_read(&msg, &w);
        let scene = Scene {
            message: &msg,
            threads: &w.threads,
            waiting: &w.waiting,
        };
        // (f) The user's fast path: all or nothing, exactly what the per-item verdicts allow.
        let complete = reading.command
            && !reading.lone
            && reading.said.values().all(|v| {
                v.first()
                    .is_some_and(|f| f.is_some() && v.iter().all(|s| s == f))
            });
        match direct_actions(&scene) {
            Some(actions) => {
                st.direct_commands += 1;
                if !reading.command {
                    fail(format!("direct_actions {actions:?} outside the grammar"));
                }
                if actions.is_empty() {
                    fail("direct_actions returned no action".into());
                }
                let mut keys = BTreeSet::new();
                for a in &actions {
                    let key = match a {
                        DirectAction::Answer { attn_id, label } => {
                            let Some(item) = w.waiting.iter().find(|i| i.attn_id == *attn_id)
                            else {
                                fail(format!("direct answer to unknown {attn_id}"));
                            };
                            if item.reason != AttentionReason::Permission {
                                fail(format!("direct answer {a:?} to a question"));
                            }
                            if w.threads
                                .iter()
                                .any(|t| t.id == item.thread && t.is_orchestrator)
                            {
                                fail(format!("direct answer {a:?} to an orchestrator's item"));
                            }
                            // The hub thread's own items never are permissions; the hub refuses
                            // them anyway (`hub::commands`). Counted, not failed.
                            if item.thread == HUB_THREAD {
                                st.direct_hub_answers += 1;
                            }
                            if canonical_label(item, label).map(|(l, _)| l).as_deref()
                                != Some(label.as_str())
                            {
                                fail(format!("{a:?}: {label:?} is no canonical label"));
                            }
                            let v = answer_coverage(&scene, attn_id, label);
                            if v != Coverage::Direct {
                                fail(format!("{a:?} but answer_coverage says {v:?}"));
                            }
                            attn_id.clone()
                        }
                        DirectAction::Mode { thread, mode } => {
                            if !MODE_IDS.contains(&mode.as_str()) {
                                fail(format!("{a:?}: not a mode id"));
                            }
                            if thread == HUB_THREAD
                                || !w
                                    .threads
                                    .iter()
                                    .any(|t| t.id == *thread && !t.is_orchestrator)
                            {
                                fail(format!("{a:?}: not a worker"));
                            }
                            let v = mode_coverage(&scene, thread, mode);
                            if v != Coverage::Direct {
                                fail(format!("{a:?} but mode_coverage says {v:?}"));
                            }
                            thread.clone()
                        }
                    };
                    if !keys.insert(key.clone()) {
                        fail(format!("{key} acted on twice in {actions:?}"));
                    }
                }
                let want: BTreeSet<String> = reading.said.keys().cloned().collect();
                if keys != want {
                    fail(format!(
                        "direct_actions covers {keys:?}, the reference {want:?} (all or nothing)"
                    ));
                }
                if !complete {
                    fail(format!(
                        "direct_actions {actions:?} though a target is uncovered or in conflict: \
                         {:?}",
                        reading.said
                    ));
                }
            }
            None => {
                if reading.command {
                    st.direct_refused += 1;
                }
                if complete {
                    fail(format!(
                        "direct_actions refused a complete command: {:?}",
                        reading.said
                    ));
                }
            }
        }
        let hinted = |rng: &mut Rng, key: &str| -> Option<String> {
            let said: Vec<&String> = reading.said.get(key)?.iter().flatten().collect();
            (!said.is_empty() && rng.chance(92)).then(|| rng.pick(&said).to_string())
        };
        let mut calls = Vec::new();
        for item in &w.waiting {
            let options: Vec<String> = if item.options.is_empty() {
                vec!["allow".into(), "deny".into()]
            } else {
                item.options.clone()
            };
            let pick =
                hinted(&mut rng, &item.attn_id).unwrap_or_else(|| rng.pick(&options).clone());
            let Some((label, _)) = canonical_label(item, &pick) else {
                fail(format!("{pick:?} is no label of {}", item.attn_id));
            };
            calls.push(Call::Answer(item.attn_id.clone(), label));
        }
        for t in &g.workers {
            let id = match hinted(&mut rng, &t.id) {
                Some(k) => MODE_IDS.iter().copied().find(|m| *m == k).unwrap_or("auto"),
                None => *rng.pick(MODE_IDS),
            };
            let mode = if rng.chance(25) {
                st.mode_tricks += 1;
                let mut c: Vec<char> = id.chars().collect();
                let at = 1 + rng.below(c.len() - 1);
                match rng.below(9) {
                    0 => c.insert(at, '\''),
                    1 => c.insert(at, '\u{2019}'),
                    2 => c.insert(at, '-'),
                    3 => c.insert(at, '_'),
                    4 => c.insert(at, '\u{200b}'),
                    5 => c = id.to_uppercase().chars().collect(),
                    6 => c.push(' '),
                    7 => c.insert(0, ' '),
                    _ => c.extend(" mode".chars()),
                }
                let m = hub_canonical(&c.into_iter().collect::<String>());
                if !MODE_IDS.contains(&m.as_str()) {
                    st.mode_tricks_kept += 1;
                }
                m
            } else {
                id.to_string()
            };
            calls.push(Call::Mode(t.id.clone(), mode));
        }
        // (a), (e) and the hub model (c).
        let mut applied: Vec<(String, String)> = Vec::new();
        let mut proposals: Vec<(String, String)> = Vec::new();
        let mut covered: BTreeSet<(String, String)> = BTreeSet::new();
        let mut refused: BTreeSet<(String, String)> = BTreeSet::new();
        for call in calls {
            st.calls += 1;
            let (key, v, want, is_answer) = match &call {
                Call::Answer(attn, label) => (
                    (attn.clone(), label.clone()),
                    answer_coverage(&scene, attn, label),
                    r_answer_verdict(&reading, attn, label),
                    true,
                ),
                Call::Mode(thread, mode) => {
                    let v = mode_coverage(&scene, thread, mode);
                    if v == Coverage::Direct && !MODE_IDS.contains(&mode.as_str()) {
                        fail(format!("mode {thread} {mode:?} accepted directly"));
                    }
                    (
                        (thread.clone(), mode.clone()),
                        v,
                        r_verdict(reading.said.get(thread), mode),
                        false,
                    )
                }
            };
            if v != want {
                fail(format!(
                    "{key:?}: module {v:?}, reference {want:?} (command {}, said {:?})",
                    reading.command, reading.said
                ));
            }
            if v == Coverage::Direct {
                st.direct_checks += 1;
                let mut t = msg.trim();
                if t.ends_with(['.', '!']) {
                    t = &t[..t.len() - 1];
                }
                if t.contains(['.', '!', '?', '¿', ';', ':', '\n', '\r']) {
                    fail(format!("{key:?} direct for a message of two sentences"));
                }
                if t.replace(',', " , ").split_whitespace().count() > 48 {
                    fail(format!(
                        "{key:?} direct for a message of more than 48 tokens"
                    ));
                }
                if let Some(i) = w.waiting.iter().find(|i| i.attn_id == key.0) {
                    if i.reason != AttentionReason::Permission {
                        fail(format!("{key:?} answered a question directly"));
                    }
                }
                for tok in norm(t).replace(',', " ").split_whitespace() {
                    if r_ref(tok, &w.threads).is_some()
                        && orch_like(tok.trim_start_matches('@'), &w.threads)
                    {
                        fail(format!(
                            "{tok:?} names a worker though an orchestrator matches"
                        ));
                    }
                }
            }
            if v == Coverage::Conflict
                && w.waiting
                    .iter()
                    .any(|i| i.attn_id == key.0 && i.reason == AttentionReason::Question)
            {
                st.question_conflicts += 1;
            }
            // (c): provenance fails now and then (refused, never proposed nor applied).
            if rng.chance(10) {
                st.refused += 1;
                refused.insert(key);
                continue;
            }
            match v {
                Coverage::Direct => {
                    if is_answer {
                        st.direct_answers += 1;
                    } else {
                        st.direct_modes += 1;
                    }
                    covered.insert(key.clone());
                    applied.push(key);
                }
                Coverage::Conflict | Coverage::Uncovered | Coverage::Lone => {
                    if v == Coverage::Conflict {
                        st.conflicts += 1;
                    }
                    st.proposals += 1;
                    proposals.push(key);
                }
            }
        }
        // (d) The card for the proposals renders losslessly and visibly.
        if !proposals.is_empty() {
            st.cards += 1;
            let name = |id: &str| {
                w.threads
                    .iter()
                    .find(|t| t.id == id)
                    .map_or(id.to_string(), |t| t.name.clone())
            };
            let lines: Vec<CardLine> = proposals
                .iter()
                .map(|(key, value)| {
                    let item = w.waiting.iter().find(|i| i.attn_id == *key);
                    let mut thread = item.map_or_else(|| name(key), |i| name(&i.thread));
                    if rng.chance(20) {
                        thread = card_junk(&mut rng);
                    }
                    if item.is_some() {
                        CardLine::Answer {
                            thread,
                            answer: if rng.chance(20) {
                                card_junk(&mut rng)
                            } else {
                                value.clone()
                            },
                            item: match rng.below(3) {
                                0 => None,
                                1 => Some(format!("{key}?")),
                                _ => Some(card_junk(&mut rng)),
                            },
                        }
                    } else {
                        let mode = if rng.chance(15) {
                            card_junk(&mut rng)
                        } else {
                            value.clone()
                        };
                        CardLine::Mode {
                            thread,
                            custom: !MODE_IDS.contains(&mode.as_str()),
                            mode,
                        }
                    }
                })
                .collect();
            let summary = card::render(&lines);
            if let Some(c) = summary.chars().find(|&c| invisible(c)) {
                fail(format!("card {summary:?} shows {c:?} as is"));
            }
            if card::parse(&summary).as_ref() != Some(&lines) {
                fail(format!(
                    "card {summary:?} reads back as {:?}, not {lines:?}",
                    card::parse(&summary)
                ));
            }
        }
        let confirm = rng.chance(50);
        if confirm && !proposals.is_empty() {
            st.confirmed += 1;
            applied.extend(proposals.iter().cloned());
        }
        for a in &applied {
            if refused.contains(a) {
                fail(format!("{a:?} failed provenance but was applied"));
            }
            if !covered.contains(a) && !(confirm && proposals.contains(a)) {
                fail(format!(
                    "{a:?} applied without a covering command or a Confirm"
                ));
            }
        }
    }
    st
}

fn env_u64(name: &str) -> Option<u64> {
    let v = std::env::var(name).ok()?;
    match v.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => v.parse().ok(),
    }
}

fn run_fuzz(default_cases: u64) {
    for (t, id) in TYPOS {
        assert_eq!(r_typo(t), Some(*id), "{t}");
    }
    let seed = env_u64("TWR_FUZZ_SEED").unwrap_or(DEFAULT_SEED);
    let cases = env_u64("TWR_FUZZ_CASES").unwrap_or(default_cases);
    eprintln!("command fuzz: TWR_FUZZ_SEED={seed:#x} TWR_FUZZ_CASES={cases}");
    let st = fuzz(seed, cases);
    eprintln!("command fuzz: {st:?}");
    // The generator must reach the interesting paths, or the fuzz proves little.
    assert!(st.direct_answers * 40 > st.calls, "too few direct answers");
    assert!(
        st.direct_modes * 40 > st.calls,
        "too few direct mode changes"
    );
    assert!(st.proposals * 2 > st.calls, "too few proposals");
    assert!(st.conflicts * 40 > st.calls, "too few conflicts");
    let mut reached: Vec<(u64, String)> = vec![
        (st.valid_commands, "valid messages read as a command".into()),
        (st.soup, "soup messages".into()),
        (
            st.question_conflicts,
            "questions covered (a conflict)".into(),
        ),
        (
            st.mode_tricks_kept,
            "mode ids the hub keeps as written".into(),
        ),
        (st.cards, "confirm cards read back".into()),
        (st.direct_checks, "direct calls checked".into()),
        (st.direct_commands, "direct user commands".into()),
        (
            st.direct_refused,
            "commands the fast path leaves to the orchestrator".into(),
        ),
        (st.confirmed, "confirmed cards".into()),
        (st.refused, "provenance refusals".into()),
    ];
    for k in [
        "question",
        "semicolon",
        "colon",
        "sentence",
        "line",
        "courtesy",
        "noun",
        "si",
        "quoted",
        "typo",
    ] {
        reached.push((
            st.breaks.get(k).copied().unwrap_or(0),
            format!("{k} mutations"),
        ));
    }
    for k in ["drop", "swap", "joiner", "verb"] {
        reached.push((
            st.maybe.get(k).copied().unwrap_or(0),
            format!("{k} mutations"),
        ));
    }
    for (n, what) in reached {
        assert!(n * 1000 > cases, "too few {what}: {n}");
    }
}

/// Fixed cases the module and the reference must read alike: courtesy and qualifier words as
/// thread names, aliases and name parts, and the group cap at its boundary.
#[test]
fn fixed_cases_agree_with_the_reference() {
    let mut threads = vec![thread("o0", "orchestrator"), thread("ta", "api")];
    threads[0].is_orchestrator = true;
    threads.push(thread("tc", "cheers"));
    let mut t = thread("tt", "zeta-7");
    t.aliases = vec!["thx".into()];
    threads.push(t);
    threads.push(thread("tq", "conditionally-1"));
    threads.push(thread("tr", "either-1"));
    let mut t = thread("ts", "zeta-8");
    t.aliases = vec!["tentatively".into()];
    threads.push(t);
    for i in 0..17 {
        threads.push(thread(&format!("tw{i}"), &format!("w{i}")));
    }
    let waiting: Vec<Waiting> = threads
        .iter()
        .skip(1)
        .enumerate()
        .map(|(k, t)| Waiting {
            attn_id: format!("a{k}"),
            thread: t.id.clone(),
            reason: AttentionReason::Permission,
            options: vec!["allow".into(), "deny".into()],
        })
        .collect();
    let w = World { threads, waiting };
    let ws = |n: usize, sep: &str| {
        (0..n)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(sep)
    };
    let messages = [
        ("allow api, cheers".to_string(), false),
        ("allow api, thx".to_string(), false),
        ("allow api, @cheers".to_string(), true),
        ("allow api, @thx".to_string(), true),
        ("allow api, conditionally".to_string(), false),
        ("allow api, tentatively".to_string(), false),
        ("set either to plan mode".to_string(), false),
        ("allow api, @conditionally-1".to_string(), true),
        ("allow api, @tentatively".to_string(), true),
        ("set @either-1 to plan mode".to_string(), true),
        (format!("allow {}, w0 to plan mode", ws(16, " & ")), true),
        (format!("allow {}, set w0 to plan mode", ws(16, ", ")), true),
        (format!("allow {}", ws(16, " & ")), true),
        (format!("allow {}", ws(17, " & ")), false),
        // Two parses (the mode clause may start at any comma): no command.
        (format!("allow {}, w16 to plan mode", ws(16, ", ")), false),
    ];
    for (msg, command) in &messages {
        let reading = r_read(msg, &w);
        assert_eq!(reading.command, *command, "{msg:?}");
        let scene = Scene {
            message: msg,
            threads: &w.threads,
            waiting: &w.waiting,
        };
        for item in &w.waiting {
            for label in ["allow", "deny"] {
                assert_eq!(
                    answer_coverage(&scene, &item.attn_id, label),
                    r_answer_verdict(&reading, &item.attn_id, label),
                    "{msg:?} {} {label}",
                    item.attn_id
                );
            }
        }
        for t in w.threads.iter().skip(1) {
            for mode in ["plan", "auto"] {
                assert_eq!(
                    mode_coverage(&scene, &t.id, mode),
                    r_verdict(reading.said.get(&t.id), mode),
                    "{msg:?} {} {mode}",
                    t.id
                );
            }
        }
    }
}

#[test]
fn command_invariants() {
    run_fuzz(DEFAULT_CASES);
}

#[test]
#[ignore = "long run: cargo test -p twr-decisions --release --test command_fuzz -- --ignored"]
fn command_invariants_long() {
    run_fuzz(1_000_000);
}

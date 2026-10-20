//! Accounts: several logins of one runtime, each a config directory the runtime's variable
//! selects (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`).
//!
//! Every runtime that takes accounts ([`ACCOUNT_RUNTIMES`]) always has its native account:
//! the runtime's own default directory, with no variable set (today's behaviour). It is named
//! [`NATIVE_DEFAULT_NAME`] until something names it. A thread records `None` for the native
//! account, so it follows the native whatever its name is now.
//!
//! Accounts come from two sources: `[accounts.<runtime>.<name>]` in `config.toml`
//! ([`crate::config::AccountConfig`]) and twr's records, the `account.added` /
//! `account.removed` events on the hub thread (`twr account add` / `remove`). Config wins: an
//! account in both keeps the config's directory, share level and native flag. In both means a
//! declaration and a live record of the same runtime and name, whatever kind each gives it (a
//! named record config declares native, a native one config declares named): its source is
//! [`Source::Both`], and `twr account remove` of it removes the record, of the kind it is
//! ([`AccountsView::live_record`]), and keeps the config's account. A record removed so stays
//! removed: when config drops the declaration later, nothing comes back (no named account, no
//! native name), and the native account is not removed either.
//!
//! Two entries of a runtime can share a name: the native account that fell back to
//! [`NATIVE_DEFAULT_NAME`] (config declared its recorded name as a named account) next to a
//! named `default`, or a removed native account next to a live named one of its name. The name
//! stands for the named account then ([`AccountsView::resolve`]); the native entry says so
//! ([`AccountInfo::shadowed`]), and lookups by name skip it ([`AccountInfo::answers_to`]).
//! `account.status` events keep each account's last known login state. [`AccountsView`] folds
//! the config and those events; the hub and every client build it the same way, so they agree
//! on when a runtime shows account labels ([`AccountsView::shows_labels`]).
//!
//! Pins ([`Pin`]) say which accounts a project's (or a thread's) threads may use: names and
//! globs (`*`, `?`), in order.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::event::{Event, EventBody, HUB_THREAD};

/// The runtimes that take accounts. `[runtimes]` presets come in a later version.
pub const ACCOUNT_RUNTIMES: &[&str] = &["claude-code", "codex", "grok-build"];

/// Whether `runtime` takes accounts ([`ACCOUNT_RUNTIMES`]).
pub fn takes_accounts(runtime: &str) -> bool {
    ACCOUNT_RUNTIMES.contains(&runtime)
}

/// The native account's name until config or a record names it.
pub const NATIVE_DEFAULT_NAME: &str = "default";

/// Longest account name, in characters.
pub const MAX_NAME_LEN: usize = 64;

/// Checks account name `name`: 1 to [`MAX_NAME_LEN`] letters, digits, `-`, `_` or `.`, the
/// first a letter or digit (so neither `.`, `..` nor `-x`). The error says what is allowed.
pub fn check_name(name: &str) -> Result<(), String> {
    check_chars(name, false)
}

/// [`check_name`], with `*` and `?` allowed anywhere (a pin's glob).
fn check_pattern(item: &str) -> Result<(), String> {
    check_chars(item, true)
}

fn check_chars(s: &str, glob: bool) -> Result<(), String> {
    let wild = |c: char| glob && matches!(c, '*' | '?');
    let ok = !s.is_empty()
        && s.chars().count() <= MAX_NAME_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') || wild(c))
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || wild(c));
    if ok {
        Ok(())
    } else if glob {
        Err(format!(
            "{s:?} is not an account name or glob: 1 to {MAX_NAME_LEN} letters, digits, '-', \
             '_', '.', '*' or '?', starting with a letter, a digit or a wildcard"
        ))
    } else {
        Err(format!(
            "{s:?} is not an account name: 1 to {MAX_NAME_LEN} letters, digits, '-', '_' or \
             '.', starting with a letter or digit"
        ))
    }
}

/// Splits an account reference: `claude-code:work` is (`claude-code`, `Some("work")`). A spec
/// without `:`, an `acp:` or `pty:` spec, or one whose part before the first `:` is not a
/// plain runtime name (letters, digits, `-`, `_`, `.`) is (`spec`, `None`). The account part is
/// returned as written (it may be empty): the caller checks it ([`check_name`]).
pub fn split_runtime(spec: &str) -> (&str, Option<&str>) {
    let Some((runtime, account)) = spec.split_once(':') else {
        return (spec, None);
    };
    let plain = !runtime.is_empty()
        && runtime
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !plain || runtime == "acp" || runtime == "pty" {
        return (spec, None);
    }
    (runtime, Some(account))
}

/// A pin: the accounts a thread may use, in order: account names and globs (`*` any run of
/// characters, `?` one). Several items, or a glob that matches several accounts, make a pool.
/// Written as a string (`"work"`, or a comma-separated list `"work-a,work-b"` as `--account`
/// and MCP take it) or a list of strings (`["work-a", "max-*"]`); serialized as a string when
/// it has one item, else as a list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Pin(pub Vec<String>);

impl Pin {
    /// Parses a comma-separated list (`work-a,work-b`): items trimmed, empty ones dropped.
    /// Not checked: [`Pin::validate`].
    pub fn parse(s: &str) -> Pin {
        Pin(s
            .split(',')
            .map(str::trim)
            .filter(|i| !i.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Whether any item matches account `name`.
    pub fn matches(&self, name: &str) -> bool {
        self.0.iter().any(|item| glob_match(item, name))
    }

    /// The names of `names` the pin matches, in pin order: an item's own name where it
    /// stands, a glob's matches in name order, each name once.
    pub fn order(&self, names: &[&str]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for item in &self.0 {
            let mut hits: Vec<&str> = names
                .iter()
                .copied()
                .filter(|n| glob_match(item, n))
                .collect();
            hits.sort_unstable();
            for h in hits {
                if !out.iter().any(|o| o == h) {
                    out.push(h.to_string());
                }
            }
        }
        out
    }

    /// Checks every item: an account name ([`check_name`]), `*` and `?` allowed. An empty pin
    /// is an error.
    pub fn validate(&self) -> Result<(), String> {
        if self.0.is_empty() {
            return Err("a pin names at least one account".into());
        }
        self.0.iter().try_for_each(|i| check_pattern(i))
    }
}

impl From<&str> for Pin {
    /// [`Pin::parse`].
    fn from(s: &str) -> Pin {
        Pin::parse(s)
    }
}

impl fmt::Display for Pin {
    /// The items, comma-separated (what [`Pin::parse`] reads back).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(","))
    }
}

impl Serialize for Pin {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self.0.as_slice() {
            [one] => s.serialize_str(one),
            items => items.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for Pin {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct PinVisitor;
        impl<'de> Visitor<'de> for PinVisitor {
            type Value = Pin;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an account name or glob, or a list of them")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Pin, E> {
                Ok(Pin::parse(v))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Pin, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<String>()? {
                    items.push(item.trim().to_string());
                }
                Ok(Pin(items))
            }
        }
        d.deserialize_any(PinVisitor)
    }
}

/// Whether `name` matches `pattern` (`*` any run of characters, `?` one; anything else
/// itself).
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    // The latest `*` and where in `name` it started matching.
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ni));
            pi += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// What a named account shares with the native one (links into the native directory).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Share {
    /// Nothing: the account's directory is its own.
    None,
    /// The user's settings, instructions, skills and the like (the design's per-runtime
    /// tables); never credentials or sessions.
    #[default]
    Config,
}

/// An account's login state, as its last status check found it (`account.status`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum Login {
    /// The runtime's status command said it is signed in.
    LoggedIn,
    /// It said it is not.
    LoggedOut,
    /// The account's directory does not exist.
    Missing,
    /// Not known: the runtime has no status command (Grok Build), or it could not be read.
    /// Also any value a newer twr records that this build does not know.
    #[default]
    #[serde(other)]
    Unknown,
}

impl Login {
    /// Whether an account in this state counts towards showing labels
    /// ([`AccountsView::shows_labels`]): anything but logged out or missing.
    pub fn counts(self) -> bool {
        !matches!(self, Login::LoggedOut | Login::Missing)
    }
}

/// Where an account is declared.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// `config.toml` only.
    Config,
    /// twr's records only (`twr account add`).
    Recorded,
    /// Both; the config's declaration is the one that counts.
    Both,
    /// Neither: the native account that nothing names (it is [`NATIVE_DEFAULT_NAME`]).
    Native,
}

/// One account of a runtime as [`AccountsView`] sees it.
#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct AccountInfo {
    /// The runtime.
    pub runtime: String,
    /// The account's name (the native account's current name for the native one).
    pub name: String,
    /// Whether it is the native account (the runtime's own default directory).
    pub native: bool,
    /// Its directory: `None` for the native account, and for a named account declared
    /// without one while the view has no default root ([`AccountsView::with_default_root`]).
    pub dir: Option<PathBuf>,
    /// What it shares with the native account.
    pub share: Share,
    /// Where it is declared.
    pub source: Source,
    /// Its last recorded login state; `None`: never checked.
    pub login: Option<Login>,
    /// How it is signed in, as the runtime says (`claude.ai`, `api-key`, ...).
    pub method: Option<String>,
    /// Why the state is what it is, when the check said (`unknown` mostly).
    pub note: Option<String>,
    /// When that state was recorded.
    pub checked_at: Option<DateTime<Utc>>,
    /// Removed (`twr account remove`): threads on it cannot start until they move.
    pub removed: bool,
    /// The native account whose name a named account that is not removed has too (the native
    /// one fell back to [`NATIVE_DEFAULT_NAME`] next to a named `default`, say): the name
    /// stands for that one ([`AccountsView::resolve`]), so a lookup by name skips this entry
    /// ([`AccountInfo::answers_to`]). Written only when true.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub shadowed: bool,
}

impl AccountInfo {
    /// Whether name `name` stands for this account, as [`AccountsView::resolve`] has it: its
    /// name, unless it is the native account and a live named account has that name too
    /// ([`AccountInfo::shadowed`]). What every lookup of a name over a runtime's accounts
    /// (`account.list`'s) matches with, so two entries with one name resolve to one.
    pub fn answers_to(&self, name: &str) -> bool {
        self.name == name && !self.shadowed
    }
}

/// A declaration's directory and share level.
#[derive(Debug, Clone, PartialEq)]
struct Decl {
    dir: Option<PathBuf>,
    share: Share,
}

/// A recorded account (`account.added`), named or the native one's name.
#[derive(Debug, Clone, PartialEq)]
struct Record {
    decl: Decl,
    removed: bool,
}

/// An `account.status`.
#[derive(Debug, Clone, PartialEq)]
struct Status {
    login: Login,
    method: Option<String>,
    note: Option<String>,
    checked_at: DateTime<Utc>,
}

/// Every account per runtime: the config's declarations and twr's records, folded from the
/// control stream (`account.added`, `account.removed`, `account.status` on the hub thread).
/// The hub keeps one (replayed at start, then live), and so does every client.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountsView {
    /// Config's named accounts: runtime -> name -> declaration.
    config: BTreeMap<String, BTreeMap<String, Decl>>,
    /// Config's native account per runtime: its name and declaration.
    config_native: BTreeMap<String, (String, Decl)>,
    /// Recorded named accounts: runtime -> name -> record (removed ones kept, flagged).
    recorded: BTreeMap<String, BTreeMap<String, Record>>,
    /// The latest recorded name of each runtime's native account.
    recorded_native: BTreeMap<String, (String, Record)>,
    /// Runtimes whose native account was removed (a record; a config native never is).
    native_removed: BTreeSet<String>,
    /// The native account's last status, per runtime (whatever its name was then).
    native_status: BTreeMap<String, Status>,
    /// Named accounts' last status, by (runtime, name).
    status: BTreeMap<(String, String), Status>,
    /// Where named accounts declared without a directory live: `<root>/<runtime>/<name>`.
    default_root: Option<PathBuf>,
}

impl AccountsView {
    /// The view of `config`'s declarations (`[accounts.<runtime>.<name>]`, checked and with
    /// `~` expanded by [`Config::from_toml_str`]), before any record.
    pub fn new(config: &Config) -> Self {
        let mut v = AccountsView::default();
        for (rt, names) in &config.accounts {
            for (name, a) in names {
                let decl = Decl {
                    dir: a.dir.clone(),
                    share: a.share,
                };
                if a.native {
                    v.config_native
                        .entry(rt.clone())
                        .or_insert((name.clone(), decl));
                } else {
                    v.config
                        .entry(rt.clone())
                        .or_default()
                        .insert(name.clone(), decl);
                }
            }
        }
        v
    }

    /// The same view, with named accounts that have no directory of their own placed under
    /// `root` (`<root>/<runtime>/<name>`; [`crate::paths::Paths::accounts_dir`]).
    pub fn with_default_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.default_root = Some(root.into());
        self
    }

    /// Folds one event: `account.added`, `account.removed` and `account.status` on the hub
    /// thread; anything else is ignored.
    pub fn apply(&mut self, ev: &Event) {
        if ev.thread != HUB_THREAD {
            return;
        }
        match &ev.body {
            EventBody::AccountAdded(p) => {
                if p.runtime.is_empty() || p.account.is_empty() {
                    return;
                }
                let record = Record {
                    decl: Decl {
                        dir: p.dir.as_ref().map(PathBuf::from),
                        share: p.share,
                    },
                    removed: false,
                };
                if p.native {
                    self.recorded_native
                        .insert(p.runtime.clone(), (p.account.clone(), record));
                    self.native_removed.remove(&p.runtime);
                } else {
                    self.recorded
                        .entry(p.runtime.clone())
                        .or_default()
                        .insert(p.account.clone(), record);
                }
            }
            EventBody::AccountRemoved(p) => {
                if p.native {
                    // A removal that is not `record_only` removed the native login, not a
                    // name: it holds whatever the native account is called when this is
                    // replayed (config may since have given its name to a named account). One
                    // that is `record_only` removed only the record of an account config
                    // declared: never the native account itself, whatever config says when
                    // this is replayed. Config's native declaration (`native = true`) wins
                    // over either, as it wins over every record: that account stays.
                    if let Some((name, r)) = self.recorded_native.get_mut(&p.runtime) {
                        if *name == p.account {
                            r.removed = true;
                        }
                    }
                    if !p.record_only && !self.config_native.contains_key(&p.runtime) {
                        self.native_removed.insert(p.runtime.clone());
                    }
                } else if let Some(r) = self
                    .recorded
                    .get_mut(&p.runtime)
                    .and_then(|m| m.get_mut(&p.account))
                {
                    r.removed = true;
                }
            }
            EventBody::AccountStatus(p) => {
                if p.runtime.is_empty() || p.account.is_empty() {
                    return;
                }
                let status = Status {
                    login: p.login,
                    method: p.method.clone(),
                    note: p.note.clone(),
                    checked_at: ev.ts,
                };
                if p.native || self.native_owns(&p.runtime, &p.account) {
                    self.native_status.insert(p.runtime.clone(), status);
                } else {
                    self.status
                        .insert((p.runtime.clone(), p.account.clone()), status);
                }
            }
            _ => {}
        }
    }

    /// The name of `runtime`'s native account: config's native name, else the latest
    /// recorded one (unless config declares that name as another account), else
    /// [`NATIVE_DEFAULT_NAME`]. A removed record still names the native account while the
    /// native account is removed (it keeps its name, so it does not take another account's);
    /// one removed while the account stayed (config declared it then: only twr's record went)
    /// names nothing, so a config that drops the declaration later brings no name back.
    pub fn native_name(&self, runtime: &str) -> String {
        if let Some((name, _)) = self.config_native.get(runtime) {
            return name.clone();
        }
        if let Some((name, r)) = self.recorded_native.get(runtime) {
            let taken = self
                .config
                .get(runtime)
                .is_some_and(|m| m.contains_key(name));
            let gone = r.removed && !self.native_removed.contains(runtime);
            if !taken && !gone {
                return name.clone();
            }
        }
        NATIVE_DEFAULT_NAME.to_string()
    }

    /// Checks config's named accounts called [`NATIVE_DEFAULT_NAME`] against the native
    /// account's name with the records folded in: that is the native account's name until
    /// config (`native = true` on another account) or a record (`twr account name-native`)
    /// names it, so a named `default` of a runtime whose native account is still `default`
    /// is refused, saying how to fix it. Config alone can't decide it (a recorded name frees
    /// `default`), so [`Config::validate`] leaves it to this, at the hub's start.
    pub fn check_config_default(&self) -> Result<(), String> {
        for (rt, names) in &self.config {
            if !names.contains_key(NATIVE_DEFAULT_NAME)
                || self.native_name(rt) != NATIVE_DEFAULT_NAME
            {
                continue;
            }
            // A recorded name config declares as a named account names the native one no
            // more (it falls back to `default`).
            let taken = match self.recorded_native.get(rt) {
                Some((n, _)) if names.contains_key(n) => format!(
                    " (config.toml declares {n}, the name twr recorded for it, as a named \
                     account)"
                ),
                _ => String::new(),
            };
            return Err(format!(
                "[accounts.{rt}.{NATIVE_DEFAULT_NAME}]: {NATIVE_DEFAULT_NAME:?} is {rt}'s \
                 native account's name until something names that account{taken}: set \
                 `native = true` here or on another [accounts.{rt}] account, pick another name, \
                 or start the hub without this one and name the native account first (twr \
                 account name-native {rt} <name>)"
            ));
        }
        Ok(())
    }

    /// Whether twr has a live record (not removed) of `runtime`'s account `name`, of either
    /// kind: `Some(true)` a record of the native account under that name, `Some(false)` one
    /// of a named account; both, the one of kind `prefer_native`. A record's existence goes
    /// by its runtime and name whatever kind config gives the account: config wins over its
    /// kind as over its directory ([`Source::Both`]), and `twr account remove` removes the
    /// record of the kind it is.
    pub fn live_record(&self, runtime: &str, name: &str, prefer_native: bool) -> Option<bool> {
        let native = self
            .recorded_native
            .get(runtime)
            .is_some_and(|(n, r)| n == name && !r.removed);
        let named = self
            .recorded
            .get(runtime)
            .and_then(|m| m.get(name))
            .is_some_and(|r| !r.removed);
        match (native, named) {
            (true, true) => Some(prefer_native),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }

    /// Whether `name` of `runtime` stands for the native account: its name
    /// ([`AccountsView::native_name`]), unless a named account that is not removed has it too
    /// (only config's native name wins over one). A live named account keeps its name from a
    /// removed native account and from the default name.
    fn native_owns(&self, runtime: &str, name: &str) -> bool {
        name == self.native_name(runtime)
            && (self.config_native.contains_key(runtime)
                || self.named_info(runtime, name).is_none_or(|a| a.removed))
    }

    /// `runtime`'s accounts that are not removed: the native one first, then the named ones
    /// by name ([`AccountsView::accounts_with`]).
    pub fn accounts(&self, runtime: &str) -> Vec<AccountInfo> {
        self.accounts_with(runtime, false)
    }

    /// `runtime`'s accounts, the native one first, then the named ones by name; removed ones
    /// (flagged [`AccountInfo::removed`]) only with `include_removed`.
    pub fn accounts_with(&self, runtime: &str, include_removed: bool) -> Vec<AccountInfo> {
        let native = self.native_info(runtime);
        let mut out: Vec<AccountInfo> = Vec::new();
        out.extend(Some(native).filter(|n| include_removed || !n.removed));
        let names: BTreeSet<&String> = self
            .config
            .get(runtime)
            .into_iter()
            .flat_map(|m| m.keys())
            .chain(
                self.recorded
                    .get(runtime)
                    .into_iter()
                    .flat_map(|m| m.keys()),
            )
            .filter(|n| !self.native_owns(runtime, n))
            .collect();
        out.extend(
            names
                .into_iter()
                .filter_map(|n| self.named_info(runtime, n))
                .filter(|a| include_removed || !a.removed),
        );
        out
    }

    /// What `name` of `runtime` stands for: `Some(None)` the native account (its current
    /// name), `Some(Some(name))` a named account that is not removed, `None` an unknown or
    /// removed one (the native one included: its name, kept, names nothing then).
    pub fn resolve(&self, runtime: &str, name: &str) -> Option<Option<String>> {
        if self.native_owns(runtime, name) {
            return (!self.native_removed.contains(runtime)).then_some(None);
        }
        let info = self.named_info(runtime, name)?;
        (!info.removed).then(|| Some(name.to_string()))
    }

    /// The account `account` of `runtime` (`None`: the native one), removed or not; `None`
    /// when it is unknown. A name the native account has is the native account (unless a
    /// live named account has it too: [`AccountsView::native_owns`]).
    pub fn info(&self, runtime: &str, account: Option<&str>) -> Option<AccountInfo> {
        match account {
            None => Some(self.native_info(runtime)),
            Some(name) if self.native_owns(runtime, name) => Some(self.native_info(runtime)),
            Some(name) => self.named_info(runtime, name),
        }
    }

    /// Whether `runtime` shows account labels: it has two or more accounts (the native one
    /// included) that are not removed and whose last login state counts ([`Login::counts`];
    /// never checked counts).
    pub fn shows_labels(&self, runtime: &str) -> bool {
        self.accounts(runtime)
            .iter()
            .filter(|a| a.login.is_none_or(Login::counts))
            .count()
            >= 2
    }

    /// How the UI writes `runtime` and `account` (`None`: the native account): `runtime`
    /// alone while it does not show labels ([`AccountsView::shows_labels`]), else
    /// `runtime:<name>` (the native account under its current name).
    pub fn label(&self, runtime: &str, account: Option<&str>) -> String {
        if !self.shows_labels(runtime) {
            return runtime.to_string();
        }
        match account {
            Some(name) => format!("{runtime}:{name}"),
            None => format!("{runtime}:{}", self.native_name(runtime)),
        }
    }

    /// Every runtime with an account declared, recorded or checked, in name order.
    pub fn runtimes(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self
            .config
            .keys()
            .chain(self.config_native.keys())
            .chain(self.recorded.keys())
            .chain(self.recorded_native.keys())
            .chain(self.native_removed.iter())
            .chain(self.native_status.keys())
            .chain(self.status.keys().map(|(rt, _)| rt))
            .collect();
        set.into_iter().cloned().collect()
    }

    /// The view as JSON: `{<runtime>: {"shown": bool, "native": <name>, "accounts":
    /// [AccountInfo...]}}` for every runtime of [`AccountsView::runtimes`], removed accounts
    /// included; without `with_dirs`, no account has a `dir` key.
    pub fn to_json(&self, with_dirs: bool) -> serde_json::Value {
        let mut out = serde_json::Map::new();
        for rt in self.runtimes() {
            let accounts: Vec<serde_json::Value> = self
                .accounts_with(&rt, true)
                .iter()
                .map(|a| {
                    let mut v = serde_json::to_value(a).unwrap_or_default();
                    if !with_dirs {
                        if let Some(o) = v.as_object_mut() {
                            o.remove("dir");
                        }
                    }
                    v
                })
                .collect();
            out.insert(
                rt.clone(),
                serde_json::json!({
                    "shown": self.shows_labels(&rt),
                    "native": self.native_name(&rt),
                    "accounts": accounts,
                }),
            );
        }
        serde_json::Value::Object(out)
    }

    /// The native account of `runtime`.
    fn native_info(&self, runtime: &str) -> AccountInfo {
        let name = self.native_name(runtime);
        let recorded = self
            .recorded_native
            .get(runtime)
            .filter(|(n, _)| *n == name);
        // As for a named account: a removed record of a configured one leaves config's. A
        // live record of its name is config's too whatever its kind (a named account's that
        // config declares native: [`AccountsView::live_record`]).
        let (share, source) = match (self.config_native.get(runtime), recorded) {
            (Some((_, d)), _) if self.live_record(runtime, &name, true).is_some() => {
                (d.share, Source::Both)
            }
            (Some((_, d)), _) => (d.share, Source::Config),
            (None, Some((_, r))) => (r.decl.share, Source::Recorded),
            (None, None) => (Share::default(), Source::Native),
        };
        let status = self.native_status.get(runtime);
        AccountInfo {
            runtime: runtime.to_string(),
            native: true,
            dir: None,
            share,
            source,
            login: status.map(|s| s.login),
            method: status.and_then(|s| s.method.clone()),
            note: status.and_then(|s| s.note.clone()),
            checked_at: status.map(|s| s.checked_at),
            removed: self.native_removed.contains(runtime),
            shadowed: !self.native_owns(runtime, &name),
            name,
        }
    }

    /// Named account `name` of `runtime` (not the native one), removed or not.
    fn named_info(&self, runtime: &str, name: &str) -> Option<AccountInfo> {
        let config = self.config.get(runtime).and_then(|m| m.get(name));
        let record = self.recorded.get(runtime).and_then(|m| m.get(name));
        // A live record of its name is config's too whatever its kind (the native account's
        // that config declares named: [`AccountsView::live_record`]).
        let (decl, source, removed) = match (config, record) {
            (Some(c), _) if self.live_record(runtime, name, false).is_some() => {
                (c, Source::Both, false)
            }
            (Some(c), _) => (c, Source::Config, false),
            (None, Some(r)) => (&r.decl, Source::Recorded, r.removed),
            (None, None) => return None,
        };
        let dir = decl.dir.clone().or_else(|| {
            self.default_root
                .as_deref()
                .map(|root| default_dir(root, runtime, name))
        });
        let status = self.status.get(&(runtime.to_string(), name.to_string()));
        Some(AccountInfo {
            runtime: runtime.to_string(),
            name: name.to_string(),
            native: false,
            dir,
            share: decl.share,
            source,
            login: status.map(|s| s.login),
            method: status.and_then(|s| s.method.clone()),
            note: status.and_then(|s| s.note.clone()),
            checked_at: status.map(|s| s.checked_at),
            removed,
            shadowed: false,
        })
    }
}

/// The directory of named account `name` of `runtime` declared without one:
/// `<root>/<runtime>/<name>` (`root`: [`crate::paths::Paths::accounts_dir`]).
pub fn default_dir(root: &Path, runtime: &str, name: &str) -> PathBuf {
    root.join(runtime).join(name)
}

/// Account directory `dir` of `runtime`, one twr places under the accounts root `root`
/// (creates, links into or deletes), as it really is: `<real root>/<runtime>/<name>`, checked
/// on resolved paths, so that no link on the way (`<root>/<runtime>` a symlink to elsewhere,
/// or `dir` itself one) leads twr outside `root`. `dir`'s parent must resolve to the resolved
/// `<root>/<runtime>`, and `dir` must not be a symlink (it need not exist). `make_parent`
/// makes `<root>/<runtime>` first when it is missing (mode 700). An error says why, naming
/// `dir`.
pub fn resolve_in_root(
    root: &Path,
    runtime: &str,
    dir: &Path,
    make_parent: bool,
) -> Result<PathBuf, String> {
    let parent = root.join(runtime);
    let outside = || {
        format!(
            "{} is not inside {} (once links are followed)",
            dir.display(),
            parent.display()
        )
    };
    let name = match dir.file_name() {
        Some(n) if n != "." && n != ".." => n.to_owned(),
        _ => return Err(outside()),
    };
    if make_parent && std::fs::symlink_metadata(&parent).is_err() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)
            .map_err(|e| format!("making {}: {e}", parent.display()))?;
    }
    let resolve =
        |p: &Path| std::fs::canonicalize(p).map_err(|e| format!("resolving {}: {e}", p.display()));
    let real_parent = resolve(&parent)?;
    if real_parent != resolve(root)?.join(runtime) {
        return Err(outside());
    }
    let Some(dir_parent) = dir.parent() else {
        return Err(outside());
    };
    // (A parent that is not there is not the one that is.)
    if std::fs::canonicalize(dir_parent).ok().as_ref() != Some(&real_parent) {
        return Err(outside());
    }
    let real = real_parent.join(name);
    if std::fs::symlink_metadata(&real).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(format!("{} is a symlink", dir.display()));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AccountConfig;
    use crate::event::{
        AccountAdded, AccountRemoved, AccountStatus, ChangedBy, Source as EvSource,
    };
    use serde_json::json;

    #[test]
    fn account_names() {
        for ok in ["work", "a", "Work-2", "max_1.b", "9lives", &"x".repeat(64)] {
            assert_eq!(check_name(ok), Ok(()), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "-a",
            "_a",
            ".hidden",
            "a/b",
            "a:b",
            "a b",
            "a,b",
            "wörk",
            "a*",
            &"x".repeat(65),
        ] {
            let e = check_name(bad).unwrap_err();
            assert!(e.contains("letters, digits"), "{bad}: {e}");
        }
    }

    #[test]
    fn runtime_specs_split_only_account_references() {
        assert_eq!(
            split_runtime("claude-code:work"),
            ("claude-code", Some("work"))
        );
        assert_eq!(split_runtime("gemini:team"), ("gemini", Some("team")));
        assert_eq!(split_runtime("codex:"), ("codex", Some("")));
        for whole in [
            "claude-code",
            "acp:gemini --experimental-acp",
            "pty:bash",
            "acp:x:y",
            "my agent:x",
            ":work",
            "a/b:c",
        ] {
            assert_eq!(split_runtime(whole), (whole, None), "{whole}");
        }
    }

    #[test]
    fn pins_parse_from_strings_lists_and_commas() {
        assert_eq!(Pin::parse("work"), Pin(vec!["work".into()]));
        assert_eq!(
            Pin::parse(" work-a, work-b ,,"),
            Pin(vec!["work-a".into(), "work-b".into()])
        );
        assert_eq!(Pin::parse(""), Pin(vec![]));
        let p: Pin = serde_json::from_value(json!("a,b")).unwrap();
        assert_eq!(p, Pin(vec!["a".into(), "b".into()]));
        let p: Pin = serde_json::from_value(json!(["max-*", "b"])).unwrap();
        assert_eq!(p, Pin(vec!["max-*".into(), "b".into()]));
        assert!(serde_json::from_value::<Pin>(json!(7)).is_err());
        #[derive(Deserialize)]
        struct T {
            a: Pin,
            b: Pin,
        }
        let t: T = toml::from_str("a = \"work\"\nb = [\"x\", \"y-*\"]\n").unwrap();
        assert_eq!(t.a, Pin(vec!["work".into()]));
        assert_eq!(t.b, Pin(vec!["x".into(), "y-*".into()]));
        // One item goes back as a string, more as a list.
        assert_eq!(serde_json::to_value(&t.a).unwrap(), json!("work"));
        assert_eq!(serde_json::to_value(&t.b).unwrap(), json!(["x", "y-*"]));
        assert_eq!(t.b.to_string(), "x,y-*");
        assert_eq!(Pin::parse(&t.b.to_string()), t.b);
    }

    #[test]
    fn pins_match_and_order() {
        let p = Pin::parse("work,max-*");
        assert!(p.matches("work") && p.matches("max-1") && p.matches("max-"));
        assert!(!p.matches("works") && !p.matches("personal"));
        let q = Pin::parse("w?rk*");
        assert!(q.matches("work") && q.matches("wark-2") && !q.matches("wrk"));
        assert!(Pin::parse("*").matches("anything"));
        assert!(Pin::parse("*a*b").matches("xxaxxb") && !Pin::parse("*a*b").matches("xxbxa"));
        let names = ["max-2", "personal", "max-1", "work", "default"];
        assert_eq!(p.order(&names), ["work", "max-1", "max-2"]);
        // A name met again later is not repeated.
        assert_eq!(
            Pin::parse("max-2,max-*,*").order(&names),
            ["max-2", "max-1", "default", "personal", "work"]
        );
        assert!(Pin::parse("ghost").order(&names).is_empty());
    }

    #[test]
    fn pins_validate() {
        assert_eq!(Pin::parse("work,max-*,?x").validate(), Ok(()));
        assert!(Pin::parse("").validate().is_err());
        for bad in ["a/b", "..", "-x", "a b", ":x"] {
            assert!(Pin(vec![bad.into()]).validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn enums_serialize_in_snake_case() {
        assert_eq!(serde_json::to_value(Share::None).unwrap(), json!("none"));
        assert_eq!(Share::default(), Share::Config);
        assert_eq!(
            serde_json::to_value(Login::LoggedOut).unwrap(),
            json!("logged_out")
        );
        assert_eq!(
            serde_json::from_value::<Login>(json!("expired")).unwrap(),
            Login::Unknown
        );
        assert_eq!(serde_json::to_value(Source::Both).unwrap(), json!("both"));
        assert!(Login::LoggedIn.counts() && Login::Unknown.counts());
        assert!(!Login::LoggedOut.counts() && !Login::Missing.counts());
    }

    fn hub(body: EventBody) -> Event {
        Event::new(HUB_THREAD, 1, None, EvSource::Hub, body)
    }

    fn added(rt: &str, name: &str, dir: Option<&str>, native: bool) -> Event {
        hub(EventBody::AccountAdded(AccountAdded {
            runtime: rt.into(),
            account: name.into(),
            dir: dir.map(str::to_string),
            native,
            by: ChangedBy::User,
            ..Default::default()
        }))
    }

    fn removed(rt: &str, name: &str, native: bool) -> Event {
        hub(EventBody::AccountRemoved(AccountRemoved {
            runtime: rt.into(),
            account: name.into(),
            native,
            ..Default::default()
        }))
    }

    fn status(rt: &str, name: &str, login: Login) -> Event {
        hub(EventBody::AccountStatus(AccountStatus {
            runtime: rt.into(),
            account: name.into(),
            login,
            method: Some("claude.ai".into()),
            ..Default::default()
        }))
    }

    fn config(entries: &[(&str, &str, Option<&str>, bool)]) -> Config {
        let mut c = Config::default();
        for (rt, name, dir, native) in entries {
            c.accounts.entry(rt.to_string()).or_default().insert(
                name.to_string(),
                AccountConfig {
                    dir: dir.map(PathBuf::from),
                    share: Share::Config,
                    native: *native,
                },
            );
        }
        c
    }

    fn view(c: &Config, evs: &[Event]) -> AccountsView {
        let mut v = AccountsView::new(c);
        for e in evs {
            v.apply(e);
        }
        v
    }

    fn names(v: &AccountsView, rt: &str, removed: bool) -> Vec<String> {
        v.accounts_with(rt, removed)
            .into_iter()
            .map(|a| a.name)
            .collect()
    }

    #[test]
    fn config_wins_over_records() {
        let c = config(&[
            ("claude-code", "work", Some("/cfg/work"), false),
            ("claude-code", "personal", None, true),
        ]);
        let v = view(
            &c,
            &[
                added("claude-code", "work", Some("/rec/work"), false),
                added("claude-code", "side", Some("/rec/side"), false),
                added("claude-code", "me", None, true),
                // Another thread's events and other kinds are not accounts.
                Event::new(
                    "t1",
                    1,
                    None,
                    EvSource::Hub,
                    EventBody::AccountAdded(AccountAdded {
                        runtime: "claude-code".into(),
                        account: "ghost".into(),
                        ..Default::default()
                    }),
                ),
            ],
        );
        assert_eq!(v.native_name("claude-code"), "personal");
        assert_eq!(names(&v, "claude-code", true), ["personal", "side", "work"]);
        let work = v.info("claude-code", Some("work")).unwrap();
        assert_eq!(work.dir.as_deref(), Some(Path::new("/cfg/work")));
        assert_eq!(work.source, Source::Both);
        let side = v.info("claude-code", Some("side")).unwrap();
        assert_eq!(
            (side.source, side.dir.as_deref()),
            (Source::Recorded, Some(Path::new("/rec/side")))
        );
        let native = v.info("claude-code", None).unwrap();
        assert_eq!(
            (native.native, native.dir, native.source),
            (true, None, Source::Config)
        );
        // "me" lost to the config's native name: it names nothing.
        assert_eq!(v.resolve("claude-code", "me"), None);
        assert_eq!(v.resolve("claude-code", "personal"), Some(None));
        assert_eq!(v.resolve("claude-code", "work"), Some(Some("work".into())));
        assert_eq!(v.resolve("claude-code", "ghost"), None);
        // A config account whose record is removed stays, from config alone.
        let v2 = view(
            &c,
            &[
                added("claude-code", "work", Some("/rec/work"), false),
                removed("claude-code", "work", false),
            ],
        );
        let work = v2.info("claude-code", Some("work")).unwrap();
        assert_eq!((work.source, work.removed), (Source::Config, false));
    }

    /// Config's named `default` is refused while the native account is still `default` with
    /// the records folded in: a recorded name (`twr account name-native`) frees it, config's
    /// native one too; a recorded name config declares as a named account does not.
    #[test]
    fn a_config_default_needs_the_native_account_named() {
        let c = config(&[("codex", "default", Some("/d"), false)]);
        let e = view(&c, &[]).check_config_default().unwrap_err();
        assert!(
            e.starts_with(
                "[accounts.codex.default]: \"default\" is codex's native account's name until \
                 something names that account: set `native = true` here or on another \
                 [accounts.codex] account, pick another name, or start the hub without this \
                 one and name the native account first (twr account name-native codex <name>)"
            ),
            "{e}"
        );
        // Named by a record (replayed): fine.
        let v = view(&c, &[added("codex", "personal", None, true)]);
        assert_eq!(v.check_config_default(), Ok(()));
        assert_eq!(v.native_name("codex"), "personal");
        assert_eq!(v.resolve("codex", "default"), Some(Some("default".into())));
        // Named by config: fine. Other runtimes are not concerned.
        let named = config(&[
            ("codex", "default", Some("/d"), false),
            ("codex", "me", None, true),
            ("claude-code", "work", Some("/w"), false),
        ]);
        assert_eq!(view(&named, &[]).check_config_default(), Ok(()));
        // The recorded name taken by a config account: `default` again, refused.
        let taken = config(&[
            ("codex", "default", Some("/d"), false),
            ("codex", "personal", Some("/p"), false),
        ]);
        let e = view(&taken, &[added("codex", "personal", None, true)])
            .check_config_default()
            .unwrap_err();
        assert!(
            e.contains(
                "(config.toml declares personal, the name twr recorded for it, as a named \
                 account)"
            ),
            "{e}"
        );
    }

    #[test]
    fn native_names_and_records() {
        let c = Config::default();
        let v = view(&c, &[]);
        assert_eq!(v.native_name("codex"), NATIVE_DEFAULT_NAME);
        assert_eq!(v.info("codex", None).unwrap().source, Source::Native);
        assert_eq!(v.resolve("codex", "default"), Some(None));
        assert!(v.runtimes().is_empty());
        let v = view(
            &c,
            &[
                added("codex", "me", None, true),
                added("codex", "team", Some("/t"), false),
            ],
        );
        assert_eq!(v.native_name("codex"), "me");
        assert_eq!(v.info("codex", None).unwrap().source, Source::Recorded);
        assert!(v.info("codex", Some("me")).unwrap().native);
        assert_eq!(v.resolve("codex", "default"), None);
        assert_eq!(names(&v, "codex", false), ["me", "team"]);
        assert_eq!(v.runtimes(), ["codex"]);
        // The latest native name counts.
        let v = view(
            &c,
            &[
                added("codex", "me", None, true),
                added("codex", "home", None, true),
            ],
        );
        assert_eq!(v.native_name("codex"), "home");
        // A recorded native name a config account already has loses to it.
        let v = view(
            &config(&[("codex", "team", Some("/t"), false)]),
            &[added("codex", "team", None, true)],
        );
        assert_eq!(v.native_name("codex"), NATIVE_DEFAULT_NAME);
        assert!(!v.info("codex", Some("team")).unwrap().native);
        // Config native + the same name recorded: both.
        let v = view(
            &config(&[("codex", "me", None, true)]),
            &[added("codex", "me", None, true)],
        );
        assert_eq!(v.info("codex", None).unwrap().source, Source::Both);
    }

    #[test]
    fn removal_and_re_adding() {
        let c = Config::default();
        let mut v = view(&c, &[added("grok-build", "g1", Some("/g1"), false)]);
        v.apply(&removed("grok-build", "g1", false));
        assert_eq!(v.resolve("grok-build", "g1"), None);
        assert_eq!(names(&v, "grok-build", false), ["default"]);
        assert_eq!(names(&v, "grok-build", true), ["default", "g1"]);
        let g1 = v.info("grok-build", Some("g1")).unwrap();
        assert!(g1.removed);
        v.apply(&added("grok-build", "g1", Some("/g1b"), false));
        let g1 = v.info("grok-build", Some("g1")).unwrap();
        assert_eq!(
            (g1.removed, g1.dir.as_deref()),
            (false, Some(Path::new("/g1b")))
        );
        assert_eq!(v.resolve("grok-build", "g1"), Some(Some("g1".into())));
    }

    #[test]
    fn native_removal() {
        let c = Config::default();
        let mut v = view(
            &c,
            &[
                added("claude-code", "me", None, true),
                added("claude-code", "work", Some("/w"), false),
            ],
        );
        assert!(v.shows_labels("claude-code"));
        v.apply(&removed("claude-code", "me", true));
        let native = v.info("claude-code", None).unwrap();
        assert!(native.removed);
        assert_eq!(native.source, Source::Recorded);
        // It keeps its name while removed.
        assert_eq!(v.native_name("claude-code"), "me");
        assert!(v.info("claude-code", Some("me")).unwrap().native);
        assert_eq!(v.resolve("claude-code", "me"), None);
        assert_eq!(v.resolve("claude-code", "default"), None);
        assert_eq!(names(&v, "claude-code", false), ["work"]);
        assert_eq!(names(&v, "claude-code", true), ["me", "work"]);
        assert!(!v.shows_labels("claude-code"));
        v.apply(&added("claude-code", "me", None, true));
        assert!(!v.info("claude-code", None).unwrap().removed);
        assert_eq!(v.resolve("claude-code", "me"), Some(None));
        assert!(v.shows_labels("claude-code"));
        // An unnamed native removed under its default name.
        let mut v = view(&c, &[]);
        v.apply(&removed("codex", "default", true));
        assert_eq!(v.resolve("codex", "default"), None);
        // A config native is never removed by a record.
        let mut v = view(&config(&[("codex", "home", None, true)]), &[]);
        v.apply(&removed("codex", "home", true));
        assert_eq!(v.resolve("codex", "home"), Some(None));
    }

    /// The native account removed under its name (`home`, with force) stays removed when
    /// config later gives that name to a named account: the removal was of the native login,
    /// not of a name, so on replay it holds under whatever name the native account has then
    /// (`default`). Config's native declaration still wins over it.
    #[test]
    fn a_native_removal_survives_its_name_going_to_a_named_account() {
        let evs = [
            added("codex", "home", None, true),
            removed("codex", "home", true),
        ];
        let live = view(&Config::default(), &evs);
        assert!(live.info("codex", None).unwrap().removed);
        let c = config(&[("codex", "home", Some("/another/home"), false)]);
        let v = view(&c, &evs);
        let native = v.info("codex", None).unwrap();
        assert_eq!(
            (native.name.as_str(), native.removed),
            (NATIVE_DEFAULT_NAME, true)
        );
        assert_eq!(v.resolve("codex", NATIVE_DEFAULT_NAME), None);
        assert_eq!(v.resolve("codex", "home"), Some(Some("home".into())));
        assert_eq!(names(&v, "codex", false), ["home"]);
        // A later native `account.added` brings it back.
        let mut back = v.clone();
        back.apply(&added("codex", "personal", None, true));
        assert!(!back.info("codex", None).unwrap().removed);
        assert_eq!(back.resolve("codex", "personal"), Some(None));
        // Config declaring the native account wins: it stays.
        let c = config(&[("codex", "work", None, true)]);
        let v = view(&c, &evs);
        let native = v.info("codex", None).unwrap();
        assert_eq!((native.name.as_str(), native.removed), ("work", false));
        assert_eq!(v.resolve("codex", "work"), Some(None));
        // A record-only native removal still never removes it, whatever its name.
        let c = config(&[("codex", "home", Some("/another/home"), false)]);
        let v = view(
            &c,
            &[
                added("codex", "home", None, true),
                removed_record("codex", "home", true),
            ],
        );
        assert!(!v.info("codex", None).unwrap().removed);
    }

    /// A recorded native account that config declares later is both; once its record is
    /// removed it is config's alone (as a named account is), replayed too, until it is
    /// recorded again.
    #[test]
    fn a_configured_native_account_whose_record_is_removed_is_configs() {
        let early = [added("codex", "home", None, true)];
        let before = view(&Config::default(), &early);
        assert_eq!(before.info("codex", None).unwrap().source, Source::Recorded);
        let c = config(&[("codex", "home", None, true)]);
        let mut v = view(&c, &early);
        assert_eq!(v.info("codex", None).unwrap().source, Source::Both);
        let removal = removed("codex", "home", true);
        v.apply(&removal);
        let check = |v: &AccountsView| {
            let native = v.info("codex", None).unwrap();
            assert_eq!(
                (native.name.as_str(), native.source, native.removed),
                ("home", Source::Config, false)
            );
            assert_eq!(v.resolve("codex", "home"), Some(None));
        };
        check(&v);
        let replayed = view(&c, &[early[0].clone(), removal.clone()]);
        check(&replayed);
        assert_eq!(replayed, v);
        // Another removal changes nothing.
        v.apply(&removal);
        check(&v);
        v.apply(&added("codex", "home", None, true));
        assert_eq!(v.info("codex", None).unwrap().source, Source::Both);
    }

    /// The record of an account config declares, removed (`twr account remove`): only the
    /// record goes, the event says so, and it stays gone whatever config says later.
    fn removed_record(rt: &str, name: &str, native: bool) -> Event {
        hub(EventBody::AccountRemoved(AccountRemoved {
            runtime: rt.into(),
            account: name.into(),
            native,
            record_only: true,
            ..Default::default()
        }))
    }

    /// A recorded native `personal` and a recorded named `default`, then config declares
    /// `personal` named: the native account falls back to `default`, next to the named one.
    /// The name is the named account's, for every lookup by name over the list as for
    /// `resolve`; the native entry says it is shadowed (in the JSON too), and is still the
    /// native account (`info(None)`). Statuses land on the account they were of.
    #[test]
    fn a_native_account_shadowed_by_a_named_one_answers_to_no_name() {
        let evs = [
            added("codex", "personal", None, true),
            added("codex", "default", Some("/d"), false),
        ];
        // Before config names `personal`: no collision.
        let v = view(&Config::default(), &evs);
        assert!(v.accounts("codex").iter().all(|a| !a.shadowed));
        let c = config(&[("codex", "personal", Some("/p"), false)]);
        let mut v = view(&c, &evs);
        assert_eq!(v.native_name("codex"), "default");
        let all = v.accounts("codex");
        assert_eq!(
            names(&v, "codex", false),
            ["default", "default", "personal"]
        );
        assert!(all[0].native && all[0].shadowed);
        assert!(!all[1].native && !all[1].shadowed);
        assert_eq!(v.resolve("codex", "default"), Some(Some("default".into())));
        let found: Vec<&AccountInfo> = all.iter().filter(|a| a.answers_to("default")).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].native, found[0].dir.as_deref()),
            (false, Some(Path::new("/d")))
        );
        assert!(v.info("codex", None).unwrap().shadowed);
        assert!(!v.info("codex", Some("default")).unwrap().native);
        let j = v.to_json(false);
        let listed = j["codex"]["accounts"].as_array().unwrap();
        assert_eq!(listed[0]["shadowed"], json!(true));
        assert!(listed[1..].iter().all(|a| a.get("shadowed").is_none()));
        // A status of the native account under the name says so; one without the flag is
        // the named account's (the name stands for it).
        let mut native_status = status("codex", "default", Login::LoggedOut);
        if let EventBody::AccountStatus(p) = &mut native_status.body {
            p.native = true;
        }
        v.apply(&native_status);
        v.apply(&status("codex", "default", Login::LoggedIn));
        assert_eq!(v.info("codex", None).unwrap().login, Some(Login::LoggedOut));
        assert_eq!(
            v.info("codex", Some("default")).unwrap().login,
            Some(Login::LoggedIn)
        );
        // Not shadowed once the named one is removed: the name is the native account's.
        v.apply(&removed("codex", "default", false));
        assert!(!v.info("codex", None).unwrap().shadowed);
        assert_eq!(v.resolve("codex", "default"), Some(None));
        // A config native is never shadowed.
        let c = config(&[("codex", "default", None, true)]);
        let v = view(&c, &[added("codex", "default", Some("/d"), false)]);
        assert!(!v.info("codex", None).unwrap().shadowed);
        assert_eq!(names(&v, "codex", true), ["default"]);
    }

    /// A record and a declaration of one name are one account whatever kind each gives it:
    /// both (`Source::Both`), the record's kind known (`live_record`). Removing its record
    /// leaves config's account; once config drops the declaration, nothing comes back, live or
    /// replayed, and the native account is not removed.
    #[test]
    fn a_record_config_gives_another_kind_is_both_and_its_removal_stays() {
        // A named record that config declares native.
        let early = [added("codex", "work", Some("/w"), false)];
        let c = config(&[("codex", "work", None, true)]);
        let mut v = view(&c, &early);
        let native = v.info("codex", None).unwrap();
        assert_eq!(
            (native.name.as_str(), native.source),
            ("work", Source::Both)
        );
        assert_eq!(v.live_record("codex", "work", true), Some(false));
        let removal = removed_record("codex", "work", false);
        v.apply(&removal);
        let native = v.info("codex", None).unwrap();
        assert_eq!(
            (native.name.as_str(), native.source, native.removed),
            ("work", Source::Config, false)
        );
        assert_eq!(v.live_record("codex", "work", true), None);
        assert_eq!(view(&c, &[early[0].clone(), removal.clone()]), v);
        // Config drops it: no named `work` again, the native account its own.
        let dropped = view(&Config::default(), &[early[0].clone(), removal.clone()]);
        assert_eq!(dropped.resolve("codex", "work"), None);
        assert_eq!(dropped.resolve("codex", "default"), Some(None));
        assert_eq!(names(&dropped, "codex", false), ["default"]);
        // Kept, it would have come back from its record.
        let kept = view(&Config::default(), &early);
        assert_eq!(kept.resolve("codex", "work"), Some(Some("work".into())));

        // A native record that config declares named.
        let early = [added("codex", "home", None, true)];
        let c = config(&[("codex", "home", Some("/h"), false)]);
        let mut v = view(&c, &early);
        assert_eq!(v.native_name("codex"), "default");
        let home = v.info("codex", Some("home")).unwrap();
        assert_eq!((home.native, home.source), (false, Source::Both));
        assert_eq!(v.info("codex", None).unwrap().source, Source::Native);
        assert_eq!(v.live_record("codex", "home", false), Some(true));
        let removal = removed_record("codex", "home", true);
        v.apply(&removal);
        let home = v.info("codex", Some("home")).unwrap();
        assert_eq!((home.source, home.removed), (Source::Config, false));
        assert!(!v.info("codex", None).unwrap().removed);
        assert_eq!(view(&c, &[early[0].clone(), removal.clone()]), v);
        // Config drops it: the native account is `default` again, not removed (replayed, the
        // removal names the native account's name then, and is still the record's alone).
        let dropped = view(&Config::default(), &[early[0].clone(), removal.clone()]);
        assert_eq!(dropped.native_name("codex"), "default");
        let native = dropped.info("codex", None).unwrap();
        assert_eq!((native.source, native.removed), (Source::Native, false));
        assert_eq!(dropped.resolve("codex", "home"), None);
        assert_eq!(dropped.resolve("codex", "default"), Some(None));

        // A native record config declares native too, removed: config dropping it later
        // neither names nor removes the native account.
        let early = [added("codex", "me", None, true)];
        let removal = removed_record("codex", "me", true);
        let dropped = view(&Config::default(), &[early[0].clone(), removal]);
        let native = dropped.info("codex", None).unwrap();
        assert_eq!(
            (native.name.as_str(), native.source, native.removed),
            ("default", Source::Native, false)
        );
        // The native account's own removal (no config) still keeps its name.
        let gone = view(
            &Config::default(),
            &[early[0].clone(), removed("codex", "me", true)],
        );
        let native = gone.info("codex", None).unwrap();
        assert_eq!((native.name.as_str(), native.removed), ("me", true));
    }

    #[test]
    fn a_removed_native_account_keeps_its_name_from_a_named_default() {
        let c = Config::default();
        let evs = [
            added("claude-code", "personal", None, true),
            added("claude-code", "default", Some("/d"), false),
            status("claude-code", "default", Login::LoggedIn),
            removed("claude-code", "personal", true),
            status("claude-code", "default", Login::LoggedOut),
        ];
        // Folded live or replayed from the log: the same view.
        let mut live = view(&c, &evs[..3]);
        for e in &evs[3..] {
            live.apply(e);
        }
        let v = view(&c, &evs);
        assert_eq!(v, live);
        assert_eq!(v.native_name("claude-code"), "personal");
        assert_eq!(v.resolve("claude-code", "personal"), None);
        assert_eq!(
            v.resolve("claude-code", "default"),
            Some(Some("default".into()))
        );
        assert_eq!(names(&v, "claude-code", false), ["default"]);
        assert_eq!(names(&v, "claude-code", true), ["personal", "default"]);
        let d = v.info("claude-code", Some("default")).unwrap();
        assert_eq!(
            (d.native, d.removed, d.dir.as_deref(), d.login),
            (false, false, Some(Path::new("/d")), Some(Login::LoggedOut))
        );
        assert!(v.info("claude-code", None).unwrap().login.is_none());
        assert_eq!(v.label("claude-code", Some("default")), "claude-code");
        // Named back under its name, it is the native account again.
        let mut v = v;
        v.apply(&added("claude-code", "personal", None, true));
        assert_eq!(v.resolve("claude-code", "personal"), Some(None));
        assert_eq!(names(&v, "claude-code", false), ["personal", "default"]);
        // A live named account keeps its name from the native account that lost its own to
        // config (the default name), and from a removed native one that has it (an older log).
        let v = view(
            &config(&[("codex", "team", Some("/t"), false)]),
            &[
                added("codex", "team", None, true),
                added("codex", "default", Some("/d"), false),
            ],
        );
        assert_eq!(v.native_name("codex"), NATIVE_DEFAULT_NAME);
        assert_eq!(v.resolve("codex", "default"), Some(Some("default".into())));
        assert!(!v.info("codex", Some("default")).unwrap().native);
        let v = view(
            &c,
            &[
                added("codex", "me", None, true),
                removed("codex", "me", true),
                added("codex", "me", Some("/m"), false),
            ],
        );
        assert_eq!(v.resolve("codex", "me"), Some(Some("me".into())));
        assert_eq!(names(&v, "codex", false), ["me"]);
        assert_eq!(names(&v, "codex", true), ["me", "me"]);
        assert!(!v.info("codex", Some("me")).unwrap().native);
    }

    #[test]
    fn statuses_land_on_their_account() {
        let c = config(&[("claude-code", "work", Some("/w"), false)]);
        let mut v = view(&c, &[status("claude-code", "default", Login::LoggedIn)]);
        let native = v.info("claude-code", None).unwrap();
        assert_eq!(native.login, Some(Login::LoggedIn));
        assert_eq!(native.method.as_deref(), Some("claude.ai"));
        assert!(native.checked_at.is_some());
        assert_eq!(v.info("claude-code", Some("work")).unwrap().login, None);
        v.apply(&status("claude-code", "work", Login::LoggedOut));
        assert_eq!(
            v.info("claude-code", Some("work")).unwrap().login,
            Some(Login::LoggedOut)
        );
        // Naming the native keeps its status.
        v.apply(&added("claude-code", "me", None, true));
        assert_eq!(
            v.info("claude-code", None).unwrap().login,
            Some(Login::LoggedIn)
        );
        // A status of an account nothing declares is kept for when it is.
        v.apply(&status("claude-code", "later", Login::LoggedIn));
        assert_eq!(v.info("claude-code", Some("later")), None);
        v.apply(&added("claude-code", "later", Some("/l"), false));
        assert_eq!(
            v.info("claude-code", Some("later")).unwrap().login,
            Some(Login::LoggedIn)
        );
    }

    #[test]
    fn when_labels_show() {
        let c = Config::default();
        // The native account alone.
        let v = view(&c, &[status("claude-code", "default", Login::LoggedIn)]);
        assert!(!v.shows_labels("claude-code"));
        assert_eq!(v.label("claude-code", None), "claude-code");
        // Native + a named account, never checked: both count.
        let mut v = view(&c, &[added("claude-code", "work", Some("/w"), false)]);
        assert!(v.shows_labels("claude-code"));
        assert_eq!(v.label("claude-code", None), "claude-code:default");
        assert_eq!(v.label("claude-code", Some("work")), "claude-code:work");
        // Per runtime.
        assert!(!v.shows_labels("codex"));
        assert_eq!(v.label("codex", None), "codex");
        // The named one logs out (or goes missing): one counts.
        v.apply(&status("claude-code", "work", Login::LoggedOut));
        assert!(!v.shows_labels("claude-code"));
        assert_eq!(v.label("claude-code", Some("work")), "claude-code");
        v.apply(&status("claude-code", "work", Login::Missing));
        assert!(!v.shows_labels("claude-code"));
        v.apply(&status("claude-code", "work", Login::Unknown));
        assert!(v.shows_labels("claude-code"));
        // The native logged out, two named logged in.
        v.apply(&status("claude-code", "default", Login::LoggedOut));
        assert!(!v.shows_labels("claude-code"));
        v.apply(&added("claude-code", "side", Some("/s"), false));
        assert!(v.shows_labels("claude-code"));
        // Removed ones do not count.
        v.apply(&removed("claude-code", "side", false));
        assert!(!v.shows_labels("claude-code"));
    }

    #[test]
    fn json_shape_and_default_dirs() {
        let c = config(&[("codex", "team", None, false)]);
        let v = view(&c, &[status("codex", "team", Login::LoggedIn)])
            .with_default_root("/data/accounts");
        assert_eq!(
            v.info("codex", Some("team")).unwrap().dir.as_deref(),
            Some(Path::new("/data/accounts/codex/team"))
        );
        let j = v.to_json(true);
        assert_eq!(j["codex"]["shown"], json!(true));
        assert_eq!(j["codex"]["native"], json!("default"));
        let accounts = j["codex"]["accounts"].as_array().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0]["native"], json!(true));
        assert_eq!(accounts[0]["dir"], json!(null));
        assert_eq!(accounts[0]["source"], json!("native"));
        assert_eq!(accounts[1]["name"], json!("team"));
        assert_eq!(accounts[1]["dir"], json!("/data/accounts/codex/team"));
        assert_eq!(accounts[1]["login"], json!("logged_in"));
        assert_eq!(accounts[1]["share"], json!("config"));
        assert_eq!(accounts[1]["source"], json!("config"));
        let j = v.to_json(false);
        assert!(j["codex"]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("dir").is_none()));
    }

    #[test]
    fn account_dirs_stay_in_the_root_once_resolved() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("accounts");
        let outside = base.join("outside");
        std::fs::create_dir_all(outside.join("team")).unwrap();
        // The runtime's directory is made (private) when asked to.
        let dir = root.join("codex").join("team");
        assert_eq!(
            resolve_in_root(&root, "codex", &dir, false).unwrap_err(),
            format!(
                "resolving {}: No such file or directory (os error 2)",
                root.join("codex").display()
            )
        );
        assert_eq!(resolve_in_root(&root, "codex", &dir, true).unwrap(), dir);
        let mode = std::fs::metadata(root.join("codex"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        // Not a child of `<root>/<runtime>`, as written or once resolved.
        for bad in [
            outside.join("team"),
            root.join("claude-code").join("team"),
            root.join("codex"),
            root.join("codex").join(".."),
        ] {
            let e = resolve_in_root(&root, "codex", &bad, true).unwrap_err();
            assert!(e.contains("is not inside"), "{bad:?}: {e}");
        }
        // A link as the account directory itself.
        symlink(outside.join("team"), &dir).unwrap();
        let e = resolve_in_root(&root, "codex", &dir, false).unwrap_err();
        assert!(e.ends_with("is a symlink"), "{e}");
        std::fs::remove_file(&dir).unwrap();
        // `<root>/codex` a link to elsewhere: `<root>/codex/team` is outside.
        std::fs::remove_dir(root.join("codex")).unwrap();
        symlink(&outside, root.join("codex")).unwrap();
        let e = resolve_in_root(&root, "codex", &dir, true).unwrap_err();
        assert!(e.contains("is not inside"), "{e}");
        // A root reached through a link is fine: what counts is where it leads.
        let linked = base.join("linked-root");
        symlink(&root, &linked).unwrap();
        std::fs::create_dir_all(root.join("grok-build")).unwrap();
        assert_eq!(
            resolve_in_root(&linked, "grok-build", &linked.join("grok-build/x"), false).unwrap(),
            root.join("grok-build/x")
        );
    }
}

//! Accounts (`twr_core::account`): the hub's registry of every runtime's logins, its socket
//! methods, and the login checks.
//!
//! **The registry** is an [`AccountsView`] in the core: the config's declarations
//! (`[accounts.<runtime>.<name>]`, with [`HubOptions::accounts_root`] as the root of the
//! directories they leave out), and the log's `account.added`, `account.removed` and
//! `account.status` (hub thread) folded over them, on replay and live ([`Core::apply`]).
//! Config wins over a record of the same account. The hub never writes `config.toml`.
//!
//! **At start** ([`check_at_start`]) every named account (config's and recorded, not removed)
//! is checked against each runtime's own directory (the one it uses with no account variable
//! set: [`HubOptions::native_dir`]) and against the others: such an account is the native one
//! (setting the variable to it is not the same as leaving it unset), or shares another's
//! login. One that config.toml declares stops the hub like a config error (two of them in one
//! directory too). A recorded one (`twr account add` refuses both, so it got that way later:
//! the hub's environment changed, `CODEX_HOME` set to its directory, say) is a warning
//! (`hub.info`) instead, so the hub starts and `twr account remove` can remove it; until then
//! it is refused at use ([`conflict`]: a new thread or a move on it, its threads' starts,
//! attaches and jobs), checked again each time. So is a config account whose directory
//! became one of those after the start (a link retargeted), saying to change config.toml.
//! So is a named account whose settings file is the native account's and selects credentials
//! ([`shared_credentials`]: `apiKeyHelper`, `env.ANTHROPIC_API_KEY`, ...), or that twr can't
//! read safely (a FIFO, a device, over 1 MiB): it would, or may, sign in as the native
//! account (a warning at start too); unlike a collision, its directory is its own, so
//! `account.list` says it apart (`shared_credentials`), and its login is checked.
//!
//! **Methods**: `account.list` (users with directories, and each runtime's own directory and
//! selector variable as the hub has them, `native_dir` / `native_var`; the orchestrator
//! without; not thread agents), and the user's `account.add`, `account.remove` and `account.status`. `account.add`
//! records `account.added` unless config declares the account (nothing is recorded then) or a
//! live record already says the same; `dry_run` says what it would do without recording
//! (with `after_native_name`, as if the native account were named that first: `twr account
//! add --native-name`'s preflight, whose new name may be the one that rename frees).
//! `account.remove` records `account.removed`: of a config account only its record goes (one
//! with no record is refused; the record is of its own kind, native or named, whatever kind
//! config gives the account, and `record_only` says it stays gone when config drops the
//! declaration later), and an account threads run on needs `move_to` (each moves to
//! that account first, `hub::switch`; one that cannot refuses the whole removal before
//! anything is recorded) or `force` (they keep it and cannot start until they move). The
//! current orchestrator on it moves by a switch only (`twr orchestrator use`): with `move_to`
//! it refuses the whole removal, before anything is recorded, saying to switch it first. Only
//! threads on the hub's own box count (`account.list`'s `threads` too): one on another box
//! runs on that box's own login.
//!
//! **Login checks** ([`HubOptions::account_probe`]): of every runtime that has a named account
//! (not removed), its native account and each named one, on the hub's own box only, in the
//! background once the hub is open and every [`AgentsPoll::every`] (the agent probes'
//! cadence), after `account.add` (that account), and on `account.status {refresh}` (waited
//! for, at most [`REFRESH_WAIT`]). One check runs at a time; what it found is recorded as
//! `account.status` only when it differs from the account's last state (login, method,
//! note). A check that finds no CLI, or no directory, says `missing`.
//!
//! [`HubOptions::accounts_root`]: crate::HubOptions::accounts_root
//! [`HubOptions::native_dir`]: crate::HubOptions::native_dir
//! [`HubOptions::account_probe`]: crate::HubOptions::account_probe
//! [`AgentsPoll::every`]: super::AgentsPoll::every

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::Utc;
use futures::future::{BoxFuture, FutureExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use twr_core::account::{
    check_name, default_dir, takes_accounts, AccountInfo, AccountsView, Login, Share, Source,
    ACCOUNT_RUNTIMES,
};
use twr_core::config::Config;
use twr_core::event::{
    AccountAdded, AccountRemoved, AccountStatus, ChangedBy, EventBody, Source as EvSource,
    HUB_THREAD,
};
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;
use twr_core::runner::{AgentInfo, AgentLogin};

use super::{append_err, rpc, Batch, Caller, Core, Hub, HubState, Inner, LOCAL_BOX};

/// How long `account.status {refresh}` waits for its checks.
pub const REFRESH_WAIT: Duration = Duration::from_secs(30);

/// How the hub checks one account's login ([`crate::HubOptions::account_probe`]): given the
/// runtime and the account's directory (`None`: the native account), what the runtime's
/// status command says, run with the account's environment.
pub type AccountProbe =
    Arc<dyn Fn(String, Option<PathBuf>) -> BoxFuture<'static, AgentInfo> + Send + Sync>;

/// The directory a runtime uses with no account variable set
/// ([`crate::HubOptions::native_dir`]); `None` when unknown, or for a runtime that takes no
/// accounts.
pub type NativeDirFn = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// A variable of the hub's environment as the hub reads it ([`crate::HubOptions::native_var`]):
/// what `account.list` reports of each runtime's selector variable (`native_var`).
pub type NativeVarFn = Arc<dyn Fn(&str) -> Option<std::ffi::OsString> + Send + Sync>;

/// The hub's own check: `twr_adapters::agents::probe_account` with the account's environment
/// (`twr_adapters::account::account_env`: its variables set, the stray credentials removed;
/// none for the native account), the hub's own `PATH` and `TWR_*_BIN`. A named account whose
/// directory does not exist is not looked at (`found: false`). What `twr hub` uses.
pub fn own_account_probe() -> AccountProbe {
    Arc::new(|runtime: String, dir: Option<PathBuf>| {
        async move {
            let (env, remove) = match &dir {
                None => (Vec::new(), Vec::new()),
                Some(d) if !d.is_dir() => {
                    return AgentInfo {
                        runtime,
                        found: false,
                        note: Some("its directory does not exist".into()),
                        ..Default::default()
                    }
                }
                Some(d) => twr_adapters::account::account_env(&runtime, d).unwrap_or_default(),
            };
            twr_adapters::agents::probe_account(
                &runtime,
                &env,
                &remove,
                |k| std::env::var_os(k),
                twr_adapters::agents::COMMAND_TIMEOUT,
            )
            .await
        }
        .boxed()
    })
}

/// Each runtime's own directory as the hub's environment says (its variable when set, else
/// `.claude`, `.codex`, `.grok` in `$HOME`, else in the password database's home:
/// `twr_adapters::account::own_native_dir`). Next-message suggestions' Codex home is the
/// native one's (`hub::suggest`).
pub fn own_native_dir() -> NativeDirFn {
    Arc::new(twr_adapters::account::own_native_dir)
}

/// The hub's own environment's variables (`std::env::var_os`): what `twr hub` reports.
pub fn own_native_var() -> NativeVarFn {
    Arc::new(|k: &str| std::env::var_os(k))
}

/// `account.list`'s `native_dir` and `native_var` of `runtime` (a user's only): its own
/// directory as the hub has it (`native_dir`: [`NativeDirFn`], `null` when none is known) and
/// its selector variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`) as the hub's
/// environment has it (`var`): `{name, set, value?}`, `value` when it is set (and UTF-8), so
/// that `twr account` runs the native account's login with the hub's selection, not its own.
/// Nothing for a runtime that takes no accounts.
fn native_json(
    runtime: &str,
    native_dir: &NativeDirFn,
    var: &NativeVarFn,
) -> Option<(Value, Value)> {
    let s = twr_adapters::account::support(runtime)?;
    let dir = native_dir(runtime)
        .and_then(|d| d.to_str().map(String::from))
        .map_or(Value::Null, Value::String);
    let mut v = json!({ "name": s.var, "set": false });
    if let Some(value) = var(s.var) {
        v["set"] = json!(true);
        if let Some(value) = value.to_str() {
            v["value"] = json!(value);
        }
    }
    Some((dir, v))
}

/// The hub's account bookkeeping outside the core (`Inner::accounts`).
pub(super) struct AccountsRt {
    probe: Option<AccountProbe>,
    /// Held by the check running (one at a time, so an older result is never recorded after
    /// a newer one).
    probing: tokio::sync::Mutex<()>,
    /// [`crate::HubOptions::accounts_root`].
    root: PathBuf,
    native_dir: NativeDirFn,
    /// Checks asked for under the core lock ([`Core::check_account_soon`]), run by the task
    /// [`start_probes`] starts.
    asked: mpsc::UnboundedSender<Target>,
    /// Their receiving end, until [`start_probes`] takes it.
    asked_rx: std::sync::Mutex<Option<mpsc::UnboundedReceiver<Target>>>,
}

impl AccountsRt {
    pub(super) fn new(probe: Option<AccountProbe>, root: PathBuf, native_dir: NativeDirFn) -> Self {
        let (asked, asked_rx) = mpsc::unbounded_channel();
        AccountsRt {
            probe,
            probing: tokio::sync::Mutex::new(()),
            root,
            native_dir,
            asked,
            asked_rx: std::sync::Mutex::new(Some(asked_rx)),
        }
    }

    /// The directory `runtime` uses with no account variable set ([`NativeDirFn`]).
    pub(super) fn native_dir(&self, runtime: &str) -> Option<PathBuf> {
        (self.native_dir)(runtime)
    }
}

/// Whether `a` and `b` are one directory: the same path (as components: a trailing `/` or a
/// `.` does not count), or, when both exist, the same directory once resolved.
pub(super) fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Whether `path` is inside directory `dir` (not `dir` itself), as written or once both
/// are resolved as far as they exist.
fn inside(path: &Path, dir: &Path) -> bool {
    let resolved = |p: &Path| {
        let mut rest = Vec::new();
        let mut at = p;
        loop {
            if let Ok(real) = std::fs::canonicalize(at) {
                return Some(rest.iter().rev().fold(real, |p: PathBuf, c| p.join(c)));
            }
            rest.push(at.file_name()?.to_owned());
            at = at.parent()?;
        }
    };
    let below = |p: &Path, d: &Path| p != d && p.starts_with(d);
    below(path, dir)
        || match (resolved(path), resolved(dir)) {
            (Some(p), Some(d)) => below(&p, &d),
            _ => false,
        }
}

/// The runtime whose own directory `dir` is, if any ([`NativeDirFn`]).
fn native_owner(native_dir: &NativeDirFn, dir: &Path) -> Option<&'static str> {
    ACCOUNT_RUNTIMES
        .iter()
        .copied()
        .find(|rt| native_dir(rt).is_some_and(|n| same_dir(&n, dir)))
}

/// How an account is written in messages: `claude-code:work`.
fn label(runtime: &str, name: &str) -> String {
    format!("{runtime}:{name}")
}

/// Where an account comes from, for messages: `[accounts.claude-code.work]` for a config
/// one, else `claude-code:work (twr account add)`.
fn origin(a: &AccountInfo) -> String {
    match a.source {
        Source::Config | Source::Both => format!("[accounts.{}.{}]", a.runtime, a.name),
        Source::Recorded => format!("{} (twr account add)", label(&a.runtime, &a.name)),
        Source::Native => label(&a.runtime, &a.name),
    }
}

/// The named account (any runtime, not removed) whose directory is `dir`, other than
/// `runtime`'s `name`.
fn dir_owner(view: &AccountsView, dir: &Path, runtime: &str, name: &str) -> Option<AccountInfo> {
    let runtimes: BTreeSet<String> = ACCOUNT_RUNTIMES
        .iter()
        .map(|r| r.to_string())
        .chain(view.runtimes())
        .collect();
    runtimes.iter().find_map(|rt| {
        view.accounts(rt).into_iter().find(|a| {
            !a.native
                && !(a.runtime == runtime && a.name == name)
                && a.dir.as_deref().is_some_and(|d| same_dir(d, dir))
        })
    })
}

/// Why named account `a` cannot be used (`None`: it can): its directory is a runtime's own
/// directory in the hub's environment (`native_dir`), or another account's (not removed),
/// both on resolved paths, as they are now. Checked at use, whatever declares the account:
/// `account.add` refuses both and [`check_at_start`] stops the hub for a config account found
/// so at start, but a directory can change after (a link retargeted, a variable of the hub's
/// environment). The fix said is the account's own: `twr account remove` for a recorded one,
/// changing config.toml (and restarting the hub) for one config declares. Said as
/// `<why>: <fix>`, after `claude-code:work can't be used: `.
pub(super) fn conflict(
    view: &AccountsView,
    native_dir: &NativeDirFn,
    a: &AccountInfo,
) -> Option<String> {
    if a.native || a.removed {
        return None;
    }
    let dir = a.dir.as_deref()?;
    let fix = match a.source {
        Source::Config | Source::Both => format!(
            "change [accounts.{}.{}] dir in config.toml and restart the hub",
            a.runtime, a.name
        ),
        _ => format!(
            "remove the account (twr account remove {} {})",
            a.runtime, a.name
        ),
    };
    if let Some(owner) = native_owner(native_dir, dir) {
        let var = twr_adapters::account::support(owner).map_or("its variable", |s| s.var);
        return Some(format!(
            "its directory {} is {owner}'s own directory in the hub's environment ({var}, or \
             its default): set {var} elsewhere for the hub and restart it, or {fix}",
            dir.display()
        ));
    }
    let other = dir_owner(view, dir, &a.runtime, &a.name)?;
    let other_label = label(&other.runtime, &other.name);
    // A config account in a record's directory: removing the record fixes it too.
    let or_other = match (a.source, other.source) {
        (Source::Config | Source::Both, Source::Recorded) => format!(
            ", or remove {other_label} (twr account remove {} {})",
            other.runtime, other.name
        ),
        _ => String::new(),
    };
    Some(format!(
        "its directory {} is {other_label}'s too ({}): {fix}{or_other}",
        dir.display(),
        origin(&other)
    ))
}

/// Why named account `a` would sign in with the native account's credentials (`None`: it
/// would not, or is no named account): its settings file is the native one (a link to it, or
/// to where it leads:
/// `twr_adapters::account::shared_credentials`), and that sets settings that select
/// credentials (`apiKeyHelper`, `env.ANTHROPIC_API_KEY`, Grok Build's
/// `auth.auth_provider_command`, ...), which the runtime applies over the account's own login
/// whatever twr removes from its environment. `twr account add` leaves such a file out, but
/// one linked before, or a native file edited since, still reaches the account. One that
/// can't be read (not a regular file, larger than [`twr_core::fsread::SETTINGS_MAX`]: read
/// without blocking and within that cap, as this runs under the core lock) is refused the
/// same: twr can't tell what the runtime would read in it. An account's own settings file
/// setting them is its own business (that may be how it signs in).
pub(super) fn shared_credentials(native_dir: &NativeDirFn, a: &AccountInfo) -> Option<String> {
    use twr_adapters::account::LeftOut;
    if a.native || a.removed {
        return None;
    }
    let dir = a.dir.as_deref()?;
    let native = native_dir(&a.runtime)?;
    let (file, why) = twr_adapters::account::shared_credentials(&a.runtime, dir, &native)?;
    let keys = match why {
        LeftOut::Credentials(keys) => keys,
        LeftOut::Unchecked(why) => {
            return Some(format!(
                "its {file} is the native account's ({}), which twr can't check: {why}, so it \
                 may sign in as the native account: replace {} with a copy of its own",
                native.join(file).display(),
                dir.join(file).display()
            ))
        }
    };
    Some(format!(
        "its {file} is the native account's ({}), which sets {}, so it would sign in as the \
         native account: replace {} with a copy without {}",
        native.join(file).display(),
        keys.join(", "),
        dir.join(file).display(),
        if keys.len() == 1 {
            "that key"
        } else {
            "those keys"
        }
    ))
}

/// The start's check of the accounts `view` folds (`config`'s, then the log's) against each
/// runtime's own directory (`native_dir`) and each other (see the module docs): a config
/// account in a runtime's own directory, two config accounts (not removed) in one, or a
/// config account named `default` while nothing names the native account (config or a
/// record: [`AccountsView::check_config_default`]), is an error naming the account and how
/// to fix it; each recorded account in [`conflict`], and
/// each named account whose settings file would sign it in as the native account
/// ([`shared_credentials`]), is a warning returned. (A config account in a recorded one's
/// directory is not an error: the record's warning says to remove it, and until then both are
/// refused at use, the config account's [`conflict`] saying so too.)
pub(super) fn check_at_start(
    view: &AccountsView,
    config: &Config,
    native_dir: &NativeDirFn,
) -> anyhow::Result<Vec<String>> {
    // A named `default` while nothing, config or record, names the native account.
    view.check_config_default().map_err(anyhow::Error::msg)?;
    for (rt, names) in &config.accounts {
        for (name, a) in names {
            if a.native {
                continue;
            }
            let Some(dir) = view.info(rt, Some(name)).and_then(|i| i.dir) else {
                continue;
            };
            if let Some(owner) = native_owner(native_dir, &dir) {
                if owner == rt {
                    anyhow::bail!(
                        "[accounts.{rt}.{name}] dir is {rt}'s own directory: use native = true"
                    );
                }
                anyhow::bail!(
                    "[accounts.{rt}.{name}] dir is {owner}'s own directory: give it a directory \
                     of its own"
                );
            }
        }
    }
    let declared = |a: &AccountInfo| matches!(a.source, Source::Config | Source::Both);
    let mut seen: Vec<(PathBuf, AccountInfo)> = Vec::new();
    let mut warnings = Vec::new();
    for rt in view.runtimes() {
        for a in view.accounts(&rt) {
            // Either kind: its settings file the native one, which selects credentials.
            if let Some(why) = shared_credentials(native_dir, &a) {
                warnings.push(format!(
                    "{} can't be used: {why}",
                    label(&a.runtime, &a.name)
                ));
            }
            let Some(dir) = a.dir.clone().filter(|_| !a.native && declared(&a)) else {
                if let Some(why) = conflict(view, native_dir, &a) {
                    warnings.push(format!(
                        "{} can't be used: {why}",
                        label(&a.runtime, &a.name)
                    ));
                }
                continue;
            };
            if let Some((_, other)) = seen.iter().find(|(d, _)| same_dir(d, &dir)) {
                anyhow::bail!(
                    "{} and {} have one directory, {}: each account has its own (change \
                     config.toml)",
                    origin(other),
                    origin(&a),
                    dir.display()
                );
            }
            seen.push((dir, a));
        }
    }
    Ok(warnings)
}

/// `account.add` parameters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountAddParams {
    /// The runtime (`claude-code`, `codex`, `grok-build`).
    pub runtime: String,
    /// The account's name ([`check_name`]).
    pub account: String,
    /// Its directory (absolute); default `<accounts root>/<runtime>/<name>`. Not with
    /// `native`.
    pub dir: Option<PathBuf>,
    /// What it shares with the native account (default `config`).
    pub share: Option<Share>,
    /// This names the native account instead (no directory of its own).
    pub native: bool,
    /// Say what would be recorded, and refuse what would be refused, recording nothing.
    pub dry_run: bool,
    /// With `dry_run`, of a named account: say it as if the native account were named this
    /// first (`twr account add --native-name`'s preflight: the new name may be the native
    /// account's now, which the rename frees). That rename is checked too, as `account.add
    /// {native, dry_run}` checks it; nothing is recorded.
    pub after_native_name: Option<String>,
}

/// `account.remove` parameters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountRemoveParams {
    /// The runtime.
    pub runtime: String,
    /// The account's name (the native account's current name for the native one).
    pub account: String,
    /// Move its threads to this account of the same runtime first (each as `thread.set_model
    /// {account}` would, by the user, within their pins), then remove it.
    pub move_to: Option<String>,
    /// Remove it although threads run on it: they keep it, and refuse to start until they
    /// move.
    pub force: bool,
}

/// `account.status` parameters.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccountStatusParams {
    /// Only this runtime.
    pub runtime: Option<String>,
    /// Only this account of `runtime` (checked with `refresh`).
    pub account: Option<String>,
    /// Check the logins again now (waited for, at most [`REFRESH_WAIT`]).
    pub refresh: bool,
}

/// One account to check: runtime, its name when the check began, whether it is the native
/// account, its directory (`None`: the native). What was checked is recorded only while the
/// account is still that one ([`Core::record_statuses`]).
type Target = (String, String, bool, Option<PathBuf>);

/// The names of `threads` (ids and names).
fn names(threads: &[(String, String)]) -> Vec<String> {
    threads.iter().map(|(_, n)| n.clone()).collect()
}

/// [`names`] for a message, the current orchestrator's (id `orch`) said to be it and how it
/// moves (a switch, `twr orchestrator use <runtime>:<account>`; `to`: the account, else
/// `<name>`).
fn listed(threads: &[(String, String)], orch: Option<&str>, rt: &str, to: Option<&str>) -> String {
    threads
        .iter()
        .map(|(id, n)| match Some(id.as_str()) == orch {
            true => format!(
                "{n} (the orchestrator: switch it with twr orchestrator use {rt}:{})",
                to.unwrap_or("<name>")
            ),
            false => n.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The [`Login`] an [`AgentInfo`] says: `missing` when no CLI (or directory) was found.
fn login_of(info: &AgentInfo) -> Login {
    if !info.found {
        return Login::Missing;
    }
    match info.login {
        AgentLogin::LoggedIn => Login::LoggedIn,
        AgentLogin::LoggedOut => Login::LoggedOut,
        AgentLogin::Unknown => Login::Unknown,
    }
}

/// A runtime that takes accounts, or `bad_request`.
fn check_runtime(runtime: &str) -> Result<(), RpcError> {
    if takes_accounts(runtime) {
        Ok(())
    } else {
        Err(rpc(
            codes::BAD_REQUEST,
            format!(
                "{runtime:?} does not take accounts ({} do)",
                ACCOUNT_RUNTIMES.join(", ")
            ),
        ))
    }
}

/// The user only (`account.add`, `account.remove`, `account.status`): the orchestrator is
/// refused like `box.add` (thread agents never get this far: the server's `AGENT_METHODS`).
fn users_only(caller: &Caller, what: &str) -> Result<(), RpcError> {
    if caller.is_orchestrator() {
        return Err(rpc(
            codes::UNAUTHORIZED,
            format!("only the user {what} (`twr account`)"),
        ));
    }
    Ok(())
}

impl Core {
    /// Thread `v`'s account for its JSON (`hub::thread_json`): its name (the native account's
    /// current name when it records none), whether its runtime shows account labels, and
    /// whether the account is removed or unknown (never the native account of a thread on
    /// another box: that is the box's own login).
    pub(super) fn thread_account_json(&self, v: &ThreadView) -> (String, bool, bool) {
        let rt = &v.runtime;
        let (name, _) = self.thread_account(v);
        let removed = match &v.account {
            None if v.box_name != LOCAL_BOX => false,
            None => self.accounts.info(rt, None).is_none_or(|a| a.removed),
            Some(a) => self.accounts.resolve(rt, a).is_none(),
        };
        (name, self.accounts.shows_labels(rt), removed)
    }

    /// The account thread `v` runs on: its name (the native account's current one when it
    /// records none) and whether it is the native account (a recorded name the native account
    /// has is the native one, [`AccountsView::info`]).
    pub(super) fn thread_account(&self, v: &ThreadView) -> (String, bool) {
        match &v.account {
            None => (self.accounts.native_name(&v.runtime), true),
            Some(a) => {
                let native = self
                    .accounts
                    .info(&v.runtime, Some(a))
                    .is_some_and(|i| i.native);
                (a.clone(), native)
            }
        }
    }

    /// The threads that run on each account (not deleted, not retired, on the hub's own box),
    /// by (runtime, name, native): a thread that records none counts for its runtime's native
    /// account. One on another box runs on that box's own login, which is not the registry's.
    fn threads_by_account(&self) -> HashMap<(String, String, bool), Vec<String>> {
        let mut out: HashMap<(String, String, bool), Vec<String>> = HashMap::new();
        for v in self.registry.list() {
            if v.retired || v.box_name != LOCAL_BOX {
                continue;
            }
            let (name, native) = self.thread_account(v);
            out.entry((v.runtime.clone(), name, native))
                .or_default()
                .push(v.name.clone());
        }
        out
    }

    /// The threads on account `name` of `runtime` (the native one when `native`; as
    /// [`Core::threads_by_account`] counts them: the hub's own box only), by name: their ids
    /// and names.
    fn threads_on(&self, runtime: &str, name: &str, native: bool) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| !v.retired && v.runtime == runtime && v.box_name == LOCAL_BOX)
            .filter(|v| self.thread_account(v) == (name.to_string(), native))
            .map(|v| (v.id.clone(), v.name.clone()))
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// The threads on `runtime`'s named account `name` (not retired, on the hub's own box)
    /// given its directory by a recorded path the account is no longer configured with
    /// (`Core::retained_env_dir`): `[{thread, path}]`, by thread name. `twr account login`
    /// signs the configured path in, so each needs `twr model <thread> --account <name>`.
    fn retained_paths(&self, runtime: &str, name: &str) -> Vec<Value> {
        let mut out: Vec<(String, PathBuf)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| !v.retired && v.runtime == runtime && v.box_name == LOCAL_BOX)
            .filter(|v| v.account.as_deref() == Some(name))
            .filter_map(|v| Some((v.name.clone(), self.retained_env_dir(v)?.0)))
            .collect();
        out.sort();
        out.into_iter()
            .map(|(thread, path)| json!({ "thread": thread, "path": path }))
            .collect()
    }

    /// The login thread `v`'s runtime process still runs on when that login was removed:
    /// `(runtime, account name)` of the account the process was started on
    /// ([`super::ThreadRt::runtime_settings`], kept until the log closes the process: a move
    /// leaves them while its turn finishes), when that account is removed now or its name
    /// leads to another directory (`Core::session_account_gone`). Its runtime may still read
    /// and write the login's directory: `twr account remove` waits for it before a logout
    /// or a purge. `None` with no runtime process, on another box (its own login), or on a
    /// login that is still the account's.
    pub(super) fn runtime_on_removed_account(&self, v: &ThreadView) -> Option<(String, String)> {
        if v.box_name != LOCAL_BOX {
            return None;
        }
        let s = self.rt.get(&v.id)?.runtime_settings.as_ref()?;
        let rt = s.runtime.as_str();
        if !takes_accounts(rt) {
            return None;
        }
        let gone = match s.account.as_deref() {
            None => self.accounts.info(rt, None).is_none_or(|a| a.removed),
            Some(n) => {
                self.accounts.resolve(rt, n).is_none()
                    || self
                        .session_account_gone(rt, Some(n), s.account_dir.as_deref())
                        .is_some()
            }
        };
        let name = s
            .account
            .clone()
            .unwrap_or_else(|| self.accounts.native_name(rt));
        gone.then(|| (rt.to_string(), name))
    }

    /// The threads (`(id, name)`, by name) whose runtime process still runs on a removed
    /// login ([`Core::runtime_on_removed_account`]) in directory `dir` (the one `runtime`'s
    /// account `name` was removed with; `None`: the native account): by the directory the
    /// process was started in when that is known, that directory or one inside it, whatever
    /// runtime and account it was started on (an earlier account in that directory, of this
    /// runtime or another, removed with `force` while its session lives), else by the
    /// runtime and the account's name. Removed threads too: their process may still be
    /// stopping.
    fn running_on_removed(
        &self,
        runtime: &str,
        name: &str,
        dir: Option<&Path>,
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| {
                let Some((r, n)) = self.runtime_on_removed_account(v) else {
                    return false;
                };
                let ran_in = self
                    .rt
                    .get(&v.id)
                    .and_then(|r| r.runtime_settings.as_ref())
                    .and_then(|s| s.account_dir.clone());
                match (ran_in, dir) {
                    (Some(d), Some(removed)) => same_dir(&d, removed) || inside(&d, removed),
                    _ => r == runtime && n == name,
                }
            })
            .map(|v| (v.id.clone(), v.name.clone()))
            .collect();
        out.sort_by(|a, b| a.1.cmp(&b.1));
        out
    }

    /// Why `runtime`'s named account `name` cannot be used: its directory is not its own alone
    /// ([`conflict`]), or its settings file is the native account's and selects credentials
    /// ([`shared_credentials`]); `None`: it can (or is no named account). What a new thread, a
    /// move, a start, an attach and a job check.
    pub(super) fn account_conflict(&self, runtime: &str, name: &str) -> Option<String> {
        let a = self.accounts.info(runtime, Some(name))?;
        conflict(&self.accounts, &self.account_native_dir, &a)
            .or_else(|| shared_credentials(&self.account_native_dir, &a))
    }

    /// [`conflict`] of `runtime`'s named account `name` alone: its directory is not its own
    /// alone, so that a login, a logout, a check or a deletion in it would reach another login.
    fn account_dir_conflict(&self, runtime: &str, name: &str) -> Option<String> {
        let a = self.accounts.info(runtime, Some(name))?;
        conflict(&self.accounts, &self.account_native_dir, &a)
    }

    /// `account.list` / `account.status`'s answer: `{runtimes: {<runtime>: {shown, native,
    /// accounts: [AccountInfo + threads (+ conflict)]}}, now}` for `runtime`, else for every
    /// runtime that takes accounts and every one the registry has; directories only
    /// `with_dirs`. `threads` counts the hub's own box's ([`Core::threads_by_account`]);
    /// `conflict` says why a recorded account cannot be used ([`conflict`]: its directory is
    /// not its own alone); `shared_credentials` (`with_dirs`) why its settings file would sign
    /// it in as the native account ([`shared_credentials`]: refused at use too); `retained_paths`
    /// (`with_dirs`, when any) its threads given its directory by a path it is no longer
    /// configured with ([`Core::retained_paths`]). `with_dirs`, each runtime that takes accounts
    /// says its own directory and selector variable as the hub has them (`native_dir`,
    /// `native_var`: [`native_json`]).
    fn accounts_json(&self, runtime: Option<&str>, with_dirs: bool) -> Value {
        let view = &self.accounts;
        let mut all = view.to_json(with_dirs);
        let wanted: BTreeSet<String> = match runtime {
            Some(rt) => [rt.to_string()].into(),
            None => ACCOUNT_RUNTIMES
                .iter()
                .map(|r| r.to_string())
                .chain(view.runtimes())
                .collect(),
        };
        let threads = self.threads_by_account();
        let mut out = serde_json::Map::new();
        for rt in wanted {
            let mut entry = all
                .get_mut(&rt)
                .map(Value::take)
                .unwrap_or_else(|| runtime_json(view, &rt, with_dirs));
            let native = native_json(&rt, &self.account_native_dir, &self.account_native_var)
                .filter(|_| with_dirs);
            if let (Some((dir, var)), Some(o)) = (native, entry.as_object_mut()) {
                o.insert("native_dir".into(), dir);
                o.insert("native_var".into(), var);
            }
            if let Some(accounts) = entry["accounts"].as_array_mut() {
                for a in accounts {
                    let name = a["name"].as_str().unwrap_or_default().to_string();
                    let native = a["native"] == json!(true);
                    if !native {
                        if let Some(why) = self.account_dir_conflict(&rt, &name) {
                            a["conflict"] = json!(why);
                        }
                        // (It names directories: a user's only.)
                        let shared = view
                            .info(&rt, Some(&name))
                            .filter(|_| with_dirs)
                            .and_then(|i| shared_credentials(&self.account_native_dir, &i));
                        if let Some(why) = shared {
                            a["shared_credentials"] = json!(why);
                        }
                        // (Paths are a user's only, as directories are.)
                        let retained = match with_dirs {
                            true => self.retained_paths(&rt, &name),
                            false => Vec::new(),
                        };
                        if !retained.is_empty() {
                            a["retained_paths"] = json!(retained);
                        }
                    }
                    let n = threads.get(&(rt.clone(), name, native)).map_or(0, Vec::len);
                    a["threads"] = json!(n);
                }
            }
            out.insert(rt, entry);
        }
        json!({ "runtimes": out, "now": Utc::now() })
    }

    /// What to check: `runtime`'s `account` when both are given; else every account (not
    /// removed) of `runtime`, or, without one, of every runtime that has a named account.
    /// Never a named account that collides ([`conflict`]): its runtime would be pointed at
    /// another login's directory (nothing is recorded for it).
    fn probe_targets(&self, runtime: Option<&str>, account: Option<&str>) -> Vec<Target> {
        let view = &self.accounts;
        let targets: Vec<Target> = match (runtime, account) {
            (Some(rt), Some(name)) => view
                .info(rt, Some(name))
                .and_then(target)
                .into_iter()
                .collect(),
            (Some(rt), None) => view.accounts(rt).into_iter().filter_map(target).collect(),
            (None, _) => view
                .runtimes()
                .iter()
                .filter(|rt| takes_accounts(rt))
                .map(|rt| view.accounts(rt))
                .filter(|accounts| accounts.iter().any(|a| !a.native))
                .flatten()
                .filter_map(target)
                .collect(),
        };
        targets
            .into_iter()
            .filter(|(rt, name, native, _)| {
                *native || self.account_dir_conflict(rt, name).is_none()
            })
            .collect()
    }

    /// Checks the login of `runtime`'s account `account` (`None`: the native one) in the
    /// background, soon (what a sign-in failure on it asks for, `hub::retry`). Nothing without
    /// a probe, or for an account the registry no longer has.
    pub(super) fn check_account_soon(&self, hub: &Inner, runtime: &str, account: Option<&str>) {
        if hub.accounts.probe.is_none() {
            return;
        }
        let targets = match account {
            Some(name) => self.probe_targets(Some(runtime), Some(name)),
            None => self
                .accounts
                .info(runtime, None)
                .and_then(target)
                .into_iter()
                .collect(),
        };
        for t in targets {
            let _ = hub.accounts.asked.send(t);
        }
    }

    /// Records what the checks found (`results`), each only when it differs from its
    /// account's last state. A result is of the directory it checked: one whose account is
    /// gone meanwhile (removed, renamed), or is no longer that directory (removed and added
    /// again elsewhere, its name now the native account's), is dropped. An error says whose
    /// checks could not be recorded, and why (their accounts keep their last state).
    fn record_statuses(
        &mut self,
        hub: &Inner,
        results: Vec<(Target, AgentInfo)>,
    ) -> Result<(), String> {
        let mut batch = Batch::default();
        let mut labels = Vec::new();
        for ((rt, name, native, dir), info) in results {
            let current = if native {
                self.accounts.info(&rt, None)
            } else {
                self.accounts
                    .info(&rt, Some(&name))
                    .filter(|a| !a.native && a.dir == dir)
            };
            let Some(current) = current.filter(|a| !a.removed) else {
                continue;
            };
            // Collides now: what was checked may be another login's directory.
            if !native && conflict(&self.accounts, &self.account_native_dir, &current).is_some() {
                continue;
            }
            let login = login_of(&info);
            if current.login == Some(login)
                && current.method == info.method
                && current.note == info.note
            {
                continue;
            }
            labels.push(label(&rt, &current.name));
            batch.push(
                HUB_THREAD,
                None,
                EvSource::Hub,
                EventBody::AccountStatus(AccountStatus {
                    runtime: rt,
                    account: current.name,
                    // (A named account may have the native account's name: `shadowed`.)
                    native,
                    login,
                    method: info.method,
                    note: info.note,
                    ..Default::default()
                }),
            );
        }
        if batch.events.is_empty() {
            return Ok(());
        }
        self.append_batch(hub, batch).map(drop).map_err(|e| {
            let checks = match labels.len() {
                1 => "check",
                _ => "checks",
            };
            format!(
                "the login {checks} of {} could not be recorded: {e:#}",
                labels.join(", ")
            )
        })
    }
}

/// What checking account `a` looks at: nothing for a removed one, or a named one without a
/// directory.
fn target(a: AccountInfo) -> Option<Target> {
    if a.removed || (!a.native && a.dir.is_none()) {
        return None;
    }
    Some((a.runtime, a.name, a.native, a.dir.filter(|_| !a.native)))
}

/// `runtime`'s entry of [`AccountsView::to_json`], for a runtime the view has nothing of yet
/// (its native account alone).
fn runtime_json(view: &AccountsView, runtime: &str, with_dirs: bool) -> Value {
    let accounts: Vec<Value> = view
        .accounts_with(runtime, true)
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
    json!({
        "shown": view.shows_labels(runtime),
        "native": view.native_name(runtime),
        "accounts": accounts,
    })
}

/// Checks `targets` (one check at a time across the hub, see the module docs) and records
/// what changed. Nothing without a probe. An error when what changed could not be recorded
/// ([`Core::record_statuses`]).
async fn probe(inner: &Arc<Inner>, targets: Vec<Target>) -> Result<(), String> {
    let Some(probe) = inner.accounts.probe.clone() else {
        return Ok(());
    };
    if targets.is_empty() {
        return Ok(());
    }
    let _one = inner.accounts.probing.lock().await;
    let checks = targets.into_iter().map(|t| {
        let check = probe(t.0.clone(), t.3.clone());
        async move { (t, check.await) }
    });
    let results = futures::future::join_all(checks).await;
    tracing::debug!(checked = results.len(), "checked the accounts' logins");
    inner.core().record_statuses(inner, results)
}

/// [`probe`] in the background: what could not be recorded is logged (the next check tries
/// again).
async fn probe_logged(inner: &Arc<Inner>, targets: Vec<Target>) {
    if let Err(why) = probe(inner, targets).await {
        tracing::warn!(%why, "recording the accounts' login checks failed");
    }
}

/// The background checks: once the hub is open, then every [`super::AgentsPoll::every`],
/// while it runs; and the ones asked for ([`Core::check_account_soon`]) as they come. Nothing
/// without a probe.
pub(super) fn start_probes(inner: &Arc<Inner>) {
    if inner.accounts.probe.is_none() {
        return;
    }
    tokio::spawn(probe_ticker(
        Arc::downgrade(inner),
        inner.state.subscribe(),
        inner.agents_poll.every,
    ));
    let asked = inner
        .accounts
        .asked_rx
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(rx) = asked {
        tokio::spawn(asked_checks(
            Arc::downgrade(inner),
            inner.state.subscribe(),
            rx,
        ));
    }
}

/// Runs the checks asked for ([`Core::check_account_soon`]), one at a time, while the hub
/// runs.
async fn asked_checks(
    hub: Weak<Inner>,
    mut state: tokio::sync::watch::Receiver<HubState>,
    mut rx: mpsc::UnboundedReceiver<Target>,
) {
    loop {
        let target = tokio::select! {
            t = rx.recv() => match t {
                Some(t) => t,
                None => return,
            },
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        };
        let Some(inner) = hub.upgrade() else {
            return;
        };
        tokio::select! {
            () = probe_logged(&inner, vec![target]) => {}
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        }
    }
}

async fn probe_ticker(
    hub: Weak<Inner>,
    mut state: tokio::sync::watch::Receiver<HubState>,
    every: Duration,
) {
    loop {
        {
            let Some(inner) = hub.upgrade() else {
                return;
            };
            let targets = inner.core().probe_targets(None, None);
            tokio::select! {
                () = probe_logged(&inner, targets) => {}
                _ = state.wait_for(|s| *s != HubState::Running) => return,
            }
        }
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        }
    }
}

impl Hub {
    /// `account.list {runtime?}`: every runtime's accounts (see [`Core::accounts_json`]),
    /// directories only `with_dirs` (a user's connection). `bad_request` for a `runtime` that
    /// takes no accounts and the registry has nothing of.
    pub fn account_list(&self, runtime: Option<&str>, with_dirs: bool) -> Result<Value, RpcError> {
        let core = self.inner.core();
        if let Some(rt) = runtime {
            if !core.accounts.runtimes().iter().any(|r| r == rt) {
                check_runtime(rt)?;
            }
        }
        Ok(core.accounts_json(runtime, with_dirs))
    }

    /// `account.status {runtime?, account?, refresh?}` (users only): `account.list`'s answer,
    /// after checking the logins again with `refresh` (`runtime`'s `account`, else `runtime`'s
    /// accounts, else every runtime's that has a named one; waited for at most
    /// [`REFRESH_WAIT`]). `internal` when what the checks found could not be recorded.
    pub async fn account_status(
        &self,
        p: AccountStatusParams,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        users_only(caller, "checks accounts")?;
        if p.account.is_some() && p.runtime.is_none() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "account needs its runtime: give runtime too",
            ));
        }
        if let Some(rt) = &p.runtime {
            check_runtime(rt)?;
        }
        if p.refresh {
            let targets = {
                let core = self.inner.core();
                if let (Some(rt), Some(name)) = (&p.runtime, &p.account) {
                    let known = core.accounts.info(rt, Some(name));
                    if known.as_ref().is_none_or(|a| a.removed) {
                        return Err(rpc(
                            codes::NOT_FOUND,
                            format!("{rt} has no account {name:?} (`twr account list`)"),
                        ));
                    }
                }
                core.probe_targets(p.runtime.as_deref(), p.account.as_deref())
            };
            match tokio::time::timeout(REFRESH_WAIT, probe(&self.inner, targets)).await {
                Ok(recorded) => recorded.map_err(|why| rpc(codes::INTERNAL, why))?,
                Err(_) => {
                    tracing::info!("the accounts' login checks took longer than {REFRESH_WAIT:?}");
                }
            }
        }
        Ok(self.inner.core().accounts_json(p.runtime.as_deref(), true))
    }

    /// `account.add {runtime, account, dir?, share?, native?, dry_run?}` (users only; see the
    /// module docs): the account as it is declared from now on, `{runtime, account, native,
    /// dir, share, source, config, existing, recorded}` (`config`: config.toml declares it,
    /// nothing is recorded; `existing`: it was there already, as given; `recorded`: this call
    /// recorded `account.added`). A real add that records one checks the account's login in
    /// the background. `bad_request` for a runtime that takes no accounts, a bad name or
    /// directory, `conflict` for what another account has or config says otherwise (a config
    /// or recorded account's own directory is checked again on every call, dry runs too:
    /// [`Hub::known_dir_free`]).
    pub fn account_add(&self, p: AccountAddParams, caller: &Caller) -> Result<Value, RpcError> {
        users_only(caller, "adds accounts")?;
        let rt = p.runtime.as_str();
        let name = p.account.as_str();
        check_runtime(rt)?;
        check_name(name).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        if p.native && p.dir.is_some() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "the native account has no dir: it is the runtime's own directory",
            ));
        }
        if let Some(d) = &p.dir {
            if !d.is_absolute() {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("dir {} is not an absolute path", d.display()),
                ));
            }
        }
        if let Some(n) = &p.after_native_name {
            if p.native || !p.dry_run {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    "after_native_name is for a dry run of a named account",
                ));
            }
            check_name(n)
                .map_err(|e| rpc(codes::BAD_REQUEST, format!("after_native_name: {e}")))?;
            if n == name {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "after_native_name {n} is the new account's name: the two need \
                         different names"
                    ),
                ));
            }
        }
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let out = if p.native {
            self.add_native(&mut core, &p, caller)?
        } else {
            if let Some(n) = &p.after_native_name {
                let rename = AccountAddParams {
                    runtime: p.runtime.clone(),
                    account: n.clone(),
                    native: true,
                    dry_run: true,
                    ..Default::default()
                };
                self.add_native(&mut core, &rename, caller)?;
            }
            self.add_named(&mut core, &p, caller)?
        };
        drop(core);
        if out["recorded"] == json!(true) {
            let target: Option<Target> = (!p.native).then(|| {
                let dir = out["dir"].as_str().map(PathBuf::from);
                (rt.to_string(), name.to_string(), false, dir)
            });
            let target = target.unwrap_or_else(|| (rt.to_string(), name.to_string(), true, None));
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move { probe_logged(&inner, vec![target]).await });
        }
        Ok(out)
    }

    /// [`Hub::account_add`] of a name for the native account.
    fn add_native(
        &self,
        core: &mut Core,
        p: &AccountAddParams,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let (rt, name) = (p.runtime.as_str(), p.account.as_str());
        let config_native = self
            .inner
            .config
            .accounts
            .get(rt)
            .and_then(|m| m.iter().find(|(_, a)| a.native))
            .map(|(n, _)| n.clone());
        let view = &core.accounts;
        let current = view.info(rt, None).expect("the native account");
        let reply = |a: &AccountInfo, config: bool, existing: bool, recorded: bool| {
            json!({
                "runtime": rt,
                "account": name,
                "native": true,
                "dir": Value::Null,
                "share": p.share.unwrap_or(a.share),
                "source": if recorded { Source::Recorded } else { a.source },
                "config": config,
                "existing": existing,
                "recorded": recorded,
            })
        };
        if let Some(cn) = config_native {
            if cn == name {
                return Ok(reply(&current, true, true, false));
            }
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "config.toml names {rt}'s native account {cn} ([accounts.{rt}.{cn}]); \
                     change it there and restart the hub"
                ),
            ));
        }
        // A name another account of the runtime has, or had: a removed one's threads keep
        // its name, and would be taken for the native account's.
        if let Some(other) = view.info(rt, Some(name)).filter(|a| !a.native) {
            let what = match other.removed {
                true => format!("was an account of {rt} (removed; its threads keep the name)"),
                false => format!("is another account of {rt} already ({})", origin(&other)),
            };
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} {what}: pick another name for the native account",
                    label(rt, name)
                ),
            ));
        }
        if current.name == name && current.source == Source::Recorded && !current.removed {
            return Ok(reply(&current, false, true, false));
        }
        if p.dry_run {
            return Ok(reply(&current, false, false, false));
        }
        let body = EventBody::AccountAdded(AccountAdded {
            runtime: rt.to_string(),
            account: name.to_string(),
            dir: None,
            share: p.share.unwrap_or_default(),
            native: true,
            by: ChangedBy::User,
            ..Default::default()
        });
        core.append(&self.inner, HUB_THREAD, None, caller.source(), body)
            .map_err(append_err)?;
        let now = core.accounts.info(rt, None).expect("the native account");
        Ok(reply(&now, false, false, true))
    }

    /// [`Hub::account_add`] of a named account.
    fn add_named(
        &self,
        core: &mut Core,
        p: &AccountAddParams,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let (rt, name) = (p.runtime.as_str(), p.account.as_str());
        // A dry run after a rename of the native account (`after_native_name`, checked by
        // the caller): the registry as that rename would leave it.
        let renamed = p.after_native_name.as_ref().map(|n| {
            let mut v = core.accounts.clone();
            v.apply(&twr_core::event::Event::new(
                HUB_THREAD,
                0,
                None,
                EvSource::Hub,
                EventBody::AccountAdded(AccountAdded {
                    runtime: rt.to_string(),
                    account: n.clone(),
                    native: true,
                    by: ChangedBy::User,
                    ..Default::default()
                }),
            ));
            v
        });
        let view = renamed.as_ref().unwrap_or(&core.accounts);
        let native_name = view.native_name(rt);
        // (Unless a live named account has the name too: the name is that one's, `shadowed`.)
        let shadowed = view.info(rt, None).is_some_and(|a| a.shadowed);
        if name == native_name && !shadowed {
            // (A removed native account keeps its name: `name-native` adds it back.)
            let removed = if view.info(rt, None).is_some_and(|a| a.removed) {
                format!(", removed: twr account name-native {rt} {name} adds it back")
            } else {
                String::new()
            };
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} is {rt}'s native account (its own directory{removed}); pick another \
                     name for the new one",
                    label(rt, name)
                ),
            ));
        }
        let reply = |dir: &Path, share: Share, source: Source, flags: [bool; 3]| {
            let [config, existing, recorded] = flags;
            json!({
                "runtime": rt,
                "account": name,
                "native": false,
                "dir": dir,
                "share": share,
                "source": source,
                "config": config,
                "existing": existing,
                "recorded": recorded,
            })
        };
        let declared = self
            .inner
            .config
            .accounts
            .get(rt)
            .and_then(|m| m.get(name))
            .is_some_and(|a| !a.native);
        let known = view.info(rt, Some(name)).filter(|a| !a.native);
        // Config's declaration wins: nothing to record.
        if declared {
            let a = known.expect("a config account");
            let dir = a.dir.clone().unwrap_or_default();
            if let Some(given) = p.dir.as_deref().filter(|d| !same_dir(d, &dir)) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} is declared in config.toml ([accounts.{rt}.{name}]) with dir {}, \
                         not {}: config.toml wins (change it there and restart the hub)",
                        label(rt, name),
                        dir.display(),
                        given.display()
                    ),
                ));
            }
            self.known_dir_free(view, &a)?;
            return Ok(reply(&dir, a.share, a.source, [true, true, false]));
        }
        // A live record: the same again is nothing new; another directory or share is refused.
        if let Some(a) = known.filter(|a| !a.removed) {
            let dir = a.dir.clone().unwrap_or_default();
            let other_dir = p.dir.as_deref().filter(|d| !same_dir(d, &dir));
            let other_share = p.share.filter(|s| *s != a.share);
            if other_dir.is_some() || other_share.is_some() {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{} is an account already (dir {}, share {}): remove it first \
                         (`twr account remove {rt} {name}`) to add it with another directory \
                         or share",
                        label(rt, name),
                        dir.display(),
                        serde_json::to_value(a.share)
                            .unwrap_or_default()
                            .as_str()
                            .unwrap_or("")
                    ),
                ));
            }
            self.known_dir_free(view, &a)?;
            return Ok(reply(&dir, a.share, a.source, [false, true, false]));
        }
        // New (or again after its removal).
        // The directory twr places under the accounts root (none given, or that one given
        // again: the CLI's add sends what its preflight answered) is checked on resolved paths
        // (no link on the way leads outside the root), the root's `<runtime>` made if missing.
        let root = &self.inner.accounts.root;
        let default = default_dir(root, rt, name);
        let dir = p.dir.clone().unwrap_or_else(|| default.clone());
        if dir == default {
            let parent = root.join(rt);
            if default.parent() != Some(parent.as_path())
                || default.file_name() != Some(std::ffi::OsStr::new(name))
            {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{} is not inside {}", default.display(), parent.display()),
                ));
            }
            twr_core::account::resolve_in_root(root, rt, &default, true)
                .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        }
        if let Some(owner) = native_owner(&self.inner.accounts.native_dir, &dir) {
            let hint = if owner == rt {
                format!(
                    ": it is the native account (`twr account name-native {rt} {name}` names it)"
                )
            } else {
                String::new()
            };
            return Err(rpc(
                codes::CONFLICT,
                format!("{} is {owner}'s own directory{hint}", dir.display()),
            ));
        }
        if let Some(other) = dir_owner(view, &dir, rt, name) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} is {}'s directory already ({}): each account has its own",
                    dir.display(),
                    label(&other.runtime, &other.name),
                    origin(&other)
                ),
            ));
        }
        let share = p.share.unwrap_or_default();
        if p.dry_run {
            return Ok(reply(&dir, share, Source::Recorded, [false, false, false]));
        }
        debug_assert!(renamed.is_none(), "after_native_name is for dry runs only");
        let body = EventBody::AccountAdded(AccountAdded {
            runtime: rt.to_string(),
            account: name.to_string(),
            dir: Some(dir.to_string_lossy().into_owned()),
            share,
            native: false,
            by: ChangedBy::User,
            ..Default::default()
        });
        core.append(&self.inner, HUB_THREAD, None, caller.source(), body)
            .map_err(append_err)?;
        Ok(reply(&dir, share, Source::Recorded, [false, false, true]))
    }

    /// [`Hub::add_named`]'s checks of an account config declares or twr has recorded (not
    /// removed), on every call, dry runs too (the CLI's add asks again once it has made the
    /// directory: a path that did not resolve before may be another's now): its default
    /// directory still inside the accounts root once links are followed
    /// (`twr_core::account::resolve_in_root`, the root's `<runtime>` made if missing), and
    /// neither a runtime's own directory nor another account's ([`conflict`], whose fix is
    /// config.toml's for a config account).
    fn known_dir_free(&self, view: &AccountsView, a: &AccountInfo) -> Result<(), RpcError> {
        let root = &self.inner.accounts.root;
        let cant = |why: String| format!("{} can't be used: {why}", label(&a.runtime, &a.name));
        if let Some(dir) = a
            .dir
            .as_deref()
            .filter(|d| *d == default_dir(root, &a.runtime, &a.name))
        {
            twr_core::account::resolve_in_root(root, &a.runtime, dir, true)
                .map_err(|e| rpc(codes::BAD_REQUEST, cant(e)))?;
        }
        match conflict(view, &self.inner.accounts.native_dir, a) {
            Some(why) => Err(rpc(codes::CONFLICT, cant(why))),
            None => Ok(()),
        }
    }

    /// `account.remove {runtime, account, move_to?, force?}` (users only; see the module
    /// docs): `{runtime, account, native, dir, removed: true, config, threads, moved,
    /// running, message, dispatch_error?}` (`dir`: the directory of the account removed, as
    /// the registry had it, null for the native account; `config`: config.toml still declares
    /// it, only its record went; `threads`: those left on it; `moved`: those moved to
    /// `move_to`, whose next turn starts a new session there; `running`: `[{thread,
    /// thread_name}]`, those whose runtime process still runs on the removed login (a moved
    /// one finishing its turn, one stopping, one left on it), [`Core::running_on_removed`],
    /// each with `runtime_account_removed` in its thread object until it no longer does;
    /// `dispatch_error`: stopping a moved one's runtime failed). With `move_to`, the threads on it whose log names no project for
    /// their pins have their repository's looked up first, before the core is locked
    /// (`Hub::look_up_pins_project`), as `thread.set_model` does: once the core is locked again,
    /// a thread on it that this call has not looked up yet (one that came onto it meanwhile) is
    /// looked up too before anything moves.
    /// `not_found` for an account the runtime does not have, `bad_request` for one config.toml
    /// declares and twr has no record of, or a `move_to` that is no other account of the
    /// runtime, `conflict` while threads run on it without `move_to` or `force`, when the
    /// current orchestrator runs on it with `move_to` (it moves by a switch: `twr orchestrator
    /// use`), and when one of them cannot move to `move_to` (each listed with why; nothing is
    /// recorded then).
    pub async fn account_remove(
        &self,
        p: AccountRemoveParams,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let mut looked_up = BTreeSet::new();
        loop {
            match self.remove_account_now(&p, caller, &looked_up)? {
                Removal::Done(out) => return Ok(out),
                // Each round looks up at least one thread more: it ends.
                Removal::LookUp(ids) => {
                    for id in ids {
                        self.look_up_pins_project(&id).await;
                        looked_up.insert(id);
                    }
                }
            }
        }
    }

    /// [`Hub::account_remove`] under the core lock: [`Removal::LookUp`] the threads that would
    /// move to `move_to` and are not in `looked_up` (their projects to look up first; nothing
    /// is recorded then).
    fn remove_account_now(
        &self,
        p: &AccountRemoveParams,
        caller: &Caller,
        looked_up: &BTreeSet<String>,
    ) -> Result<Removal, RpcError> {
        users_only(caller, "removes accounts")?;
        let (rt, name) = (p.runtime.as_str(), p.account.as_str());
        check_runtime(rt)?;
        check_name(name).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let move_to = p.move_to.as_deref().map(str::trim);
        if let Some(to) = move_to {
            check_name(to).map_err(|e| rpc(codes::BAD_REQUEST, format!("move_to: {e}")))?;
        }
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let Some(a) = core.accounts.info(rt, Some(name)) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("{rt} has no account {name:?} (`twr account list`)"),
            ));
        };
        if a.removed {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("{} was removed already", label(rt, name)),
            ));
        }
        let config = matches!(a.source, Source::Config | Source::Both);
        if a.source == Source::Config {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is declared in config.toml ([accounts.{rt}.{name}]); remove it there \
                     and restart the hub",
                    label(rt, name)
                ),
            ));
        }
        if let Some(to) = move_to {
            let target = core.accounts.resolve(rt, to);
            let same = target.as_ref().is_some_and(|t| match t {
                Some(t) => t == name,
                None => a.native,
            });
            if target.is_none() || same {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "move_to must be another account of {rt} than {name} (`twr account \
                         list`)"
                    ),
                ));
            }
        }
        // Only the record goes from a config account: its threads keep it, from config. The
        // record is of the kind it is, whatever kind config gives the account (a named record
        // config declares native, say: `AccountsView::live_record`).
        let record_native = match config {
            true => core
                .accounts
                .live_record(rt, name, a.native)
                .unwrap_or(a.native),
            false => a.native,
        };
        let mut on_it: Vec<(String, String)> = Vec::new();
        if !config {
            on_it = core.threads_on(rt, name, a.native);
        }
        // The current orchestrator among them moves by a switch only (a replacement with a
        // handoff, `hub::orch`), never by `move_to`'s `thread.set_model`.
        let orch = on_it
            .iter()
            .map(|(id, _)| id.as_str())
            .find(|id| {
                core.registry
                    .get(id)
                    .is_some_and(ThreadView::is_current_orchestrator)
            })
            .map(str::to_string);
        if let (Some(to), Some(_)) = (move_to, &orch) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "the orchestrator runs on {}: switch it first (twr orchestrator use \
                     {rt}:{to}), then remove the account",
                    label(rt, name)
                ),
            ));
        }
        if !on_it.is_empty() && !p.force && move_to.is_none() {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} has threads on it: {}. Move them to another account first (move_to), \
                     or remove it with force: they keep it and cannot start until they move",
                    label(rt, name),
                    listed(&on_it, orch.as_deref(), rt, None)
                ),
            ));
        }
        // Every move checked before anything is recorded: all of them, or none.
        let mut moves = Vec::new();
        if let Some(to) = move_to.filter(|_| !on_it.is_empty()) {
            let missing: Vec<String> = on_it
                .iter()
                .map(|(id, _)| id)
                .filter(|id| !looked_up.contains(*id))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Ok(Removal::LookUp(missing));
            }
            let req = super::ModelRequest {
                account: Some(to),
                ..Default::default()
            };
            let mut refused = Vec::new();
            for (id, thread) in &on_it {
                match core.model_change(&self.inner, id, &req, None, caller) {
                    Ok(super::model::ModelChange {
                        event: Some(ev), ..
                    }) => moves.push((id.clone(), ev)),
                    Ok(_) => refused.push(format!("{thread} (already on {})", label(rt, to))),
                    Err(e) => refused.push(format!("{thread} ({})", e.message)),
                }
            }
            if !refused.is_empty() {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "cannot move every thread on {} to {}, so nothing was removed: {}",
                        label(rt, name),
                        label(rt, to),
                        refused.join("; ")
                    ),
                ));
            }
        }
        let mut batch = Batch::default();
        for (id, ev) in &moves {
            batch.push(id, None, caller.source(), ev.clone());
        }
        batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::AccountRemoved(AccountRemoved {
                runtime: rt.to_string(),
                account: name.to_string(),
                native: record_native,
                record_only: config,
                by: ChangedBy::User,
                ..Default::default()
            }),
        );
        core.append_batch(&self.inner, batch).map_err(append_err)?;
        // The moves apply as `thread.set_model`'s do: an idle runtime stops now, a busy one
        // after its turn.
        let mut dispatch_errors = Vec::new();
        for (id, _) in &moves {
            let mut err = None;
            core.apply_settings(&self.inner, id, &mut err);
            dispatch_errors.extend(err);
        }
        // What still runs on the login: a busy thread that moved finishes its turn there, an
        // idle one is stopping, one left on it (`force`) runs on until it stops.
        let removed_dir = a.dir.as_deref().filter(|_| !a.native);
        let running: Vec<Value> = core
            .running_on_removed(rt, name, removed_dir)
            .into_iter()
            .map(|(id, thread)| json!({ "thread": id, "thread_name": thread }))
            .collect();
        let moved: Vec<String> = if moves.is_empty() {
            Vec::new()
        } else {
            names(&on_it)
        };
        let threads: Vec<String> = if moves.is_empty() {
            names(&on_it)
        } else {
            Vec::new()
        };
        let message = if config {
            format!(
                "removed twr's record of {}; config.toml still declares it \
                 ([accounts.{rt}.{name}]), so it stays",
                label(rt, name)
            )
        } else if let (Some(to), false) = (move_to, moved.is_empty()) {
            format!(
                "removed {}; {} moved to {}: their next turns start new sessions there with a \
                 handoff",
                label(rt, name),
                moved.join(", "),
                label(rt, to)
            )
        } else if threads.is_empty() {
            format!("removed {}", label(rt, name))
        } else {
            format!(
                "removed {}; {} stay on it and cannot start until they move",
                label(rt, name),
                listed(&on_it, orch.as_deref(), rt, None)
            )
        };
        let mut out = json!({
            "runtime": rt,
            "account": name,
            "native": a.native,
            "dir": a.dir.as_ref().filter(|_| !a.native),
            "removed": true,
            "config": config,
            "threads": threads,
            "moved": moved,
            "running": running,
            "message": message,
        });
        if !dispatch_errors.is_empty() {
            out["dispatch_error"] = json!(dispatch_errors.join("; "));
        }
        Ok(Removal::Done(out))
    }
}

/// What [`Hub::remove_account_now`] did.
enum Removal {
    /// The account's removal (or its record's), its answer.
    Done(Value),
    /// Nothing yet: these threads move with it, and their projects are to be looked up first
    /// (`Hub::look_up_pins_project`).
    LookUp(Vec<String>),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as Map;
    use twr_core::config::AccountConfig;

    /// A directory inside another, as written or through a link; not the directory itself,
    /// nor a sibling that shares its name's start.
    #[test]
    fn inside_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("acc");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::os::unix::fs::symlink(&d, tmp.path().join("link")).unwrap();
        assert!(inside(&d.join("sub"), &d));
        assert!(inside(&d.join("missing/deeper"), &d));
        assert!(inside(&tmp.path().join("link/sub"), &d));
        assert!(inside(&d.join("sub"), &tmp.path().join("link")));
        assert!(!inside(&d, &d));
        assert!(!inside(&tmp.path().join("link"), &d));
        assert!(!inside(&tmp.path().join("acc2"), &d));
        assert!(!inside(&d, &d.join("sub")));
    }

    fn native_dirs(dirs: &[(&str, &str)]) -> NativeDirFn {
        let m: Map<String, PathBuf> = dirs
            .iter()
            .map(|(r, d)| (r.to_string(), PathBuf::from(d)))
            .collect();
        Arc::new(move |rt: &str| m.get(rt).cloned())
    }

    fn config(entries: &[(&str, &str, Option<&str>)]) -> Config {
        let mut c = Config::default();
        for (rt, name, dir) in entries {
            c.accounts.entry(rt.to_string()).or_default().insert(
                name.to_string(),
                AccountConfig {
                    dir: dir.map(PathBuf::from),
                    ..Default::default()
                },
            );
        }
        c
    }

    /// `account.list`'s `native_dir` and `native_var`: the hub's directory, and its selector
    /// set (with its value, an empty one too), unset, or set to what is no UTF-8 (no value).
    #[test]
    fn native_json_says_the_hubs_directory_and_selector() {
        use std::os::unix::ffi::OsStringExt;
        let dirs = native_dirs(&[("codex", "/x/codex")]);
        let var: NativeVarFn = Arc::new(|k: &str| match k {
            "CODEX_HOME" => Some("/x/codex".into()),
            "GROK_HOME" => Some("".into()),
            "CLAUDE_CONFIG_DIR" => Some(std::ffi::OsString::from_vec(vec![0xff])),
            _ => None,
        });
        assert_eq!(
            native_json("codex", &dirs, &var),
            Some((
                json!("/x/codex"),
                json!({"name": "CODEX_HOME", "set": true, "value": "/x/codex"})
            ))
        );
        assert_eq!(
            native_json("grok-build", &dirs, &var),
            Some((
                Value::Null,
                json!({"name": "GROK_HOME", "set": true, "value": ""})
            ))
        );
        assert_eq!(
            native_json("claude-code", &dirs, &var).unwrap().1,
            json!({"name": "CLAUDE_CONFIG_DIR", "set": true})
        );
        let unset: NativeVarFn = Arc::new(|_: &str| None);
        assert_eq!(
            native_json("codex", &dirs, &unset).unwrap().1,
            json!({"name": "CODEX_HOME", "set": false})
        );
        assert_eq!(native_json("fake", &dirs, &var), None);
    }

    #[test]
    fn the_start_refuses_native_and_shared_directories() {
        let nd = native_dirs(&[("claude-code", "/h/.claude"), ("codex", "/h/.codex")]);
        let c = config(&[("claude-code", "work", Some("/h/.claude/"))]);
        let v = AccountsView::new(&c).with_default_root("/data/accounts");
        let e = check_at_start(&v, &c, &nd).unwrap_err().to_string();
        assert_eq!(
            e,
            "[accounts.claude-code.work] dir is claude-code's own directory: use native = true"
        );
        let c = config(&[("codex", "team", Some("/h/.claude"))]);
        let v = AccountsView::new(&c);
        let e = check_at_start(&v, &c, &nd).unwrap_err().to_string();
        assert!(e.contains("claude-code's own directory: give it"), "{e}");
        // A record in a config account's directory.
        let c = config(&[("codex", "team", Some("/w"))]);
        let mut v = AccountsView::new(&c).with_default_root("/data/accounts");
        v.apply(&twr_core::event::Event::new(
            HUB_THREAD,
            1,
            None,
            EvSource::Hub,
            EventBody::AccountAdded(AccountAdded {
                runtime: "claude-code".into(),
                account: "side".into(),
                dir: Some("/w".into()),
                ..Default::default()
            }),
        ));
        assert_eq!(
            check_at_start(&v, &c, &nd).unwrap(),
            vec![
                "claude-code:side can't be used: its directory /w is codex:team's too \
                 ([accounts.codex.team]): remove the account (twr account remove claude-code \
                 side)"
                    .to_string()
            ]
        );
        // Two config accounts in one directory: the hub does not start.
        let c = config(&[
            ("codex", "a", Some("/same")),
            ("claude-code", "b", Some("/same")),
        ]);
        let e = check_at_start(&AccountsView::new(&c), &c, &nd)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("[accounts.claude-code.b] and [accounts.codex.a] have one directory"),
            "{e}"
        );
        // Records: in a runtime's own directory (the hub's environment changed since), or two
        // in one directory (each flagged); a removed one is not looked at.
        let c = Config::default();
        let mut v = AccountsView::new(&c);
        let added = |v: &mut AccountsView, seq: u64, rt: &str, name: &str, dir: &str| {
            v.apply(&twr_core::event::Event::new(
                HUB_THREAD,
                seq,
                None,
                EvSource::Hub,
                EventBody::AccountAdded(AccountAdded {
                    runtime: rt.into(),
                    account: name.into(),
                    dir: Some(dir.into()),
                    ..Default::default()
                }),
            ));
        };
        added(&mut v, 1, "claude-code", "own", "/h/.codex");
        added(&mut v, 2, "codex", "x", "/two");
        added(&mut v, 3, "codex", "y", "/two");
        added(&mut v, 4, "grok-build", "gone", "/h/.claude");
        v.apply(&twr_core::event::Event::new(
            HUB_THREAD,
            5,
            None,
            EvSource::Hub,
            EventBody::AccountRemoved(AccountRemoved {
                runtime: "grok-build".into(),
                account: "gone".into(),
                ..Default::default()
            }),
        ));
        let w = check_at_start(&v, &c, &nd).unwrap();
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(
            w[0],
            "claude-code:own can't be used: its directory /h/.codex is codex's own directory in \
             the hub's environment (CODEX_HOME, or its default): set CODEX_HOME elsewhere for \
             the hub and restart it, or remove the account (twr account remove claude-code own)"
        );
        assert!(w[1].starts_with("codex:x can't be used: its directory /two is codex:y's too (codex:y (twr account add))"), "{}", w[1]);
        assert!(
            w[2].starts_with("codex:y can't be used: its directory /two is codex:x's too"),
            "{}",
            w[2]
        );
        // Defaults under the root are each their own.
        let c = config(&[("codex", "a", None), ("codex", "b", None)]);
        let v = AccountsView::new(&c).with_default_root("/data/accounts");
        assert!(check_at_start(&v, &c, &nd).unwrap().is_empty());
    }

    #[test]
    fn logins_from_agent_infos() {
        let info = |found: bool, login: AgentLogin| AgentInfo {
            found,
            login,
            ..Default::default()
        };
        assert_eq!(login_of(&info(false, AgentLogin::LoggedIn)), Login::Missing);
        assert_eq!(login_of(&info(true, AgentLogin::LoggedIn)), Login::LoggedIn);
        assert_eq!(
            login_of(&info(true, AgentLogin::LoggedOut)),
            Login::LoggedOut
        );
        assert_eq!(login_of(&info(true, AgentLogin::Unknown)), Login::Unknown);
    }
}

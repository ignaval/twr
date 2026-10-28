//! Which account a thread runs on (`twr_core::account`), and what that means for its
//! starts.
//!
//! **A new thread's account** ([`choose_account`], from `thread.new` right after its runtime),
//! the first of these that applies:
//!
//! 1. **Explicit**: `thread.new` `account`, or the account part of its `runtime`
//!    (`claude-code:work`, `hub::runtime_spec`); both given, they must agree. A name, a glob or
//!    a comma-separated list ([`Pin`]): its first match that is not logged out or missing,
//!    preferring one the project's pin allows. One outside the project's pin, or inside its
//!    exclusions, is refused for the orchestrator always, and for the user unless
//!    `outside_pin` (reason `override`).
//! 2. **Project**: the box project's pin, else the project's (`accounts.<runtime>` over
//!    `account`; `Config::account_pin`), minus its exclusions: the first usable match. A pin
//!    is a promise about where the project's code goes, so one that matches no account, or
//!    none usable, refuses the thread. A thread made for a repository path gets the pins of
//!    the configured project of that repository however the path names it (a symlink, a
//!    subdirectory, another worktree, or a folder of it: `Hub::pins_project` in
//!    `hub::threads`), recorded as `ext.twr.account_project` for its moves
//!    (`ThreadView::pins_project`); one whose log names none (made before pins were recorded,
//!    or before its project was configured) has it looked up at the move
//!    (`Hub::look_up_pins_project`).
//! 3. **Default**: `[account_policy.<runtime>] default`, on the hub's own box only (an
//!    excluded one falls through to the native account).
//! 4. **Native**: the runtime's own login, refused when it was removed (or excluded).
//!
//! A runtime that takes no accounts refuses an explicit account and a project pin alike. On
//! another box than the hub's own, an explicit account, or a pin that picks a named account,
//! is refused: accounts there are the box's own logins (phase 5); a pin there matches the
//! native account alone (one that matches only named accounts is refused so, whatever their
//! state here). So the registry's native
//! account (the hub's own login) says nothing of a thread there: its removal or its login
//! state neither refuses a new thread there nor stops one from starting, and such threads are
//! not counted as the registry's (`account.list`, `account.remove`). A named account (recorded
//! or config's) whose directory collides, now, with a runtime's own directory or another
//! account's (`hub::accounts::conflict`) is refused for new threads and moves
//! ([`Core::pick_account`]). The choice is recorded on `thread.created` as `ext.twr.account`
//! and `account_dir` (a named account only, its directory as it really is: links followed,
//! [`resolved`]), `account_reason` (`explicit`, `project`, `default`, `override`, and
//! `config` for the orchestrator's `[orchestrator] account`; absent for the native account
//! nothing else chose) and `account_pin` (the explicit pin). Under the lock it is recorded
//! with, the account is checked again as a start would check it ([`Core::recheck_account`]),
//! and one that had to be usable (any but the native account nothing else chose, or a
//! remote box's own login) is refused as it would have been when chosen if a check found it
//! logged out or missing meanwhile. A move's destination, and a new orchestrator's, are
//! checked so under the lock before anything is recorded (or the current orchestrator
//! stopped).
//! The orchestrator's account is chosen the same way, without a project (`hub::orch`); its
//! refusals say how to switch it (`twr orchestrator use <runtime>:<account>`, `[orchestrator]
//! account`), never `twr model`.
//!
//! **Its environment** is the account's, set in `hub::spawn_config` (the one place every start,
//! attach and job takes its configuration from).
//!
//! **Its starts** ([`Core::refuse_account`]: a runtime's start, an attach, a job) are refused
//! while its account is removed (or unknown; the native account only on the hub's own box),
//! collides (`hub::accounts::conflict`), its directory is not the one recorded when the
//! thread got it (config changed, it was removed and added again elsewhere, or a link it is
//! retargeted: the session lives in the old directory), or its directory is missing. Its
//! runtimes are given the account's directory by the path recorded with it (the path its
//! login used, [`Core::account_env_dir`]) while that leads to the recorded directory; one
//! that no longer does (a link removed or retargeted) refuses its starts too, rather than
//! going on through another path to the directory (a runtime may key a login on the path:
//! Claude Code's macOS Keychain entry), until a move to the same account starts a new session
//! through the configured path. A thread recorded before that path was is given the
//! configured path while it leads to the recorded directory. A recorded path that still leads
//! there while config names the directory by another path is kept (the session goes on), and
//! `twr account login` signs in the configured one: an explicit move to the same account
//! (`twr model T --account <name>`) rebinds the thread to the configured path, a new session
//! ([`Core::retained_env_dir`]), and its sign-in failures name both steps (`hub::retry`).

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use twr_core::account::{takes_accounts, AccountInfo, AccountsView, Login, Pin};
use twr_core::config::{BoxProjectConfig, Config};
use twr_core::event::{
    EventBody, ACCOUNT_DIR_KEY, ACCOUNT_ENV_DIR_KEY, ACCOUNT_KEY, ACCOUNT_PIN_KEY,
    ACCOUNT_REASON_KEY,
};
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

use super::accounts::same_dir;
use super::{rpc, Core, Inner, LOCAL_BOX};

/// `account_reason` of an account the thread asked for.
pub(super) const REASON_EXPLICIT: &str = "explicit";
/// `account_reason` of an account the project's pin chose.
pub(super) const REASON_PROJECT: &str = "project";
/// `account_reason` of the runtime's `[account_policy] default`.
pub(super) const REASON_DEFAULT: &str = "default";
/// `account_reason` of the native account nothing else chose.
pub(super) const REASON_NATIVE: &str = "native";
/// `account_reason` of an account the user chose outside the project's pin (`outside_pin`).
pub(super) const REASON_OVERRIDE: &str = "override";
/// `account_reason` of the first orchestrator's configured account (`[orchestrator] account`,
/// or the account part of `[orchestrator] runtime`; `hub::orch`).
pub(super) const REASON_CONFIG: &str = "config";

/// The `REASON_*` constant recorded as `reason` (`ext.twr.account_reason`); `None` for one
/// this build does not know.
pub(super) fn reason_of(reason: &str) -> Option<&'static str> {
    [
        REASON_EXPLICIT,
        REASON_PROJECT,
        REASON_DEFAULT,
        REASON_NATIVE,
        REASON_OVERRIDE,
        REASON_CONFIG,
    ]
    .into_iter()
    .find(|r| *r == reason)
}

/// The account a thread gets ([`choose_account`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct AccountChoice {
    /// The named account; `None`: the native one.
    pub(super) account: Option<String>,
    /// The named account's directory, links followed.
    pub(super) dir: Option<PathBuf>,
    /// The named account's directory as configured (the path its runtimes get).
    pub(super) env_dir: Option<PathBuf>,
    /// Why (`REASON_*`); empty for a runtime that takes no accounts (nothing recorded).
    pub(super) reason: &'static str,
    /// The thread's own pin (the explicit one), normalized ([`Pin`]'s display form).
    pub(super) pin: Option<String>,
    /// When the choice required a usable account (an explicit one, a pin's, the policy's
    /// default; not the native account nothing else chose, nor a remote box's own login): the
    /// head of the refusal of one logged out or missing ([`unusable_message`]), for the
    /// recheck under the lock the thread is recorded with ([`AccountChoice::recheck`]).
    pub(super) login_head: Option<String>,
}

impl AccountChoice {
    /// Refuses (`conflict`) an account `accounts` no longer has as it was chosen on
    /// `runtime` for a thread on box `box_name`: a named one removed, or in another directory
    /// (links followed), and the native one removed (renamed is still it) when the box is the
    /// hub's own (elsewhere it is the box's own login); and (`bad_request`, as when it was
    /// chosen) one that had to be usable ([`AccountChoice::login_head`]) and is logged out or
    /// missing now. Checked again under the lock the thread is recorded with.
    pub(super) fn recheck(
        &self,
        accounts: &AccountsView,
        runtime: &str,
        box_name: &str,
    ) -> Result<(), RpcError> {
        if self.reason.is_empty() {
            return Ok(());
        }
        let local = box_name == LOCAL_BOX;
        let Some(name) = &self.account else {
            return match accounts.info(runtime, None).filter(|a| a.removed && local) {
                Some(native) => Err(changed(runtime, &native.name)),
                None => self.recheck_login(accounts.info(runtime, None), runtime, local),
            };
        };
        let same = accounts.resolve(runtime, name) == Some(Some(name.clone()))
            && accounts
                .info(runtime, Some(name))
                .and_then(|a| a.dir)
                .map(resolved)
                == self.dir;
        if !same {
            return Err(changed(runtime, name));
        }
        self.recheck_login(accounts.info(runtime, Some(name)), runtime, local)
    }

    /// [`AccountChoice::recheck`]'s login check of the chosen account `a` (`local`: on the
    /// hub's own box).
    fn recheck_login(
        &self,
        a: Option<AccountInfo>,
        runtime: &str,
        local: bool,
    ) -> Result<(), RpcError> {
        let (Some(head), Some(a), true) = (&self.login_head, a, local) else {
            return Ok(());
        };
        if a.login.is_none_or(Login::counts) {
            return Ok(());
        }
        Err(rpc(
            codes::BAD_REQUEST,
            unusable_message(head, runtime, &[(&a, false)]),
        ))
    }

    /// Adds its `thread.created` `ext.twr` entries to `twr`.
    pub(super) fn record(&self, twr: &mut Map<String, Value>) {
        if self.reason.is_empty() {
            return;
        }
        if let Some(a) = &self.account {
            twr.insert(ACCOUNT_KEY.into(), json!(a));
        }
        if let Some(d) = &self.dir {
            twr.insert(ACCOUNT_DIR_KEY.into(), json!(d.to_string_lossy()));
        }
        if let Some(d) = &self.env_dir {
            twr.insert(ACCOUNT_ENV_DIR_KEY.into(), json!(d.to_string_lossy()));
        }
        // (Absent: the native account nothing else chose.)
        if self.reason != REASON_NATIVE {
            twr.insert(ACCOUNT_REASON_KEY.into(), json!(self.reason));
        }
        if let Some(p) = &self.pin {
            twr.insert(ACCOUNT_PIN_KEY.into(), json!(p));
        }
    }
}

/// A pin, or exclusions, and where they are configured: `[projects.<p>]` or
/// `[boxes.<b>.projects.<p>]`.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct PinFrom {
    pub(super) pin: Pin,
    pub(super) table: String,
}

/// What a project says about its threads' accounts on one runtime and box
/// ([`project_pins`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct ProjectPins {
    /// The configured project (`None`: a repository no configured project has, which
    /// nothing pins).
    pub(super) project: Option<String>,
    /// Its pin on the runtime.
    pub(super) pin: Option<PinFrom>,
    /// Its exclusions.
    pub(super) exclude: Option<PinFrom>,
    /// A thread being moved (`thread.set_model`): its name and its own pin
    /// (`ThreadView::account_pin`), which the account it moves to must match too.
    pub(super) own: Option<(String, Pin)>,
}

impl ProjectPins {
    fn excluded(&self, name: &str) -> bool {
        self.exclude.as_ref().is_some_and(|e| e.pin.matches(name))
    }

    /// Whether the project lets its threads use account `name`: its pin matches it (or there
    /// is none), and its exclusions do not; and so does the moved thread's own pin.
    fn allows(&self, name: &str) -> bool {
        self.pin.as_ref().is_none_or(|p| p.pin.matches(name))
            && !self.excluded(name)
            && self.own.as_ref().is_none_or(|(_, p)| p.matches(name))
    }

    /// What keeps a thread off account `name` ([`ProjectPins::allows`] is false): the
    /// project's exclusions, else its pin, else the thread's own pin.
    fn keeps_off(&self, name: &str) -> String {
        match (&self.exclude, &self.pin, &self.own) {
            (Some(e), _, _) if e.pin.matches(name) => {
                format!("project {} excludes account {}", self.project(), e.pin)
            }
            (_, Some(p), _) if !p.pin.matches(name) => {
                format!("project {} pins account {}", self.project(), p.pin)
            }
            (_, _, Some((thread, p))) => format!("{thread} is pinned to account {p}"),
            // `allows` is false only with a pin or an exclusion.
            _ => format!("project {} pins no account", self.project()),
        }
    }

    fn project(&self) -> &str {
        self.project.as_deref().unwrap_or_default()
    }
}

/// The pins of configured project `project` for a thread on box `box_name` running `runtime`
/// ([`pins_of`]), with the box's projects as it was added (which may be after the hub
/// started). Nothing for a repository no configured project has (`None`).
pub(super) fn project_pins(
    hub: &Inner,
    project: Option<&str>,
    box_name: &str,
    runtime: &str,
) -> ProjectPins {
    let Some(project) = project else {
        return ProjectPins::default();
    };
    let bp = (box_name != LOCAL_BOX)
        .then(|| hub.box_config(box_name))
        .flatten()
        .and_then(|b| b.projects.get(project).cloned())
        .or_else(|| {
            hub.config
                .boxes
                .get(box_name)
                .and_then(|b| b.projects.get(project).cloned())
        });
    pins_of(&hub.config, bp.as_ref(), project, box_name, runtime)
}

/// The pins of `config`'s project `project` on `runtime` ([`Config::account_pin`]), with
/// box `box_name`'s project `bp` (when it has one) replacing them: its `accounts.<runtime>`,
/// else its `account`, over the project's pin; its `exclude_accounts` over the project's.
fn pins_of(
    config: &Config,
    bp: Option<&BoxProjectConfig>,
    project: &str,
    box_name: &str,
    runtime: &str,
) -> ProjectPins {
    let (pin, exclude) = config.account_pin(Some(project), None, runtime);
    let table = format!("[projects.{project}]");
    let mut out = ProjectPins {
        project: Some(project.to_string()),
        pin: pin.map(|pin| PinFrom {
            pin,
            table: table.clone(),
        }),
        exclude: exclude.map(|pin| PinFrom { pin, table }),
        own: None,
    };
    if let Some(bp) = bp {
        let table = format!("[boxes.{box_name}.projects.{project}]");
        if let Some(pin) = bp.accounts.get(runtime).or(bp.account.as_ref()) {
            out.pin = Some(PinFrom {
                pin: pin.clone(),
                table: table.clone(),
            });
        }
        if let Some(pin) = &bp.exclude_accounts {
            out.exclude = Some(PinFrom {
                pin: pin.clone(),
                table,
            });
        }
    }
    out
}

/// What a new thread asks for ([`choose_account`]).
pub(super) struct AccountAsk<'a> {
    /// Its runtime (canonical: `acp` for a preset).
    pub(super) runtime: &'a str,
    /// Its own pin ([`explicit_pin`]).
    pub(super) explicit: Option<Pin>,
    /// The user's override of the project's pin.
    pub(super) outside_pin: bool,
    /// Asked by the orchestrator.
    pub(super) orchestrator: bool,
    /// The account of an orchestrator (`hub::orch`), for the refusals' wording: it moves
    /// with `twr orchestrator use <runtime>:<account>`, not `--account`.
    pub(super) for_orchestrator: bool,
    /// Its box.
    pub(super) box_name: &'a str,
    /// Its project's pins ([`project_pins`]).
    pub(super) pins: &'a ProjectPins,
    /// The name of the thread being moved to the account (`thread.set_model`), for the
    /// refusals' wording; `None`: a new thread.
    pub(super) moving: Option<&'a str>,
}

/// The thread's own pin: `account` (`thread.new` / MCP), else `runtime_account` (the account
/// part of its `runtime`, already checked); both given, they must name the same accounts.
/// `bad_request` for an empty or malformed one.
pub(super) fn explicit_pin(
    account: Option<&str>,
    runtime_account: Option<&str>,
) -> Result<Option<Pin>, RpcError> {
    let given = match account {
        None => None,
        Some(a) => {
            let pin = Pin::parse(a);
            if pin.0.is_empty() {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    "account names no account: give a name, a glob or a comma-separated list",
                ));
            }
            pin.validate()
                .map_err(|e| rpc(codes::BAD_REQUEST, format!("account: {e}")))?;
            Some(pin)
        }
    };
    let from_runtime = runtime_account.map(Pin::parse);
    match (given, from_runtime) {
        (Some(a), Some(b)) if a != b => Err(rpc(
            codes::BAD_REQUEST,
            format!("the runtime names account {b} and account names {a}: give the account once"),
        )),
        (a, b) => Ok(a.or(b)),
    }
}

/// The refusal of a named account on another box than the hub's own.
fn remote_refusal() -> RpcError {
    rpc(
        codes::UNSUPPORTED,
        "accounts on remote boxes aren't supported yet: use the box's own login (twr box \
         login <box> <agent>)",
    )
}

/// Whether thread.new `account` / `runtime` (`<runtime>:<account>`) ask for an account: such
/// a thread on another box than the hub's own is refused before the box is asked
/// ([`remote_refusal`]).
pub(super) fn refuse_remote_explicit(
    box_name: &str,
    account: Option<&str>,
    runtime: Option<&str>,
) -> Result<(), RpcError> {
    let in_runtime = runtime.is_some_and(|r| twr_core::account::split_runtime(r).1.is_some());
    if box_name != LOCAL_BOX && (account.is_some() || in_runtime) {
        return Err(remote_refusal());
    }
    Ok(())
}

/// How an account is written in messages: `claude-code:work`.
fn label(runtime: &str, name: &str) -> String {
    format!("{runtime}:{name}")
}

/// The refusal of a new thread whose account `name` of `runtime` changed while its
/// workspace was made ([`AccountChoice::recheck`]).
fn changed(runtime: &str, name: &str) -> RpcError {
    rpc(
        codes::CONFLICT,
        format!(
            "{} changed while the thread was being made; try again",
            label(runtime, name)
        ),
    )
}

/// What moves thread `view` (on `runtime`) to account `account` (`None`: `<name>`, one to
/// pick): `twr model <thread> --account <account>`, or for the orchestrator, which moves by a
/// switch, `twr orchestrator use <runtime>:<account>`.
pub(super) fn move_command(view: &ThreadView, runtime: &str, account: Option<&str>) -> String {
    let account = account.unwrap_or("<name>");
    match view.is_orchestrator() {
        true => format!("twr orchestrator use {runtime}:{account}"),
        false => format!("twr model {} --account {account}", view.name),
    }
}

/// The refusal (`conflict`) of named account `name` of `runtime` that collides now
/// (`hub::accounts::conflict`: `why`).
fn conflict_refusal(runtime: &str, name: &str, why: &str) -> RpcError {
    rpc(
        codes::CONFLICT,
        format!("{} can't be used: {why}", label(runtime, name)),
    )
}

/// The refusal (`conflict`) of named account `name` of `runtime` whose directory `dir` is
/// missing (deleted by hand): a runtime would make an empty one, logged out, and fail on that.
fn missing_dir_refusal(runtime: &str, name: &str, dir: &Path) -> RpcError {
    rpc(
        codes::CONFLICT,
        format!(
            "{}'s directory {} is missing: twr account add {runtime} {name} makes it again and \
             logs it in",
            label(runtime, name),
            dir.display()
        ),
    )
}

/// Account directory `dir` as it really is when it exists (links followed: a link
/// retargeted later is another directory, [`Core::refuse_account`]), else as it is given.
pub(super) fn resolved(dir: PathBuf) -> PathBuf {
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

/// What to run so that `a` (of `runtime`) can be used again.
fn login_fix(runtime: &str, a: &AccountInfo) -> String {
    format!("twr account login {runtime} {}", a.name)
}

/// Why `a` cannot be used: its login state, or the project's exclusions.
fn unusable_state(a: &AccountInfo, excluded: bool) -> &'static str {
    if excluded {
        return "excluded (exclude_accounts)";
    }
    match a.login {
        Some(Login::LoggedOut) => "logged out",
        Some(Login::Missing) => "missing",
        _ => "unusable",
    }
}

/// `head` (what asked for the accounts), then why none of `unusable` can be used and what
/// fixes each.
fn unusable_message(head: &str, runtime: &str, unusable: &[(&AccountInfo, bool)]) -> String {
    let fix = |a: &AccountInfo, excluded: bool| (!excluded).then(|| login_fix(runtime, a));
    match unusable {
        [(a, ex)] => match fix(a, *ex) {
            Some(f) => format!("{head}, which is {}: {f}", unusable_state(a, *ex)),
            None => format!("{head}, which is {}", unusable_state(a, *ex)),
        },
        _ => {
            let each: Vec<String> = unusable
                .iter()
                .map(|(a, ex)| match fix(a, *ex) {
                    Some(f) => format!("{} is {} ({f})", a.name, unusable_state(a, *ex)),
                    None => format!("{} is {}", a.name, unusable_state(a, *ex)),
                })
                .collect();
            format!(
                "{head}, and none of its accounts can be used: {}",
                each.join(", ")
            )
        }
    }
}

/// The name to suggest adding for a pin that matches nothing: its first plain name, else
/// `<name>`.
fn add_name(pin: &Pin) -> &str {
    pin.0
        .iter()
        .find(|i| !i.contains(['*', '?']))
        .map_or("<name>", String::as_str)
}

/// A new thread's account (see the module docs): `accounts` is the registry, `config` gives
/// the policy's default.
pub(super) fn choose_account(
    accounts: &AccountsView,
    config: &Config,
    ask: &AccountAsk,
) -> Result<AccountChoice, RpcError> {
    let rt = ask.runtime;
    let pins = ask.pins;
    let bad = |m: String| Err(rpc(codes::BAD_REQUEST, m));
    if !takes_accounts(rt) {
        if ask.explicit.is_some() {
            return bad(format!("runtime {rt} takes no accounts"));
        }
        if let Some(p) = &pins.pin {
            return bad(format!(
                "runtime {rt} takes no accounts (project {} pins account {})",
                pins.project(),
                p.pin
            ));
        }
        return Ok(AccountChoice::default());
    }
    let local = ask.box_name == LOCAL_BOX;
    if !local && ask.explicit.is_some() {
        return Err(remote_refusal());
    }
    // On another box the native account is the box's own login: the registry's (the hub's own
    // login) being removed or logged out says nothing of it. Its named accounts are the hub's
    // own box's: a pin there matches the native account alone (one that does not is refused
    // as a remote account, whatever the named accounts' state here).
    let all: Vec<AccountInfo> = match local {
        true => accounts.accounts(rt),
        false => accounts
            .accounts_with(rt, true)
            .into_iter()
            .filter(|a| a.native)
            .map(|a| AccountInfo {
                removed: false,
                login: None,
                ..a
            })
            .collect(),
    };
    let names: Vec<&str> = all.iter().map(|a| a.name.as_str()).collect();
    // A name is the account it stands for (`AccountsView::resolve`): never the native account
    // that a live named one shares its name with (`AccountInfo::answers_to`).
    let info = |n: &str| all.iter().find(|a| a.answers_to(n));
    let usable = |a: &AccountInfo| a.login.is_none_or(Login::counts);
    let (chosen, reason, login_head): (&AccountInfo, &'static str, String) = if let Some(explicit) =
        &ask.explicit
    {
        let head = format!("account \"{explicit}\" on {rt}");
        let matched: Vec<&AccountInfo> = explicit
            .order(&names)
            .iter()
            .filter_map(|n| info(n))
            .collect();
        if matched.is_empty() {
            return bad(format!(
                "{rt} has no account \"{explicit}\": twr account add {rt} {}",
                add_name(explicit)
            ));
        }
        let ok: Vec<&AccountInfo> = matched.iter().copied().filter(|a| usable(a)).collect();
        let Some(first) = ok.first() else {
            let unusable: Vec<(&AccountInfo, bool)> = matched.iter().map(|a| (*a, false)).collect();
            return bad(unusable_message(&head, rt, &unusable));
        };
        match ok.iter().find(|a| pins.allows(&a.name)) {
            Some(a) => (a, REASON_EXPLICIT, head),
            None => {
                let what = pins.keeps_off(&first.name);
                let target = label(rt, &first.name);
                if ask.orchestrator {
                    return bad(match ask.moving {
                        None => {
                            format!("{what}; only the user can open its threads on another account")
                        }
                        Some(_) => {
                            format!("{what}; only the user can move its threads to another account")
                        }
                    });
                }
                if !ask.outside_pin {
                    return bad(match ask.moving {
                        None => format!(
                            "{what}: pass outside_pin (twr thread new --outside-pin) to open the \
                             thread on {target} anyway"
                        ),
                        Some(t) => format!(
                            "{what}: pass outside_pin (twr model {t} --outside-pin) to move it \
                             to {target} anyway"
                        ),
                    });
                }
                (first, REASON_OVERRIDE, head)
            }
        }
    } else if let Some(p) = &pins.pin {
        let head = format!(
            "project {} pins account \"{}\" on {rt}",
            pins.project(),
            p.pin
        );
        let matched: Vec<&AccountInfo> =
            p.pin.order(&names).iter().filter_map(|n| info(n)).collect();
        if matched.is_empty() && !local {
            return Err(remote_refusal());
        }
        if matched.is_empty() {
            return bad(format!(
                "project {} pins account \"{}\", which {rt} doesn't have: add it (twr account \
                 add {rt} {}), or change the pin ({} accounts.{rt} = \"<account>\")",
                pins.project(),
                p.pin,
                add_name(&p.pin),
                p.table
            ));
        }
        match matched
            .iter()
            .find(|a| usable(a) && !pins.excluded(&a.name))
        {
            Some(a) => (a, REASON_PROJECT, head),
            None => {
                let unusable: Vec<(&AccountInfo, bool)> = matched
                    .iter()
                    .map(|a| (*a, pins.excluded(&a.name)))
                    .collect();
                return bad(unusable_message(&head, rt, &unusable));
            }
        }
    } else {
        let default = config
            .account_policy
            .get(rt)
            .and_then(|p| p.default.as_deref())
            .filter(|_| local);
        let from_default = match default {
            None => None,
            Some(d) => match info(d) {
                None => {
                    return bad(format!(
                        "[account_policy.{rt}] default is \"{d}\", which {rt} doesn't have: add \
                         it (twr account add {rt} {d}), or change it"
                    ))
                }
                // Excluded by the project: the native account, as with no default.
                Some(a) if pins.excluded(&a.name) => None,
                Some(a) if !usable(a) => {
                    return bad(unusable_message(
                        &format!("[account_policy.{rt}] default is \"{d}\""),
                        rt,
                        &[(a, false)],
                    ))
                }
                Some(a) => Some(a),
            },
        };
        match from_default {
            Some(a) => (
                a,
                REASON_DEFAULT,
                format!("[account_policy.{rt}] default is \"{}\"", a.name),
            ),
            None => {
                // (Always there: the view has every runtime's native account, and `all` has
                // it unless it was removed on the hub's own box.)
                let Some(native) = all
                    .iter()
                    .find(|a| a.native)
                    .cloned()
                    .or_else(|| accounts.info(rt, None))
                else {
                    return Ok(AccountChoice {
                        reason: REASON_NATIVE,
                        ..Default::default()
                    });
                };
                if native.removed {
                    let pick = match ask.for_orchestrator {
                        true => format!(
                            "twr orchestrator use {rt}:<account>, or [orchestrator] account = \
                             \"<account>\""
                        ),
                        false => "--account".to_string(),
                    };
                    return bad(format!(
                        "{rt}'s own login ({0}) was removed: pick an account ({pick}) or twr \
                         account name-native {rt} {0}",
                        native.name
                    ));
                }
                if pins.excluded(&native.name) {
                    return bad(format!(
                        "project {} excludes {rt}'s own login ({}) and pins no other account \
                         for {rt}: pick one (--account), or pin one ({} accounts.{rt} = \
                         \"<account>\")",
                        pins.project(),
                        native.name,
                        pins.exclude.as_ref().map_or("", |e| e.table.as_str()),
                    ));
                }
                return Ok(AccountChoice {
                    reason: REASON_NATIVE,
                    ..Default::default()
                });
            }
        }
    };
    let pin = ask.explicit.as_ref().map(Pin::to_string);
    // (A remote box's own login is not the registry's to check.)
    let login_head = local.then_some(login_head);
    if chosen.native {
        return Ok(AccountChoice {
            account: None,
            dir: None,
            env_dir: None,
            reason,
            pin,
            login_head,
        });
    }
    if !local {
        return Err(remote_refusal());
    }
    let Some(dir) = chosen.dir.clone() else {
        return Err(rpc(
            codes::INTERNAL,
            format!("{} has no directory", label(rt, &chosen.name)),
        ));
    };
    Ok(AccountChoice {
        account: Some(chosen.name.clone()),
        dir: Some(resolved(dir.clone())),
        env_dir: Some(dir),
        reason,
        pin,
        login_head,
    })
}

impl Core {
    /// A new thread's account, or a moved one's ([`choose_account`] with the registry):
    /// refused (`conflict`) when it is a named account (recorded or config's) whose directory
    /// collides with a runtime's own directory or another account's
    /// (`hub::accounts::conflict`).
    pub(super) fn pick_account(
        &self,
        config: &Config,
        ask: &AccountAsk,
    ) -> Result<AccountChoice, RpcError> {
        let choice = choose_account(&self.accounts, config, ask)?;
        let why = choice
            .account
            .as_deref()
            .and_then(|n| self.account_conflict(ask.runtime, n));
        match (why, &choice.account) {
            (Some(why), Some(name)) => Err(conflict_refusal(ask.runtime, name, &why)),
            _ => Ok(choice),
        }
    }

    /// Checks new thread's account `choice` (on `runtime`, box `box_name`) again under the
    /// lock the thread is recorded with: as [`AccountChoice::recheck`] (removed, or in another
    /// directory meanwhile; logged out or missing now when it had to be usable), and as a
    /// start would ([`Core::refuse_account`]): a named account that collides now
    /// (`hub::accounts::conflict`) or whose directory is missing. Also every other
    /// destination before it is committed: a move's ([`Core::account_move`], so also
    /// `account.remove {move_to}`'s) and a new orchestrator's (`Core::orchestrator_account`,
    /// before the current one is stopped).
    pub(super) fn recheck_account(
        &self,
        choice: &AccountChoice,
        runtime: &str,
        box_name: &str,
    ) -> Result<(), RpcError> {
        choice.recheck(&self.accounts, runtime, box_name)?;
        let Some(name) = choice
            .account
            .as_deref()
            .filter(|_| !choice.reason.is_empty())
        else {
            return Ok(());
        };
        if let Some(why) = self.account_conflict(runtime, name) {
            return Err(conflict_refusal(runtime, name, &why));
        }
        let configured = self
            .accounts
            .info(runtime, Some(name))
            .and_then(|a| a.dir)
            .or_else(|| choice.env_dir.clone());
        match configured.filter(|d| !d.is_dir()) {
            Some(d) => Err(missing_dir_refusal(runtime, name, &d)),
            None => Ok(()),
        }
    }

    /// The account thread `view` moves to with `thread.set_model` on runtime `spec` (the
    /// thread's own, or the one it moves to), for `caller`; `None` when it stays on its account.
    ///
    /// The account asked for is `account`, else `spec`'s account part (both given, they must
    /// agree: [`explicit_pin`]); on a move to another runtime with neither, the thread's own pin
    /// (`ThreadView::account_pin`, when that is the runtime it was made on), else what a new
    /// thread there would get (the project's pin, the policy's default, the native account).
    /// It is chosen like a new thread's ([`choose_account`]: unknown, removed, logged-out or
    /// remote accounts are refused alike), and on the runtime the thread was made on must
    /// also match its own pin: outside the pins, refused for the orchestrator, and for the
    /// user unless `outside_pin`. The account the thread runs on already, in the directory it
    /// began in, is no move: asked for by its name alone, or chosen (first usable) from a list
    /// or a glob, so `work,personal` moves a thread on `personal` to a usable `work`; its
    /// account in another directory is (the session lives in the old one), and so is its
    /// account whose recorded path no longer leads there ([`Core::broken_env_dir`]: a new
    /// session through the configured path), or, asked for explicitly, still leads there
    /// while the account is configured by another path to it ([`Core::retained_env_dir`]: a
    /// rebind to the configured path, a new session).
    pub(super) fn account_move(
        &self,
        hub: &Inner,
        view: &ThreadView,
        spec: &super::runtime_spec::RuntimeSpec,
        account: Option<&str>,
        outside_pin: bool,
        caller: &super::Caller,
    ) -> Result<Option<AccountChoice>, RpcError> {
        let rt = spec.runtime.as_str();
        let other_runtime = rt != view.runtime;
        let asked = explicit_pin(account, spec.account.as_deref())?;
        if asked.is_none() && !other_runtime {
            return Ok(None);
        }
        let recorded = self.rt.get(&view.id).and_then(|r| r.account_dir.clone());
        // The path its runtimes were given no longer leads to its directory: asking for the
        // same account again moves it (a new session through the configured path).
        let broken_path = self.broken_env_dir(view);
        // Its recorded path still leads there, but the account is configured by another path
        // to it now: asking for the same account explicitly rebinds it to the configured path
        // (a new session; `twr account login` signs that path in, not the recorded one).
        let retained_path = asked
            .as_ref()
            .and_then(|_| self.retained_env_dir(view))
            .filter(|_| !other_runtime);
        let in_dir = |dir: Option<&Path>| match (dir, recorded.as_deref()) {
            (Some(a), Some(b)) => same_dir(a, b),
            (None, None) => true,
            _ => false,
        };
        // The account it runs on is all that was asked for: no move, unless its directory
        // changed. A list or a glob is chosen from first (its first usable account, below), and
        // is no move only when that is the account it runs on.
        if let (Some(pin), false) = (&asked, other_runtime) {
            let current = view
                .account
                .clone()
                .unwrap_or_else(|| self.accounts.native_name(rt));
            // (The native account of a thread on another box is that box's own login.)
            let remote_native = view.box_name != LOCAL_BOX && view.account.is_none();
            let now = self
                .accounts
                .info(rt, view.account.as_deref())
                .filter(|a| (!a.removed || remote_native) && a.answers_to(&current));
            if pin.0.iter().all(|item| *item == current)
                && now.is_some_and(|a| in_dir(a.dir.as_deref().filter(|_| !a.native)))
                && broken_path.is_none()
                && retained_path.is_none()
            {
                return Ok(None);
            }
        }
        // The thread's own pin names accounts of the runtime it was made on.
        let own = view
            .account_pin
            .as_deref()
            .map(Pin::parse)
            .filter(|p| !p.0.is_empty() && takes_accounts(rt))
            .filter(|_| self.created_on(&view.id).is_none_or(|c| c == rt));
        // A thread whose project the log does not say (made for a repository path before
        // pins were recorded, or before its project was configured): the one looked up for
        // this move (`Hub::look_up_pins_project`).
        let project = view.pins_project().or_else(|| {
            self.rt
                .get(&view.id)
                .and_then(|r| r.looked_up_project.as_deref())
        });
        let mut pins = project_pins(hub, project, &view.box_name, rt);
        pins.own = own.clone().map(|p| (view.name.clone(), p));
        let choice = self.pick_account(
            &hub.config,
            &AccountAsk {
                runtime: rt,
                explicit: asked.or(own),
                outside_pin,
                orchestrator: caller.is_orchestrator(),
                for_orchestrator: false,
                box_name: &view.box_name,
                pins: &pins,
                moving: Some(&view.name),
            },
        )?;
        let changed = choice.account != view.account
            || (choice.account.is_some()
                && (other_runtime
                    || !in_dir(choice.dir.as_deref())
                    || broken_path.is_some()
                    || retained_path.is_some()));
        if !changed {
            return Ok(None);
        }
        // The destination as a new thread's is checked under the lock it is recorded with
        // (a directory deleted since the last login check, a collision): before the move, or
        // `account.remove {move_to}`, commits anything.
        self.recheck_account(&choice, rt, &view.box_name)?;
        Ok(Some(choice))
    }

    /// Why named account `account` of `runtime`, which a session ran on in directory `dir`
    /// (links followed: `RunSettings::account_dir`), is no longer that account: `removed`, or
    /// `previous directory` when its name leads to another directory now (removed and added
    /// again elsewhere, the native account's name now). `None`: it still is, or the session's
    /// directory is not known (a start from before the stamp), or it ran on the native account.
    pub(super) fn session_account_gone(
        &self,
        runtime: &str,
        account: Option<&str>,
        dir: Option<&Path>,
    ) -> Option<&'static str> {
        let (name, dir) = (account?, dir?);
        match self.accounts.resolve(runtime, name) {
            None => Some("removed"),
            Some(None) => Some("previous directory"),
            Some(Some(_)) => {
                let now = self
                    .accounts
                    .info(runtime, Some(name))
                    .and_then(|a| a.dir)
                    .filter(|d| same_dir(d, dir));
                now.is_none().then_some("previous directory")
            }
        }
    }

    /// The runtime thread `thread` was made on (its `thread.created`); `None` when the log
    /// cannot say.
    fn created_on(&self, thread: &str) -> Option<String> {
        let created = crate::store::first_of_kind(self.store.conn(), thread, "thread.created");
        match created.ok().flatten()?.body {
            EventBody::ThreadCreated(p) => Some(p.runtime),
            _ => None,
        }
    }

    /// The directory of thread `view`'s named account, as recorded when it got it
    /// (`ext.twr.account_dir`, `ThreadRt::account_dir`: links followed), else the registry's
    /// now; `None` for the native account.
    pub(super) fn account_dir(&self, view: &ThreadView) -> Option<PathBuf> {
        let name = view.account.as_deref()?;
        self.rt
            .get(&view.id)
            .and_then(|r| r.account_dir.clone())
            .or_else(|| {
                self.accounts
                    .info(&view.runtime, Some(name))
                    .filter(|a| !a.native)
                    .and_then(|a| a.dir)
            })
    }

    /// The directory a runtime `runtime` of thread `view` on the native account keeps its
    /// sessions in, as the environment it would start with selects it ([`Core::native_store`]
    /// of [`super::spawn_config`]'s, with no account: `threads.env` and the project's `env`
    /// over the hub's own). Stamped on the native account's starts
    /// (`RunSettings::session_store`, from the start's own environment), and what a move back
    /// compares it with (`hub::switch`). `None` on another box (its own environment decides),
    /// or when not known.
    pub(super) fn session_store(
        &self,
        hub: &Inner,
        view: &ThreadView,
        runtime: &str,
    ) -> Option<PathBuf> {
        if view.box_name != LOCAL_BOX {
            return None;
        }
        let cfg = super::spawn_config(hub, view, None);
        self.native_store(runtime, &cfg.env, &cfg.env_remove)
    }

    /// The directory runtime `runtime` on the native account keeps its sessions in when it
    /// starts with `env` set and `remove` removed over the hub's own environment (how the
    /// hub's runtimes start): its selector variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
    /// `GROK_HOME`) as that environment has it, else its default under that environment's
    /// `HOME` (`twr_adapters::account::native_dir`), links followed where it exists. When
    /// `env` and `remove` touch neither the variable nor `HOME`, the hub's own selection
    /// (`HubOptions::native_dir`). The process's environment is not changed.
    pub(super) fn native_store(
        &self,
        runtime: &str,
        env: &[(String, String)],
        remove: &[String],
    ) -> Option<PathBuf> {
        let selector = twr_adapters::account::support(runtime)?.var;
        let touched = |k: &str| env.iter().any(|(n, _)| n == k) || remove.iter().any(|r| r == k);
        let dir = if touched(selector) || touched("HOME") {
            // As the runtime's process gets it: a name in `remove` is removed (also when `env`
            // sets it), else `env`'s last value, else the hub's.
            let var = |k: &str| -> Option<std::ffi::OsString> {
                if remove.iter().any(|r| r == k) {
                    return None;
                }
                match env.iter().rev().find(|(n, _)| n == k) {
                    Some((_, v)) => Some(v.into()),
                    None => (self.account_native_var)(k),
                }
            };
            let home = twr_adapters::account::user_home(var, twr_adapters::account::passwd_home);
            twr_adapters::account::native_dir(runtime, var, home.as_deref())?
        } else {
            (self.account_native_dir)(runtime)?
        };
        Some(std::fs::canonicalize(&dir).unwrap_or(dir))
    }

    /// The login note of the directory a session's login lives in (`dir`: a named account's,
    /// or the native account's session store, [`Core::session_store`]) under the accounts root
    /// (`twr_core::account::login_note`, every workspace's): `Ok(None)` when there is none, or
    /// no root is known. An error says why it cannot be read.
    pub(super) fn login_note(&self, dir: &Path) -> std::io::Result<Option<String>> {
        match self.accounts.default_root() {
            Some(root) => twr_core::account::login_note(root, dir),
            None => Ok(None),
        }
    }

    /// The directory thread `view`'s runtimes get as their named account's
    /// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`): the path recorded with the account
    /// ([`ACCOUNT_ENV_DIR_KEY`]) while it still leads (links followed) to the recorded
    /// directory ([`Core::account_dir`]), so a config that names the same directory by
    /// another path changes nothing a runtime keys on that path (Claude Code's macOS Keychain
    /// entry). One that no longer leads there is never replaced by another path to it (a
    /// start, attach or job is refused: [`Core::broken_env_dir`]); the recorded directory
    /// stands in for what reads the configuration without starting anything. For a thread
    /// recorded before that key, the registry's path while it leads there; else the recorded
    /// directory.
    pub(super) fn account_env_dir(&self, view: &ThreadView) -> Option<PathBuf> {
        let recorded = self.account_dir(view)?;
        if let Some(env) = self
            .rt
            .get(&view.id)
            .and_then(|r| r.account_env_dir.clone())
        {
            return Some(match same_dir(&env, &recorded) {
                true => env,
                false => recorded,
            });
        }
        let configured = self
            .accounts
            .info(&view.runtime, view.account.as_deref())
            .filter(|a| !a.native && !a.removed)
            .and_then(|a| a.dir);
        Some(match configured {
            Some(c) if same_dir(&c, &recorded) => c,
            _ => recorded,
        })
    }

    /// The path thread `view`'s named account was recorded to be given by
    /// ([`ACCOUNT_ENV_DIR_KEY`]) and the recorded directory ([`Core::account_dir`]), when the
    /// path no longer leads there (removed, or a link retargeted). `None` for a thread
    /// recorded before that key, or on the native account.
    pub(super) fn broken_env_dir(&self, view: &ThreadView) -> Option<(PathBuf, PathBuf)> {
        view.account.as_ref()?;
        let env = self.rt.get(&view.id)?.account_env_dir.clone()?;
        let recorded = self.account_dir(view)?;
        (!same_dir(&env, &recorded)).then_some((env, recorded))
    }

    /// The path thread `view`'s named account was recorded to be given by
    /// ([`ACCOUNT_ENV_DIR_KEY`]) and the account's configured path now, when the recorded one
    /// still leads to the recorded directory ([`Core::account_env_dir`] keeps it) and the
    /// configured one names that directory by another path (config changed to an alias of
    /// it). A runtime may key a login on the path (Claude Code's macOS Keychain entry), so
    /// `twr account login`, which signs the configured path in, does not sign the thread's in:
    /// an explicit move to the same account rebinds it to the configured path
    /// ([`Core::account_move`]). `None` otherwise (the native account, a removed one, a path
    /// that no longer leads there: [`Core::broken_env_dir`]).
    pub(super) fn retained_env_dir(&self, view: &ThreadView) -> Option<(PathBuf, PathBuf)> {
        let name = view.account.as_deref()?;
        let env = self.rt.get(&view.id)?.account_env_dir.clone()?;
        let recorded = self.account_dir(view)?;
        let configured = self
            .accounts
            .info(&view.runtime, Some(name))
            .filter(|a| !a.native && !a.removed)?
            .dir?;
        (configured != env && same_dir(&env, &recorded) && same_dir(&configured, &recorded))
            .then_some((env, configured))
    }

    /// Thread `view`'s runtime configuration ([`super::spawn_config`]) with its account's
    /// environment ([`Core::account_env_dir`]).
    pub(super) fn spawn_config(
        &self,
        hub: &Inner,
        view: &ThreadView,
    ) -> twr_core::adapter::SpawnConfig {
        super::spawn_config(hub, view, self.account_env_dir(view).as_deref())
    }

    /// Refuses (`conflict`) to start anything of thread `view` (its runtime, an attach, a job)
    /// on an account that cannot run it: a named account that was removed (or is no longer
    /// declared), that collides with a runtime's own directory or another account's
    /// (`hub::accounts::conflict`), whose directory is not the one recorded when the thread got
    /// it (both with links followed: a link retargeted is another directory), or whose
    /// directory is missing (not a directory), or whose recorded path no longer leads to the
    /// recorded directory ([`Core::broken_env_dir`]); the native account when it was removed,
    /// on the hub's own box only (on another box it is that box's own login).
    /// Called at the moment of use (each start, attach and job launch), not before a wait.
    pub(super) fn refuse_account(&self, view: &ThreadView) -> Result<(), RpcError> {
        let rt = view.runtime.as_str();
        if !takes_accounts(rt) {
            return Ok(());
        }
        let name = view
            .account
            .clone()
            .unwrap_or_else(|| self.accounts.native_name(rt));
        let thread = &view.name;
        let (move_it, starts) = match view.is_orchestrator() {
            true => ("switch it", "starts a new orchestrator there"),
            false => ("move it", "starts a new session there"),
        };
        let removed = || {
            Err(rpc(
                codes::CONFLICT,
                format!(
                    "{thread} runs on {}, which was removed: {move_it} ({})",
                    label(rt, &name),
                    move_command(view, rt, None)
                ),
            ))
        };
        let Some(account) = &view.account else {
            let local = view.box_name == LOCAL_BOX;
            if local && self.accounts.info(rt, None).is_some_and(|a| a.removed) {
                return removed();
            }
            return Ok(());
        };
        let Some(now) = self.accounts.resolve(rt, account) else {
            return removed();
        };
        if let Some(why) = self.account_conflict(rt, account) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{thread} runs on {}, which can't be used: {why}",
                    label(rt, account)
                ),
            ));
        }
        let effective = match now {
            Some(_) => self.accounts.info(rt, Some(account)).and_then(|a| a.dir),
            None => None,
        };
        let recorded = self.rt.get(&view.id).and_then(|r| r.account_dir.clone());
        if let Some(recorded) = recorded {
            if !effective.as_deref().is_some_and(|d| same_dir(d, &recorded)) {
                // As configured, and where it leads when that is elsewhere (a link).
                let shown = |d: Option<&Path>| match d {
                    Some(d) => match std::fs::canonicalize(d) {
                        Ok(real) if real != d => format!("{}, now {}", d.display(), real.display()),
                        _ => d.display().to_string(),
                    },
                    None => format!("{rt}'s own directory"),
                };
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{}'s directory changed since {thread}'s session began ({} -> {}): {} \
                         {starts}",
                        label(rt, account),
                        recorded.display(),
                        shown(effective.as_deref()),
                        move_command(view, rt, Some(account))
                    ),
                ));
            }
        }
        // Its directory gone (deleted by hand).
        if let Some(d) = effective.filter(|d| !d.is_dir()) {
            return Err(missing_dir_refusal(rt, account, &d));
        }
        // The path its session was given no longer leads there (a link removed or
        // retargeted): another path to the directory may be another login to the runtime
        // (Claude Code's macOS Keychain entry is keyed on the path), so no session goes on
        // through one; a move to the same account starts a new one through the configured path.
        if let Some((env, dir)) = self.broken_env_dir(view) {
            let starts = match view.is_orchestrator() {
                true => "starts a new orchestrator",
                false => "starts a new session",
            };
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{}'s path {} no longer leads to {}: {} {starts} through the configured path",
                    label(rt, account),
                    env.display(),
                    dir.display(),
                    move_command(view, rt, Some(account))
                ),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::event::{
        AccountAdded, AccountRemoved, AccountStatus, Event, EventBody, Source, HUB_THREAD,
    };

    const CONFIG: &str = r#"
[accounts.claude-code.work]
dir = "/acc/work"
[accounts.claude-code.work-b]
dir = "/acc/work-b"
[accounts.claude-code.personal]
native = true
[accounts.codex.team]
dir = "/acc/team"

[account_policy.codex]
default = "team"

[projects.acme-api]
repo = "/r/api"
account = "work"

[projects.mixed]
repo = "/r/mixed"
account = "work"
accounts = { codex = "team" }

[projects.pool]
repo = "/r/pool"
account = ["work-b", "work*"]

[projects.globbed]
repo = "/r/globbed"
account = "work*"

[projects.ex]
repo = "/r/ex"
exclude_accounts = ["personal"]

[projects.codex-ex]
repo = "/r/codex-ex"
exclude_accounts = "team"

[projects.every]
repo = "/r/every"
account = "*"
exclude_accounts = "work*"

[boxes.gpu]
ssh = "me@gpu"
transport = "ssh"

[boxes.gpu.projects.acme-api]
repo = "/srv/api"
accounts = { claude-code = "personal" }
"#;

    fn config() -> Config {
        Config::from_toml_str(CONFIG).unwrap()
    }

    fn view(config: &Config, events: &[EventBody]) -> AccountsView {
        let mut v = AccountsView::new(config).with_default_root("/root");
        for (i, body) in events.iter().enumerate() {
            v.apply(&Event::new(
                HUB_THREAD,
                i as u64 + 1,
                None,
                Source::Hub,
                body.clone(),
            ));
        }
        v
    }

    fn logged_out(rt: &str, name: &str) -> EventBody {
        EventBody::AccountStatus(AccountStatus {
            runtime: rt.into(),
            account: name.into(),
            login: Login::LoggedOut,
            ..Default::default()
        })
    }

    /// What `runtime` on `box_name` gets for a thread of `project` asking for `explicit`.
    #[derive(Clone, Copy)]
    struct Case<'a> {
        runtime: &'a str,
        project: Option<&'a str>,
        box_name: &'a str,
        explicit: Option<&'a str>,
        outside_pin: bool,
        orchestrator: bool,
    }

    impl<'a> Case<'a> {
        fn new(runtime: &'a str, project: Option<&'a str>) -> Self {
            Case {
                runtime,
                project,
                box_name: LOCAL_BOX,
                explicit: None,
                outside_pin: false,
                orchestrator: false,
            }
        }

        /// The choice, without its [`AccountChoice::login_head`] (`rechecks_logins` checks
        /// those).
        fn run(&self, c: &Config, v: &AccountsView) -> Result<AccountChoice, String> {
            self.choose(c, v).map(|choice| AccountChoice {
                login_head: None,
                ..choice
            })
        }

        fn choose(&self, c: &Config, v: &AccountsView) -> Result<AccountChoice, String> {
            let bp = self
                .project
                .and_then(|p| c.boxes.get(self.box_name).and_then(|b| b.projects.get(p)));
            let pins = match self.project {
                Some(p) => pins_of(c, bp, p, self.box_name, self.runtime),
                None => ProjectPins::default(),
            };
            choose_account(
                v,
                c,
                &AccountAsk {
                    runtime: self.runtime,
                    explicit: explicit_pin(self.explicit, None).unwrap(),
                    outside_pin: self.outside_pin,
                    orchestrator: self.orchestrator,
                    for_orchestrator: false,
                    box_name: self.box_name,
                    pins: &pins,
                    moving: None,
                },
            )
            .map_err(|e| e.message)
        }
    }

    fn named(name: &str, dir: &str, reason: &'static str, pin: Option<&str>) -> AccountChoice {
        AccountChoice {
            account: Some(name.into()),
            dir: Some(dir.into()),
            env_dir: Some(dir.into()),
            reason,
            pin: pin.map(str::to_string),
            login_head: None,
        }
    }

    fn native(reason: &'static str, pin: Option<&str>) -> AccountChoice {
        AccountChoice {
            reason,
            pin: pin.map(str::to_string),
            ..Default::default()
        }
    }

    /// A recorded native `personal` and a recorded named `default`, then config declares
    /// `personal` named: the native account falls back to `default` next to the named one.
    /// `default` is the named account (`AccountsView::resolve`) for an explicit choice, a
    /// project pin and the policy's default alike, never the native account's login.
    #[test]
    fn a_name_the_native_account_shares_is_the_named_accounts() {
        let toml = r#"
[accounts.codex.personal]
dir = "/acc/personal"

[account_policy.codex]
default = "default"

[projects.pinned]
repo = "/r/pinned"
accounts = { codex = "default" }

[projects.globbed]
repo = "/r/globbed"
accounts = { codex = "def*" }
"#;
        let c = Config::from_toml_str(toml).unwrap();
        let mut events = vec![
            EventBody::AccountAdded(AccountAdded {
                runtime: "codex".into(),
                account: "personal".into(),
                native: true,
                ..Default::default()
            }),
            EventBody::AccountAdded(AccountAdded {
                runtime: "codex".into(),
                account: "default".into(),
                dir: Some("/acc/default".into()),
                ..Default::default()
            }),
        ];
        let v = view(&c, &events);
        assert_eq!(v.native_name("codex"), "default");
        let want = |reason, pin: Option<&str>| named("default", "/acc/default", reason, pin);
        let explicit = Case {
            explicit: Some("default"),
            ..Case::new("codex", None)
        };
        assert_eq!(
            explicit.run(&c, &v),
            Ok(want(REASON_EXPLICIT, Some("default")))
        );
        assert_eq!(
            Case::new("codex", Some("pinned")).run(&c, &v),
            Ok(want(REASON_PROJECT, None))
        );
        assert_eq!(
            Case::new("codex", Some("globbed")).run(&c, &v),
            Ok(want(REASON_PROJECT, None))
        );
        assert_eq!(
            Case::new("codex", None).run(&c, &v),
            Ok(want(REASON_DEFAULT, None))
        );
        // The named one logged out: refused, never the native account in its place.
        events.push(logged_out("codex", "default"));
        let v = view(&c, &events);
        for case in [
            explicit,
            Case::new("codex", Some("pinned")),
            Case::new("codex", None),
        ] {
            let e = case.run(&c, &v).unwrap_err();
            assert!(e.contains("logged out"), "{e}");
        }
        // Once the named one is removed, the name is the native account's again.
        events.push(EventBody::AccountRemoved(AccountRemoved {
            runtime: "codex".into(),
            account: "default".into(),
            ..Default::default()
        }));
        let v = view(&c, &events);
        assert_eq!(
            explicit.run(&c, &v),
            Ok(native(REASON_EXPLICIT, Some("default")))
        );
    }

    /// Explicit > project pin (`accounts.<rt>` over `account`; names, globs, lists in order)
    /// > the policy's default (local only) > native.
    #[test]
    fn the_order_of_choices() {
        let c = config();
        let v = view(&c, &[]);
        let run = |case: Case| case.run(&c, &v);
        assert_eq!(
            run(Case::new("claude-code", None)),
            Ok(native(REASON_NATIVE, None))
        );
        assert_eq!(
            run(Case {
                explicit: Some("work"),
                ..Case::new("claude-code", None)
            }),
            Ok(named("work", "/acc/work", REASON_EXPLICIT, Some("work")))
        );
        // The native account by name, explicitly.
        assert_eq!(
            run(Case {
                explicit: Some("personal"),
                ..Case::new("claude-code", None)
            }),
            Ok(native(REASON_EXPLICIT, Some("personal")))
        );
        // A list: its first match; a glob: its matches by name.
        assert_eq!(
            run(Case {
                explicit: Some(" work-b , work"),
                ..Case::new("claude-code", None)
            }),
            Ok(named(
                "work-b",
                "/acc/work-b",
                REASON_EXPLICIT,
                Some("work-b,work")
            ))
        );
        assert_eq!(
            run(Case::new("claude-code", Some("acme-api"))),
            Ok(named("work", "/acc/work", REASON_PROJECT, None))
        );
        assert_eq!(
            run(Case::new("claude-code", Some("pool"))),
            Ok(named("work-b", "/acc/work-b", REASON_PROJECT, None))
        );
        assert_eq!(
            run(Case::new("claude-code", Some("globbed"))),
            Ok(named("work", "/acc/work", REASON_PROJECT, None))
        );
        assert_eq!(
            run(Case::new("codex", Some("mixed"))),
            Ok(named("team", "/acc/team", REASON_PROJECT, None))
        );
        // `*` minus the exclusions: the native account is what is left.
        assert_eq!(
            run(Case::new("claude-code", Some("every"))),
            Ok(native(REASON_PROJECT, None))
        );
        // The policy's default, on the hub's own box only; an excluded one falls through.
        assert_eq!(
            run(Case::new("codex", None)),
            Ok(named("team", "/acc/team", REASON_DEFAULT, None))
        );
        assert_eq!(
            run(Case {
                box_name: "gpu",
                ..Case::new("codex", None)
            }),
            Ok(native(REASON_NATIVE, None))
        );
        assert_eq!(
            run(Case::new("codex", Some("codex-ex"))),
            Ok(native(REASON_NATIVE, None))
        );
        // A runtime that takes no accounts: nothing to record.
        assert_eq!(run(Case::new("fake", None)), Ok(AccountChoice::default()));
        assert_eq!(
            run(Case::new("fake", Some("ex"))),
            Ok(AccountChoice::default())
        );
    }

    /// A logged-out account is skipped in a pool and refused alone, with the login to run.
    #[test]
    fn unusable_accounts() {
        let c = config();
        let v = view(&c, &[logged_out("claude-code", "work-b")]);
        assert_eq!(
            Case::new("claude-code", Some("pool")).run(&c, &v),
            Ok(named("work", "/acc/work", REASON_PROJECT, None))
        );
        let v = view(
            &c,
            &[
                logged_out("claude-code", "work"),
                EventBody::AccountStatus(AccountStatus {
                    runtime: "claude-code".into(),
                    account: "work-b".into(),
                    login: Login::Missing,
                    ..Default::default()
                }),
            ],
        );
        assert_eq!(
            Case::new("claude-code", Some("acme-api")).run(&c, &v),
            Err(
                "project acme-api pins account \"work\" on claude-code, which is logged out: \
                 twr account login claude-code work"
                    .into()
            )
        );
        assert_eq!(
            Case::new("claude-code", Some("globbed")).run(&c, &v),
            Err(
                "project globbed pins account \"work*\" on claude-code, and none of its \
                 accounts can be used: work is logged out (twr account login claude-code work), \
                 work-b is missing (twr account login claude-code work-b)"
                    .into()
            )
        );
        assert_eq!(
            Case {
                explicit: Some("work"),
                ..Case::new("claude-code", None)
            }
            .run(&c, &v),
            Err(
                "account \"work\" on claude-code, which is logged out: twr account login \
                 claude-code work"
                    .into()
            )
        );
        let v = view(&c, &[logged_out("codex", "team")]);
        assert_eq!(
            Case::new("codex", None).run(&c, &v),
            Err(
                "[account_policy.codex] default is \"team\", which is logged out: twr account \
                 login codex team"
                    .into()
            )
        );
    }

    /// Every refusal, word for word.
    #[test]
    fn refusals() {
        let c = config();
        let v = view(&c, &[]);
        let run = |case: Case| case.run(&c, &v).unwrap_err();
        assert_eq!(
            run(Case::new("codex", Some("acme-api"))),
            "project acme-api pins account \"work\", which codex doesn't have: add it (twr \
             account add codex work), or change the pin ([projects.acme-api] accounts.codex = \
             \"<account>\")"
        );
        assert_eq!(
            run(Case {
                explicit: Some("work"),
                ..Case::new("codex", None)
            }),
            "codex has no account \"work\": twr account add codex work"
        );
        assert_eq!(
            run(Case {
                explicit: Some("work"),
                ..Case::new("fake", None)
            }),
            "runtime fake takes no accounts"
        );
        assert_eq!(
            run(Case::new("fake", Some("acme-api"))),
            "runtime fake takes no accounts (project acme-api pins account work)"
        );
        // Outside the pin: the user with outside_pin only, never the orchestrator.
        let outside = Case {
            explicit: Some("work-b"),
            ..Case::new("claude-code", Some("acme-api"))
        };
        assert_eq!(
            run(Case { ..outside }),
            "project acme-api pins account work: pass outside_pin (twr thread new \
             --outside-pin) to open the thread on claude-code:work-b anyway"
        );
        assert_eq!(
            Case {
                outside_pin: true,
                ..outside
            }
            .run(&c, &v),
            Ok(named(
                "work-b",
                "/acc/work-b",
                REASON_OVERRIDE,
                Some("work-b")
            ))
        );
        let orch = Case {
            outside_pin: true,
            orchestrator: true,
            ..outside
        };
        assert_eq!(
            run(orch),
            "project acme-api pins account work; only the user can open its threads on \
             another account"
        );
        // Within the pin, the orchestrator may name it.
        assert_eq!(
            Case {
                explicit: Some("work"),
                orchestrator: true,
                ..Case::new("claude-code", Some("acme-api"))
            }
            .run(&c, &v),
            Ok(named("work", "/acc/work", REASON_EXPLICIT, Some("work")))
        );
        // A pool asked for: the match the pin allows.
        assert_eq!(
            Case {
                explicit: Some("work-b,work"),
                orchestrator: true,
                ..Case::new("claude-code", Some("acme-api"))
            }
            .run(&c, &v),
            Ok(named(
                "work",
                "/acc/work",
                REASON_EXPLICIT,
                Some("work-b,work")
            ))
        );
        assert_eq!(
            run(Case {
                explicit: Some("personal"),
                ..Case::new("claude-code", Some("ex"))
            }),
            "project ex excludes account personal: pass outside_pin (twr thread new \
             --outside-pin) to open the thread on claude-code:personal anyway"
        );
        // Nothing but the excluded native account.
        assert_eq!(
            run(Case::new("claude-code", Some("ex"))),
            "project ex excludes claude-code's own login (personal) and pins no other account \
             for claude-code: pick one (--account), or pin one ([projects.ex] \
             accounts.claude-code = \"<account>\")"
        );
        // Remote boxes: an explicit account, or a pin that picks a named one.
        let remote = "accounts on remote boxes aren't supported yet: use the box's own login \
                      (twr box login <box> <agent>)";
        assert_eq!(
            run(Case {
                explicit: Some("work"),
                box_name: "gpu",
                ..Case::new("claude-code", None)
            }),
            remote
        );
        assert_eq!(
            run(Case {
                box_name: "gpu",
                ..Case::new("codex", Some("mixed"))
            }),
            remote
        );
        // The box project's pin replaces the project's: the native account there.
        assert_eq!(
            Case {
                box_name: "gpu",
                ..Case::new("claude-code", Some("acme-api"))
            }
            .run(&c, &v),
            Ok(native(REASON_PROJECT, None))
        );
        // The policy's default naming an account the runtime lacks.
        let mut c2 = c.clone();
        c2.account_policy.get_mut("codex").unwrap().default = Some("gone".into());
        assert_eq!(
            Case::new("codex", None)
                .run(&c2, &view(&c2, &[]))
                .unwrap_err(),
            "[account_policy.codex] default is \"gone\", which codex doesn't have: add it (twr \
             account add codex gone), or change it"
        );
        // The native account removed.
        let v = view(
            &c,
            &[EventBody::AccountRemoved(AccountRemoved {
                runtime: "grok-build".into(),
                account: "default".into(),
                native: true,
                ..Default::default()
            })],
        );
        assert_eq!(
            Case::new("grok-build", None).run(&c, &v).unwrap_err(),
            "grok-build's own login (default) was removed: pick an account (--account) or twr \
             account name-native grok-build default"
        );
        // On another box the native account is the box's own login: the registry's removal
        // or login state says nothing of it.
        let gpu = |rt| Case {
            box_name: "gpu",
            ..Case::new(rt, None)
        };
        assert_eq!(
            gpu("grok-build").run(&c, &v),
            Ok(native(REASON_NATIVE, None))
        );
        let v = view(&c, &[logged_out("claude-code", "personal")]);
        assert_eq!(
            Case {
                box_name: "gpu",
                ..Case::new("claude-code", Some("acme-api"))
            }
            .run(&c, &v),
            Ok(native(REASON_PROJECT, None))
        );
    }

    /// On another box a pin matches the native account alone: one that matches only named
    /// accounts is refused as a remote account, whatever their state on the hub's box (never
    /// a local login's hint).
    #[test]
    fn a_remote_pin_matches_the_native_account_alone() {
        let c = config();
        let remote = "accounts on remote boxes aren't supported yet: use the box's own login \
                      (twr box login <box> <agent>)";
        let gpu = |project| Case {
            box_name: "gpu",
            ..Case::new("claude-code", Some(project))
        };
        let removed = EventBody::AccountRemoved(AccountRemoved {
            runtime: "claude-code".into(),
            account: "work-b".into(),
            ..Default::default()
        });
        for v in [
            view(&c, &[]),
            view(&c, &[logged_out("claude-code", "work")]),
            view(
                &c,
                &[
                    logged_out("claude-code", "work"),
                    logged_out("claude-code", "work-b"),
                ],
            ),
            view(&c, &[removed]),
        ] {
            for project in ["pool", "globbed"] {
                assert_eq!(gpu(project).run(&c, &v).unwrap_err(), remote, "{project}");
            }
        }
        // `*` matches the native account there.
        assert_eq!(
            gpu("every").run(&c, &view(&c, &[])),
            Ok(native(REASON_PROJECT, None))
        );
    }

    /// The native account nothing else chose records no reason (absent is native); any other
    /// choice records its own.
    #[test]
    fn the_native_reason_is_not_recorded() {
        let mut twr = Map::new();
        native(REASON_NATIVE, None).record(&mut twr);
        assert!(twr.is_empty(), "{twr:?}");
        native(REASON_PROJECT, None).record(&mut twr);
        assert_eq!(twr.get(ACCOUNT_REASON_KEY), Some(&json!(REASON_PROJECT)));
    }

    /// The box project's keys over the project's.
    #[test]
    fn box_project_pins_replace_the_projects() {
        let c = config();
        let bp = &c.boxes["gpu"].projects["acme-api"];
        let p = pins_of(&c, Some(bp), "acme-api", "gpu", "claude-code");
        assert_eq!(
            p.pin,
            Some(PinFrom {
                pin: Pin::parse("personal"),
                table: "[boxes.gpu.projects.acme-api]".into()
            })
        );
        let p = pins_of(&c, Some(bp), "acme-api", "gpu", "codex");
        assert_eq!(
            p.pin,
            Some(PinFrom {
                pin: Pin::parse("work"),
                table: "[projects.acme-api]".into()
            })
        );
    }

    /// Under the commit lock: an account removed meanwhile (the native one too: renamed is
    /// still it), or a named one in another directory (a link retargeted included), refuses
    /// the thread; a runtime that takes no accounts has nothing to check.
    #[test]
    fn rechecks() {
        use twr_core::event::AccountAdded;
        let c = config();
        let v = view(&c, &[]);
        let native = Case::new("grok-build", None).run(&c, &v).unwrap();
        assert_eq!(native, self::native(REASON_NATIVE, None));
        assert_eq!(native.recheck(&v, "grok-build", LOCAL_BOX), Ok(()));
        let renamed = view(
            &c,
            &[EventBody::AccountAdded(AccountAdded {
                runtime: "grok-build".into(),
                account: "mine".into(),
                native: true,
                ..Default::default()
            })],
        );
        assert_eq!(native.recheck(&renamed, "grok-build", LOCAL_BOX), Ok(()));
        let removed = view(
            &c,
            &[EventBody::AccountRemoved(AccountRemoved {
                runtime: "grok-build".into(),
                account: "default".into(),
                native: true,
                ..Default::default()
            })],
        );
        assert_eq!(
            native
                .recheck(&removed, "grok-build", LOCAL_BOX)
                .unwrap_err()
                .message,
            "grok-build:default changed while the thread was being made; try again"
        );
        // On another box: the box's own login.
        assert_eq!(native.recheck(&removed, "grok-build", "gpu"), Ok(()));
        // An explicit native account by name, removed: the same.
        let explicit = AccountChoice {
            pin: Some("default".into()),
            ..self::native(REASON_EXPLICIT, None)
        };
        assert!(explicit.recheck(&removed, "grok-build", LOCAL_BOX).is_err());
        assert_eq!(
            AccountChoice::default().recheck(&removed, "fake", LOCAL_BOX),
            Ok(())
        );

        // A named account's directory: as it really is.
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, link) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("link"),
        );
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let mut c = config();
        c.accounts
            .get_mut("claude-code")
            .unwrap()
            .get_mut("work")
            .unwrap()
            .dir = Some(link.clone());
        let v = view(&c, &[]);
        let work = Case {
            explicit: Some("work"),
            ..Case::new("claude-code", None)
        }
        .run(&c, &v)
        .unwrap();
        assert_eq!(work.dir, Some(a.canonicalize().unwrap()));
        assert_eq!(work.recheck(&v, "claude-code", LOCAL_BOX), Ok(()));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert_eq!(
            work.recheck(&v, "claude-code", LOCAL_BOX)
                .unwrap_err()
                .message,
            "claude-code:work changed while the thread was being made; try again"
        );
        let mut moved = c.clone();
        moved
            .accounts
            .get_mut("claude-code")
            .unwrap()
            .get_mut("work")
            .unwrap()
            .dir = Some(a.clone());
        assert_eq!(
            work.recheck(&view(&moved, &[]), "claude-code", LOCAL_BOX),
            Ok(())
        );
        moved
            .accounts
            .get_mut("claude-code")
            .unwrap()
            .get_mut("work")
            .unwrap()
            .dir = Some("/acc/elsewhere".into());
        assert!(work
            .recheck(&view(&moved, &[]), "claude-code", LOCAL_BOX)
            .is_err());
    }

    /// A choice that had to be usable (an explicit named or native account, a project pin's,
    /// the policy's default) is refused again, as it would have been when chosen, once its
    /// account is logged out or missing by the time the thread is recorded. The native
    /// account nothing else chose, and a remote box's own login, are not.
    #[test]
    fn rechecks_logins() {
        let c = config();
        let v = view(&c, &[]);
        let explicit = |name| Case {
            explicit: Some(name),
            ..Case::new("claude-code", None)
        };
        let cases = [
            (explicit("work"), "claude-code", "work"),
            (
                Case::new("claude-code", Some("acme-api")),
                "claude-code",
                "work",
            ),
            (Case::new("codex", None), "codex", "team"),
            (explicit("personal"), "claude-code", "personal"),
            (
                Case::new("claude-code", Some("acme-api")),
                "claude-code",
                "work-b",
            ),
        ];
        for (case, rt, name) in cases {
            let choice = case.choose(&c, &v).unwrap();
            assert_eq!(choice.recheck(&v, rt, LOCAL_BOX), Ok(()), "{name}");
            let out = view(&c, &[logged_out(rt, name)]);
            let now = choice.recheck(&out, rt, LOCAL_BOX);
            if name == "work-b" {
                // Another account than the one chosen: still fine.
                assert_eq!(now, Ok(()));
                continue;
            }
            let e = now.unwrap_err();
            assert_eq!(e.code, codes::BAD_REQUEST);
            // The message a choice made now gets.
            assert_eq!(Err(e.message), case.run(&c, &out), "{name}");
        }
        let missing = view(
            &c,
            &[EventBody::AccountStatus(AccountStatus {
                runtime: "claude-code".into(),
                account: "work".into(),
                login: Login::Missing,
                ..Default::default()
            })],
        );
        let work = explicit("work").choose(&c, &v).unwrap();
        assert_eq!(
            work.recheck(&missing, "claude-code", LOCAL_BOX)
                .unwrap_err()
                .message,
            "account \"work\" on claude-code, which is missing: twr account login claude-code \
             work"
        );
        // Unchanged: the native account nothing else chose, and a remote box's own login.
        let out = view(&c, &[logged_out("claude-code", "personal")]);
        let native = Case::new("claude-code", None).choose(&c, &v).unwrap();
        assert_eq!(native.login_head, None);
        assert_eq!(native.recheck(&out, "claude-code", LOCAL_BOX), Ok(()));
        let remote = Case {
            box_name: "gpu",
            ..Case::new("claude-code", Some("acme-api"))
        }
        .choose(&c, &v)
        .unwrap();
        assert_eq!(remote.login_head, None);
        assert_eq!(remote.recheck(&out, "claude-code", "gpu"), Ok(()));
    }

    #[test]
    fn explicit_pins() {
        assert_eq!(explicit_pin(None, None), Ok(None));
        assert_eq!(
            explicit_pin(Some("a, b"), Some("a,b")),
            Ok(Some(Pin::parse("a,b")))
        );
        assert_eq!(explicit_pin(None, Some("w")), Ok(Some(Pin::parse("w"))));
        assert_eq!(
            explicit_pin(Some("a"), Some("b")).unwrap_err().message,
            "the runtime names account b and account names a: give the account once"
        );
        assert!(explicit_pin(Some(" , "), None)
            .unwrap_err()
            .message
            .contains("names no account"));
        assert!(explicit_pin(Some(".."), None)
            .unwrap_err()
            .message
            .contains("not an account name"));
    }
}

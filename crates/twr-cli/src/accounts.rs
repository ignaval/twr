//! `twr account`: a runtime's logins ("accounts", [`twr_core::account`]) from the command
//! line. `list` and `status` show the hub's registry (`account.list`, `account.status`);
//! `add` does the parts that need this terminal (the account's directory, the links to the
//! native directory, the runtime's own login in the foreground) and the hub the bookkeeping
//! (`account.add`); `login` signs an account in again; `remove` takes one out
//! (`account.remove`), optionally logging it out and deleting its directory; `name-native`
//! names the runtime's own login.
//!
//! The login runs the runtime's program as a spawn of a thread on this box would
//! (`twr_adapters::agents::agent_command`: `TWR_*_BIN`, else the command on `PATH`) with the
//! account's environment (`twr_adapters::account::account_env`: its variable set, the stray
//! credentials removed). twr does not capture what it prints, nor what the user types.
//!
//! What acts on a runtime's own directory (the native account's login, the links `add`
//! makes, the checks that a named account's directory is not a runtime's own) follows the
//! hub's selection of it, not this environment's: `account.list`'s `native_dir` and
//! `native_var` ([`Natives`]), the native account's login running with the hub's selector
//! variable ([`native_env`]). A hub that does not say (an older one): this environment's.

use std::path::{Path, PathBuf};
use std::process::ExitStatus;

use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use clap::Subcommand;
use serde_json::{json, Value};
use twr_core::paths::Paths;
use twr_core::runner::{AgentInfo, AgentLogin};

use super::{
    ask_on_stderr, connect, consent, localize, refuse_agent_action, str_of, table, workspace_hint,
    Consent,
};

#[derive(Debug, Subcommand)]
pub(super) enum AccountCmd {
    /// List every runtime's accounts: where each is declared (config.toml, twr's records), its
    /// last login check, the threads on it and its directory. A runtime whose only account is
    /// its own login, never checked, is left out.
    #[command(alias = "ls")]
    List {
        /// Print the raw `account.list` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Check the accounts' logins again now (all, a runtime's, or one: `claude-code:work`)
    /// and list them, with each named account's links to the native directory and the
    /// variables of this environment a named account does not get.
    Status {
        /// `<runtime>` or `<runtime>:<name>` (`claude` and `grok` work for `claude-code` and
        /// `grok-build`).
        target: Option<String>,
        /// Print the raw `account.status` JSON.
        #[arg(long)]
        json: bool,
    },
    /// Add an account: make its directory (mode 700), link the native account's settings
    /// into it (--share config, the default), run the runtime's login in this terminal with
    /// the account's variable set (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`), then
    /// record it in the hub. An account config.toml declares is set up and logged in the
    /// same way, and nothing is recorded.
    Add {
        /// `claude-code` (or `claude`), `codex`, `grok-build` (or `grok`).
        runtime: String,
        /// The account's name: letters, digits, `-`, `_`, `.`.
        name: String,
        /// Its directory (default: `<data>/accounts/<runtime>/<name>`): an empty one, or a
        /// config directory that is already logged in.
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// What it shares with the native account: `config` (settings, instructions, skills;
        /// never the login or the sessions) or `none`.
        #[arg(long, value_parser = ["none", "config"])]
        share: Option<String>,
        /// Name the runtime's current login (the native account) this, instead of being
        /// asked (default: `default`).
        #[arg(long, value_name = "NAME")]
        native_name: Option<String>,
        /// Log in with a device code (Codex, Grok Build: no browser opens, over ssh).
        #[arg(long)]
        device: bool,
        /// Set the account up without logging in (`twr account login` does it later).
        #[arg(long)]
        no_login: bool,
        /// Arguments for the runtime's login command (after `--`), such as `--console`.
        #[arg(last = true, value_name = "LOGIN ARGS")]
        login_args: Vec<String>,
    },
    /// Sign an account in again (after an expiry or a logout), in this terminal. The native
    /// account signs in where the hub's threads on it run: with the hub's
    /// `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GROK_HOME`, or without it when the hub has none.
    Login {
        /// The runtime.
        runtime: String,
        /// The account's name.
        name: String,
        /// Log in with a device code (Codex, Grok Build).
        #[arg(long)]
        device: bool,
        /// Arguments for the runtime's login command (after `--`).
        #[arg(last = true, value_name = "LOGIN ARGS")]
        login_args: Vec<String>,
    },
    /// Remove an account. Threads on it need --move-to (each moves, with a handoff) or
    /// --force (they keep it and cannot start until moved). Its directory stays unless
    /// --purge; `twr account add --dir` can adopt it again.
    Remove {
        /// The runtime.
        runtime: String,
        /// The account's name.
        name: String,
        /// Move its threads to this account of the runtime first.
        #[arg(long, value_name = "NAME")]
        move_to: Option<String>,
        /// Remove it although threads run on it.
        #[arg(long)]
        force: bool,
        /// Run the runtime's logout in the account's directory too, once no thread's runtime
        /// runs on it (a moved thread finishes its turn first; waited for up to 10 minutes).
        #[arg(long)]
        logout: bool,
        /// Delete the account's directory too (only one under twr's accounts directory),
        /// after asking y/N (without a terminal, only with --yes), once no thread's runtime
        /// runs on it, as --logout waits.
        #[arg(long)]
        purge: bool,
        /// With --purge: delete without asking.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Name the runtime's own login (the native account) without adding another.
    NameNative {
        /// The runtime.
        runtime: String,
        /// The name.
        name: String,
    },
}

/// `twr account ...`.
pub(super) async fn run(cmd: AccountCmd, paths: &Paths, socket: &Path) -> anyhow::Result<()> {
    match cmd {
        AccountCmd::List { json } => list(socket, json).await,
        AccountCmd::Status { target, json } => status(socket, target.as_deref(), json).await,
        AccountCmd::Add {
            runtime,
            name,
            dir,
            share,
            native_name,
            device,
            no_login,
            login_args,
        } => {
            let req = AddRequest {
                runtime: runtime_name(&runtime).to_string(),
                name,
                dir,
                share,
                native_name,
                device,
                no_login,
                login_args,
            };
            add(paths, socket, req).await
        }
        AccountCmd::Login {
            runtime,
            name,
            device,
            login_args,
        } => {
            login(
                paths,
                socket,
                runtime_name(&runtime),
                &name,
                device,
                &login_args,
            )
            .await
        }
        AccountCmd::Remove {
            runtime,
            name,
            move_to,
            force,
            logout,
            purge,
            yes,
        } => {
            let req = RemoveRequest {
                runtime: runtime_name(&runtime).to_string(),
                name,
                move_to,
                force,
                logout,
                purge,
                yes,
            };
            remove(paths, socket, req).await
        }
        AccountCmd::NameNative { runtime, name } => {
            name_native(socket, runtime_name(&runtime), &name).await
        }
    }
}

/// The runtime `name` names in `twr account`: `claude` is `claude-code`, `grok` is
/// `grok-build`; anything else as written.
pub(super) fn runtime_name(name: &str) -> &str {
    match name {
        "claude" => "claude-code",
        "grok" => "grok-build",
        other => other,
    }
}

/// How the runtime calls itself, for a question: `Claude Code`.
fn product(runtime: &str) -> &str {
    match runtime {
        "claude-code" => "Claude Code",
        "codex" => "Codex",
        "grok-build" => "Grok Build",
        other => other,
    }
}

/// The runtime's command, for a message: `claude`.
fn program(runtime: &str) -> &str {
    match runtime {
        "claude-code" => "claude",
        "grok-build" => "grok",
        other => other,
    }
}

/// `path` with `$HOME` written `~`.
fn tilde(path: &Path) -> String {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|h| h.is_absolute() && h.parent().is_some());
    if let Some(rest) = home.as_deref().and_then(|h| path.strip_prefix(h).ok()) {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// The directory `runtime` uses with no account variable set, as this environment says
/// (`twr_adapters::account::own_native_dir`). What twr follows is the hub's ([`Natives`]);
/// this one only from a hub that does not say.
fn native_dir(runtime: &str) -> Option<PathBuf> {
    twr_adapters::account::own_native_dir(runtime)
}

/// A runtime's selector variable (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`) as the
/// hub's environment has it (`account.list`'s `native_var`).
#[derive(Clone, Debug, PartialEq)]
enum HubVar {
    Unset,
    Set(String),
    /// Set, to what the hub does not say (not UTF-8): it can't be passed on.
    Unreadable,
}

/// A runtime's own directory (the native account's) as the hub has it (`account.list`'s
/// `native_dir`, [`Natives`]).
#[derive(Clone, Debug, PartialEq)]
struct Native {
    /// The directory; `None` when none is known (no `HOME`).
    dir: Option<PathBuf>,
    /// The runtime's selector variable in the hub's environment; `None` when the hub does not
    /// say (an older one): `dir` is then this environment's ([`native_dir`]), and the
    /// variable is left as this environment has it.
    var: Option<HubVar>,
}

impl Native {
    /// Runtime `runtime`'s, from its entry of an `account.list` / `account.status` answer
    /// (with directories); this environment's from an entry without `native_dir`.
    fn of(runtime: &str, entry: &Value) -> Native {
        let Some(dir) = entry.get("native_dir") else {
            return Native {
                dir: native_dir(runtime),
                var: None,
            };
        };
        let v = &entry["native_var"];
        let var = match (v["set"].as_bool(), v["value"].as_str()) {
            (Some(false), _) => HubVar::Unset,
            (Some(true), Some(value)) => HubVar::Set(value.to_string()),
            _ => HubVar::Unreadable,
        };
        Native {
            dir: dir.as_str().filter(|d| !d.is_empty()).map(PathBuf::from),
            var: Some(var),
        }
    }

    /// Where it is said to be the runtime's own directory: `the hub's environment`, or
    /// `this environment` from a hub that does not say.
    fn whose(&self) -> &'static str {
        match self.var {
            Some(_) => "the hub's environment",
            None => "this environment",
        }
    }
}

/// Each runtime's own directory as the hub has it ([`Native`]), from an `account.list`
/// answer of every runtime.
struct Natives(std::collections::BTreeMap<String, Native>);

impl Natives {
    fn of(list: &Value) -> Natives {
        Natives(
            twr_core::account::ACCOUNT_RUNTIMES
                .iter()
                .map(|rt| (rt.to_string(), Native::of(rt, &list["runtimes"][*rt])))
                .collect(),
        )
    }

    /// Runtime `runtime`'s (this environment's for one the answer does not have).
    fn get(&self, runtime: &str) -> Native {
        self.0
            .get(runtime)
            .cloned()
            .unwrap_or_else(|| Native::of(runtime, &Value::Null))
    }
}

/// [`Natives`] as the hub has them now (`account.list`).
async fn natives(c: &mut twr_core::client::Client) -> anyhow::Result<Natives> {
    Ok(Natives::of(&c.call("account.list", json!({})).await?))
}

/// The environment `runtime`'s native account runs with to be on the login the hub's threads
/// on it use (`n`): the variables to set and the ones to remove, for
/// `twr_adapters::process::apply_env_std`. Its selector variable set to the hub's value when
/// the hub has it set, removed when the hub has it unset (so an export here can't redirect
/// the login); nothing from a hub that does not say (an older one: this environment's). An
/// error when the hub's value can't be passed on.
fn native_env(runtime: &str, n: &Native) -> anyhow::Result<twr_adapters::account::AccountEnv> {
    let Some(s) = twr_adapters::account::support(runtime) else {
        return Ok((Vec::new(), Vec::new()));
    };
    match &n.var {
        None => Ok((Vec::new(), Vec::new())),
        Some(HubVar::Unset) => Ok((Vec::new(), vec![s.var.to_string()])),
        Some(HubVar::Set(v)) => Ok((vec![(s.var.to_string(), v.clone())], Vec::new())),
        Some(HubVar::Unreadable) => bail!(
            "the hub's {} is set to what twr can't pass on (not UTF-8): run {}'s own commands \
             with the hub's {} yourself",
            s.var,
            program(runtime),
            s.var
        ),
    }
}

/// When this environment makes another directory `runtime`'s own than the hub does (`n`): a
/// line saying the hub's, and that twr follows it. `None` when they agree, or the hub does not
/// say.
fn follow_note(runtime: &str, n: &Native) -> Option<String> {
    let var = n.var.as_ref()?;
    let here = native_dir(runtime);
    let same = match (&n.dir, &here) {
        (Some(a), Some(b)) => same_dir(a, b),
        (a, b) => a == b,
    };
    if same {
        return None;
    }
    let name = twr_adapters::account::support(runtime).map_or("its variable", |s| s.var);
    let set = match var {
        HubVar::Unset => "unset",
        _ => "set",
    };
    let shown = |d: &Option<PathBuf>| d.as_deref().map_or_else(|| "none".into(), tilde);
    Some(format!(
        "note: the hub's {runtime} directory is {} ({name} {set} there), not {} as here: twr \
         follows the hub",
        shown(&n.dir),
        shown(&here)
    ))
}

/// The runtimes of an `account.list` / `account.status` answer, in name order.
fn runtimes(r: &Value) -> Vec<(&str, &Value)> {
    r["runtimes"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.as_str(), v))
        .collect()
}

/// A runtime entry's accounts.
fn accounts(entry: &Value) -> Vec<&Value> {
    entry["accounts"].as_array().into_iter().flatten().collect()
}

/// Account `name` of a runtime entry (removed ones too): the one the name stands for, as the
/// hub resolves it, never the native account whose name a live named account has too
/// ([`answers_to`]).
fn find<'a>(entry: &'a Value, name: &str) -> Option<&'a Value> {
    accounts(entry).into_iter().find(|a| answers_to(a, name))
}

/// Whether name `name` stands for account entry `a` (`twr_core::account::AccountInfo::
/// answers_to`): its name, unless it is the native account `shadowed` by a live named one of
/// that name (the native account fell back to `default` next to a named `default`, say).
fn answers_to(a: &Value, name: &str) -> bool {
    a["name"] == name && !flag(a, "shadowed")
}

fn flag(v: &Value, key: &str) -> bool {
    v[key].as_bool() == Some(true)
}

/// Whether an account counts towards showing labels, as the hub's label rule counts it: not
/// removed, and not last found logged out or missing (never checked counts).
fn counts(a: &Value) -> bool {
    !flag(a, "removed") && !matches!(a["login"].as_str(), Some("logged_out" | "missing"))
}

/// An account's last login check: `logged in (claude.ai)`, `logged out`, `unknown`, `not
/// installed`, `no directory`, `never checked`.
fn login_text(a: &Value) -> String {
    match a["login"].as_str() {
        None => "never checked".into(),
        Some("logged_in") => match a["method"].as_str() {
            Some(m) => format!("logged in ({m})"),
            None => "logged in".into(),
        },
        Some("logged_out") => "logged out".into(),
        Some("missing") if str_of(&a["note"]).contains("directory") => "no directory".into(),
        Some("missing") => "not installed".into(),
        Some(_) => "unknown".into(),
    }
}

/// [`login_text`] of a check made here ([`probe_here`]).
fn probed_text(info: &AgentInfo) -> String {
    if !info.found {
        return "not installed".into();
    }
    match info.login {
        AgentLogin::LoggedIn => match &info.method {
            Some(m) => format!("logged in ({m})"),
            None => "logged in".into(),
        },
        AgentLogin::LoggedOut => "logged out".into(),
        AgentLogin::Unknown => match &info.note {
            Some(n) => format!("unknown ({n})"),
            None => "unknown".into(),
        },
    }
}

/// The STATUS cell: [`login_text`], `removed`, with ` (native)` for the native account.
fn status_cell(a: &Value) -> String {
    let s = if flag(a, "removed") {
        "removed".to_string()
    } else {
        login_text(a)
    };
    if flag(a, "native") {
        format!("{s} (native)")
    } else {
        s
    }
}

/// The SOURCE cell: `config`, `twr` (its records), `config+twr`, `-` (the native account
/// nothing names).
fn source_cell(a: &Value) -> &'static str {
    match a["source"].as_str() {
        Some("config") => "config",
        Some("recorded") => "twr",
        Some("both") => "config+twr",
        _ => "-",
    }
}

/// The DIR cell: the account's directory (the native one's as the hub has it, from its
/// runtime's `entry`: [`Native`]), `~` for `$HOME`.
fn dir_cell(runtime: &str, entry: &Value, a: &Value) -> String {
    match a["dir"].as_str() {
        Some(d) => tilde(Path::new(d)),
        None if flag(a, "native") => Native::of(runtime, entry)
            .dir
            .map_or("-".into(), |d| tilde(&d)),
        None => "-".into(),
    }
}

/// Whether `twr account list` leaves a runtime out: its only account is its native one,
/// never checked (a single-account user's table stays empty).
fn quiet(entry: &Value) -> bool {
    accounts(entry)
        .iter()
        .all(|a| flag(a, "native") && a["login"].is_null() && !flag(a, "removed"))
}

/// The table of `runtimes` (`(runtime, entry)`), and a line for each runtime that shows
/// account labels.
fn accounts_table(runtimes: &[(&str, &Value)]) -> String {
    let header = ["RUNTIME", "ACCOUNT", "SOURCE", "STATUS", "THREADS", "DIR"];
    let mut rows: Vec<[String; 6]> = Vec::new();
    for (rt, entry) in runtimes {
        for a in accounts(entry) {
            rows.push([
                rt.to_string(),
                str_of(&a["name"]).to_string(),
                source_cell(a).to_string(),
                status_cell(a),
                a["threads"].as_u64().unwrap_or(0).to_string(),
                dir_cell(rt, entry, a),
            ]);
        }
    }
    let mut out = table(&header, &rows);
    for (rt, entry) in runtimes {
        if flag(entry, "shown") {
            let n = accounts(entry).into_iter().filter(|a| counts(a)).count();
            out.push_str(&format!(
                "{rt} shows account labels ({n} accounts logged in)\n"
            ));
        }
    }
    out
}

/// `twr account list`.
async fn list(socket: &Path, raw: bool) -> anyhow::Result<()> {
    let r = connect(socket)
        .await?
        .call("account.list", json!({}))
        .await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let shown: Vec<(&str, &Value)> = runtimes(&r)
        .into_iter()
        .filter(|(_, e)| !quiet(e))
        .collect();
    if shown.is_empty() {
        outln!(
            "no accounts but each runtime's own login (`twr{} account add <runtime> <name>` \
             adds one)",
            workspace_hint()
        );
        return Ok(());
    }
    out!("{}", accounts_table(&shown));
    Ok(())
}

/// `twr account status [<runtime>[:<name>]]`.
async fn status(socket: &Path, target: Option<&str>, raw: bool) -> anyhow::Result<()> {
    let (rt, name) = match target {
        Some(t) => {
            let (rt, name) = twr_core::account::split_runtime(t);
            (Some(runtime_name(rt)), name.filter(|n| !n.is_empty()))
        }
        None => (None, None),
    };
    let mut params = json!({ "refresh": true });
    if let Some(rt) = rt {
        params["runtime"] = json!(rt);
    }
    if let Some(n) = name {
        params["account"] = json!(n);
    }
    let mut c = connect(socket).await?;
    let r = c.call("account.status", params).await?;
    if raw {
        outln!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    // Every runtime's own directory (one runtime's answer has its own alone).
    let natives = match rt {
        Some(_) => natives(&mut c).await?,
        None => Natives::of(&r),
    };
    drop(c);
    // One account asked for: its row alone.
    let picked: Vec<(String, Value)> = runtimes(&r)
        .into_iter()
        .filter(|(_, e)| target.is_some() || !quiet(e))
        .map(|(rt, e)| {
            let mut e = e.clone();
            if let (Some(n), Some(list)) = (name, e["accounts"].as_array_mut()) {
                list.retain(|a| answers_to(a, n));
            }
            (rt.to_string(), e)
        })
        .collect();
    let picked: Vec<(&str, &Value)> = picked.iter().map(|(r, e)| (r.as_str(), e)).collect();
    if picked.is_empty() {
        outln!(
            "no accounts but each runtime's own login (`twr{} account add <runtime> <name>` \
             adds one)",
            workspace_hint()
        );
        return Ok(());
    }
    out!("{}", accounts_table(&picked));
    for note in status_notes(&picked, |k| std::env::var_os(k), &natives) {
        outln!("{note}");
    }
    for (rt, _) in &picked {
        if let Some(note) = follow_note(rt, &natives.get(rt)) {
            outln!("{note}");
        }
    }
    Ok(())
}

/// What `twr account status` adds under its table: each named account's links that are not
/// what `twr account add` made (`work: settings.json is no longer linked`), and what in the
/// settings picks its credentials (the hub's `shared_credentials`, [`credential_notes`]),
/// then each variable
/// of this environment (`var`) that a named account of a runtime listed does not get. A named
/// account whose directory is not its own alone (the hub's `conflict`, or a runtime's own
/// directory as the hub has it: [`native_here`]) gets why instead of its links.
/// `natives`: each runtime's own directory as the hub has it ([`Natives`]).
fn status_notes(
    runtimes: &[(&str, &Value)],
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
    natives: &Natives,
) -> Vec<String> {
    use twr_adapters::account::{
        check_links, credential_settings, link_items, settings_file, LinkState,
    };
    let mut notes = Vec::new();
    let mut removed_vars: Vec<&'static str> = Vec::new();
    for (rt, entry) in runtimes {
        let named: Vec<&Value> = accounts(entry)
            .into_iter()
            .filter(|a| !flag(a, "native") && !flag(a, "removed"))
            .collect();
        if named.is_empty() {
            continue;
        }
        if let Some(s) = twr_adapters::account::support(rt) {
            for k in s.remove {
                if var(k).is_some_and(|v| !v.is_empty()) && !removed_vars.contains(k) {
                    removed_vars.push(k);
                }
            }
        }
        // One whose directory is not its own alone: why, and no link checks (of the native
        // directory against itself, say).
        let named: Vec<&Value> = named
            .into_iter()
            .filter(|a| {
                let name = str_of(&a["name"]);
                // Its settings file the native one, which selects credentials: refused at use,
                // its directory still its own (its links are checked).
                if let Some(why) = a["shared_credentials"].as_str() {
                    notes.push(format!("{name}: can't be used: {why}"));
                }
                let why = match a["conflict"].as_str() {
                    Some(why) => Some(format!("can't be used: {why}")),
                    None => a["dir"]
                        .as_str()
                        .and_then(|d| native_here(Path::new(d), natives)),
                };
                match why {
                    Some(why) => {
                        notes.push(format!("{name}: {why}"));
                        false
                    }
                    None => true,
                }
            })
            .collect();
        let Some(native) = natives.get(rt).dir else {
            continue;
        };
        for a in named {
            let Some(dir) = a["dir"].as_str().map(Path::new).filter(|d| d.is_dir()) else {
                continue;
            };
            let share = match a["share"].as_str() {
                Some("none") => twr_core::account::Share::None,
                _ => twr_core::account::Share::Config,
            };
            let name = str_of(&a["name"]);
            let settings = settings_file(rt);
            for (item, state) in check_links(&native, dir, link_items(rt, share)) {
                let said = match state {
                    LinkState::Linked => continue,
                    LinkState::Replaced => "is no longer linked (a copy of its own now)",
                    LinkState::Elsewhere => "links somewhere else",
                    LinkState::Absent if native.join(&item).symlink_metadata().is_ok() => {
                        // Left out by `twr account add`: the native one selects credentials.
                        let keys = (settings == Some(item.as_str()))
                            .then(|| credential_settings(rt, &native.join(&item)).ok())
                            .flatten()
                            .filter(|k| !k.is_empty());
                        if let Some(keys) = keys {
                            notes.push(format!(
                                "{name}: {item} is not linked: the native one sets {}, which \
                                 would sign it in as the native account (copy it without them \
                                 to share it)",
                                keys.join(", ")
                            ));
                            continue;
                        }
                        "is not linked"
                    }
                    LinkState::Absent => continue,
                };
                notes.push(format!("{name}: {item} {said}"));
            }
            notes.extend(credential_notes(rt, name, a, dir, &native));
        }
    }
    for k in removed_vars {
        notes.push(format!(
            "{k} is set here; twr removes it for named accounts"
        ));
    }
    notes
}

/// `twr account status`'s notes on named account `name` of `rt` (`account.list`'s entry
/// `a`, in `dir`; `native`: the runtime's own directory) about the settings that pick its
/// credentials: its own settings file setting some (allowed: that may be how it signs in,
/// but its login then is not what `/login` made), or it reporting a key or cloud sign-in
/// (`api-key`, `bedrock`, `vertex`) while its settings file is the native one, which may
/// select those in a way twr doesn't know. (The native file setting ones twr knows refuses
/// the account: the hub's `conflict`.)
fn credential_notes(rt: &str, name: &str, a: &Value, dir: &Path, native: &Path) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let Some(file) = twr_adapters::account::settings_file(rt) else {
        return Vec::new();
    };
    let ino = |p: &Path| std::fs::metadata(p).ok().map(|m| (m.dev(), m.ino()));
    let own = dir.join(file);
    let Some(at) = ino(&own) else {
        return Vec::new();
    };
    if Some(at) == ino(&native.join(file)) {
        if a["shared_credentials"].is_string() {
            return Vec::new();
        }
        let method = a["method"].as_str().unwrap_or_default();
        if ["api-key", "bedrock", "vertex"].contains(&method) {
            return vec![format!(
                "{name}: signed in with {method} while its {file} is the native one's: check \
                 that file for what picks credentials, or give the account a copy of its own"
            )];
        }
        return Vec::new();
    }
    match twr_adapters::account::credential_settings(rt, &own) {
        Ok(keys) if !keys.is_empty() => vec![format!(
            "{name}: its own {file} sets {}: the account signs in with that, not with its login",
            keys.join(", ")
        )],
        _ => Vec::new(),
    }
}

/// `twr account add`'s request.
struct AddRequest {
    runtime: String,
    name: String,
    dir: Option<PathBuf>,
    share: Option<String>,
    native_name: Option<String>,
    device: bool,
    no_login: bool,
    login_args: Vec<String>,
}

impl AddRequest {
    /// The command that runs this add again (a login that failed: everything made so far is
    /// kept, and adopted).
    fn again(&self) -> String {
        let mut s = format!(
            "twr{} account add {} {}",
            workspace_hint(),
            self.runtime,
            self.name
        );
        if let Some(d) = &self.dir {
            s.push_str(&format!(
                " --dir {}",
                twr_core::paths::sh_word(&d.to_string_lossy())
            ));
        }
        if let Some(sh) = &self.share {
            s.push_str(&format!(" --share {sh}"));
        }
        if self.device {
            s.push_str(" --device");
        }
        if !self.login_args.is_empty() {
            let args: Vec<String> = self
                .login_args
                .iter()
                .map(|a| twr_core::paths::sh_word(a))
                .collect();
            s.push_str(&format!(" -- {}", args.join(" ")));
        }
        s
    }
}

/// `--dir` as an absolute path: `~/` for `$HOME`, a relative one from here.
fn absolute_dir(d: &Path) -> anyhow::Result<PathBuf> {
    let d = match d.strip_prefix("~") {
        Ok(rest) => {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_absolute())
                .ok_or_else(|| anyhow!("--dir {}: HOME is not set", d.display()))?;
            home.join(rest)
        }
        Err(_) => d.to_path_buf(),
    };
    std::path::absolute(&d).with_context(|| format!("--dir {}", d.display()))
}

/// `twr account add` (see [`AccountCmd::Add`]).
async fn add(paths: &Paths, socket: &Path, mut req: AddRequest) -> anyhow::Result<()> {
    refuse_agent_action("add accounts")?;
    let rt = req.runtime.clone();
    let name = req.name.clone();
    if !twr_core::account::takes_accounts(&rt) {
        bail!(
            "{rt:?} does not take accounts ({} do)",
            twr_core::account::ACCOUNT_RUNTIMES.join(", ")
        );
    }
    twr_core::account::check_name(&name).map_err(anyhow::Error::msg)?;
    if let Some(n) = &req.native_name {
        twr_core::account::check_name(n).map_err(|e| anyhow!("--native-name: {e}"))?;
        if *n == name {
            bail!("--native-name {n} is the new account's name: the two need different names");
        }
    }
    if let Some(d) = &req.dir {
        req.dir = Some(absolute_dir(d)?);
    }
    // 1. The native account as the hub has it: a --native-name that is its name already
    // renames nothing.
    let mut c = connect(socket).await?;
    let list = c.call("account.list", json!({ "runtime": rt })).await?;
    let entry = &list["runtimes"][rt.as_str()];
    // Every runtime's own directory as the hub has it: the links' source, and none an
    // account's directory may be.
    let natives = natives(&mut c).await?;
    let own = natives.get(&rt);
    let native = accounts(entry)
        .into_iter()
        .find(|a| flag(a, "native"))
        .cloned()
        .unwrap_or(Value::Null);
    let native_now = str_of(&native["name"]).to_string();
    let unnamed = native["source"] == "native";
    let first = !accounts(entry)
        .iter()
        .any(|a| !flag(a, "native") && !flag(a, "removed"));
    let mut native_name = req.native_name.clone().filter(|n| *n != native_now);

    // 2. The hub's preflight: what it would record, refused before anything is touched. With
    // a rename of the native account, the two together: the rename first, then the new
    // account as if it were done (its name may be the one the rename frees).
    let mut params = json!({ "runtime": rt, "account": name, "dry_run": true });
    if let Some(d) = &req.dir {
        params["dir"] = json!(d);
    }
    if let Some(s) = &req.share {
        params["share"] = json!(s);
    }
    let mut plan = preflight(&mut c, &mut params, native_name.as_deref()).await?;
    // Nothing is linked or logged in where that would reach another login.
    let dir_of = |plan: &Value| {
        plan["dir"]
            .as_str()
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("the hub did not say which directory {rt}:{name} has"))
    };
    let known = find(entry, &name).cloned().unwrap_or(Value::Null);
    refuse_shared_dir(
        &rt,
        &name,
        &known,
        &dir_of(&plan)?,
        &format!("not setting {rt}:{name} up"),
        &natives,
    )?;
    if let Some(note) = follow_note(&rt, &own) {
        outln!("{note}");
    }
    // The native account's name, when nothing names it yet: asked, then checked together
    // with the new account again.
    if native_name.is_none() && req.native_name.is_none() && unnamed && first {
        let here = probe_here(&rt, None, &own).await;
        if here.found && here.login == AgentLogin::LoggedIn {
            native_name = ask_native_name(&rt, &native_now, &name)?;
            if native_name.is_some() {
                plan = preflight(&mut c, &mut params, native_name.as_deref()).await?;
            }
        }
    }
    drop(c);
    let config = flag(&plan, "config");
    let existing = flag(&plan, "existing");
    let dir = dir_of(&plan)?;
    let share = plan["share"].as_str().unwrap_or("config").to_string();

    // 3. The directory, private: one twr places in the accounts root only where it really is
    // inside it (no link on the way leads elsewhere; `twr_core::account::resolve_in_root`).
    let root = paths.accounts_dir();
    let real = if dir == twr_core::account::default_dir(&root, &rt, &name) {
        twr_core::account::resolve_in_root(&root, &rt, &dir, true).map_err(anyhow::Error::msg)?
    } else {
        dir.clone()
    };
    let made = private_dir(&real)?;
    if made {
        outln!("made {} (mode 700)", tilde(&dir));
    }
    // Collisions again, now that it exists: a directory reached another way than another
    // account's or a runtime's own (through a link) is the same one only once both resolve.
    let doing = format!("not setting {rt}:{name} up");
    let mut recheck = params.clone();
    recheck["dir"] = json!(dir);
    let again = match connect(socket).await {
        Ok(mut c) => c.call("account.add", recheck).await.map(drop),
        Err(e) => Err(e),
    };
    let again = again
        .map_err(|e| anyhow!("{doing}: {e:#}"))
        .and_then(|()| refuse_shared_dir(&rt, &name, &Value::Null, &dir, &doing, &natives));
    if let Err(e) = again {
        if made {
            // (Empty: nothing was linked or logged in yet.)
            let _ = std::fs::remove_dir(&real);
        }
        return Err(e);
    }

    // 4. The links to the native account's settings.
    if share != "none" {
        link(&rt, &real, &share, &own)?;
    }

    // 5. The login, in this terminal.
    let mut login_end = None;
    if !req.no_login {
        let ran = run_login(&rt, &name, Some(&dir), req.device, &req.login_args, &own).await?;
        login_end = Some(Utc::now());
        if ran.success() {
            note_login(&root, &rt, &name, Some(&dir));
        } else {
            let finish = if config || existing {
                format!("`twr{} account login {rt} {name}`", workspace_hint())
            } else {
                format!("`{}`", req.again())
            };
            bail!(
                "{}'s login {}; {} is kept: {finish} finishes setting {rt}:{name} up",
                program(&rt),
                ended(&ran),
                tilde(&dir)
            );
        }
    }

    // 6. The hub's record (none for an account config.toml declares).
    let mut c = connect(socket).await?;
    if let Some(n) = &native_name {
        let r = c
            .call(
                "account.add",
                json!({ "runtime": rt, "account": n, "native": true }),
            )
            .await?;
        if flag(&r, "recorded") {
            outln!("{rt}'s own login ({}) is now named {n}", tilde_native(&own));
        }
    }
    params["dry_run"] = json!(false);
    params["dir"] = json!(dir);
    // (Renamed above: the hub checks the name against the registry as it is now.)
    if let Some(p) = params.as_object_mut() {
        p.remove("after_native_name");
    }
    let r = c.call("account.add", params).await?;
    if flag(&r, "config") {
        outln!("{rt}:{name} is declared in config.toml ([accounts.{rt}.{name}]): nothing recorded");
    } else if flag(&r, "recorded") {
        outln!("added {rt}:{name} ({})", tilde(&dir));
    } else {
        outln!("{rt}:{name} was an account already ({})", tilde(&dir));
    }
    drop(c);

    // 7. What its login check says, and what that does to the labels.
    after_login(socket, &rt, &name, Some(&dir), login_end, &own).await
}

/// `twr account add`'s dry run of `params` (`account.add {dry_run}`), its answer: with
/// `native_name`, the native account's rename to it first (`account.add {native, dry_run}`),
/// then `params` as if it were done (`after_native_name`, kept in `params` for the dry runs
/// after it); without, `params` alone.
async fn preflight(
    c: &mut twr_core::client::Client,
    params: &mut Value,
    native_name: Option<&str>,
) -> anyhow::Result<Value> {
    let Some(p) = params.as_object_mut() else {
        bail!("account.add parameters are not an object");
    };
    p.remove("after_native_name");
    if let Some(n) = native_name {
        let check = json!({
            "runtime": p["runtime"].clone(),
            "account": n,
            "native": true,
            "dry_run": true,
        });
        c.call("account.add", check).await?;
        p.insert("after_native_name".into(), json!(n));
    }
    c.call("account.add", params.clone()).await
}

/// `~/.claude`, or what the hub makes the runtime's own directory (`n`).
fn tilde_native(n: &Native) -> String {
    n.dir
        .as_deref()
        .map_or_else(|| "its own directory".into(), tilde)
}

/// Asks on a terminal what to call the runtime's current login (`current`: its name now);
/// `None` keeps it. Without a terminal, says it keeps its name.
fn ask_native_name(runtime: &str, current: &str, new: &str) -> anyhow::Result<Option<String>> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        outln!(
            "your current {} login stays usable as account {current} (--native-name names it \
             otherwise)",
            product(runtime)
        );
        return Ok(None);
    }
    loop {
        let answer = ask_on_stderr(&format!(
            "Your current {} login stays usable as an account. Name it [{current}]: ",
            product(runtime)
        ))?;
        let answer = answer.trim();
        if answer.is_empty() || answer == current {
            return Ok(None);
        }
        if answer == new {
            errln!("{new} is the new account's name: pick another one");
            continue;
        }
        match twr_core::account::check_name(answer) {
            Ok(()) => return Ok(Some(answer.to_string())),
            Err(e) => errln!("{e}"),
        }
    }
}

/// Makes account directory `dir` (and the directories above it that are missing), mode 700;
/// one that exists must be a directory of this user that no other user can read or write.
/// Whether it made it.
fn private_dir(dir: &Path) -> anyhow::Result<bool> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    match std::fs::metadata(dir) {
        Ok(m) => {
            check_private(dir, &m)?;
            Ok(false)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .with_context(|| format!("making {}", dir.display()))?;
            // Whatever the umask left.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("making {} private", dir.display()))?;
            Ok(true)
        }
        Err(e) => Err(anyhow!("reading {}: {e}", dir.display())),
    }
}

/// Refuses account directory `dir` (its metadata `m`, links followed) unless it is a
/// directory of this user that no other user can read or write.
fn check_private(dir: &Path, m: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !m.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    if m.uid() != twr_core::paths::effective_uid() {
        bail!("{} belongs to another user", dir.display());
    }
    let mode = m.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "{} is readable by others (mode {mode:o}): chmod 700 it",
            dir.display()
        );
    }
    Ok(())
}

/// Named account directory `dir` of `runtime` checked before a login or a logout runs in it,
/// as `twr account add` makes them: one in `<accounts root>/<runtime>/` (`root`) only where
/// it really is inside it once links are followed (`twr_core::account::resolve_in_root`), and
/// the directory it resolves to one of this user's, private ([`check_private`]). The resolved
/// directory; `None` when nothing is there.
fn checked_dir(root: &Path, runtime: &str, dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    if let Err(e) = dir.symlink_metadata() {
        if e.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        bail!("reading {}: {e}", dir.display());
    }
    let real = if dir.parent() == Some(root.join(runtime).as_path()) {
        twr_core::account::resolve_in_root(root, runtime, dir, false).map_err(anyhow::Error::msg)?
    } else {
        dir.to_path_buf()
    };
    match std::fs::metadata(&real) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => bail!("reading {}: {e}", real.display()),
        Ok(m) => {
            check_private(&real, &m)?;
            Ok(Some(real))
        }
    }
}

/// Whether `a` and `b` are one directory: the same path, or the same directory once
/// resolved.
fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// Why named account directory `dir` is not that account's alone, so that a login, a logout
/// or a deletion in it would reach another login too: a runtime's own directory as the hub
/// has it (`natives`, [`Natives`]); `None` when it is its own.
fn native_here(dir: &Path, natives: &Natives) -> Option<String> {
    let (owner, n) = twr_core::account::ACCOUNT_RUNTIMES.iter().find_map(|rt| {
        let n = natives.get(rt);
        n.dir
            .as_deref()
            .is_some_and(|d| same_dir(d, dir))
            .then_some((rt, n))
    })?;
    let var = twr_adapters::account::support(owner).map_or("its variable", |s| s.var);
    Some(format!(
        "{} is {owner}'s own directory in {} ({var}, or its default)",
        tilde(dir),
        n.whose()
    ))
}

/// Refuses what would act on named account `rt:name`'s directory `dir` (`account.list`'s
/// entry `a`) when it is not that account's alone: the hub says it collides with a runtime's
/// own directory or another account's (`conflict`), or it is a runtime's own directory as the
/// hub has it (`natives`, [`native_here`]). `doing` says what is refused (`not logging
/// claude-code:work in`).
fn refuse_shared_dir(
    rt: &str,
    name: &str,
    a: &Value,
    dir: &Path,
    doing: &str,
    natives: &Natives,
) -> anyhow::Result<()> {
    if let Some(why) = a["conflict"].as_str() {
        bail!("{doing}: {rt}:{name} can't be used: {why}");
    }
    if let Some(why) = native_here(dir, natives) {
        bail!("{doing}: {why}");
    }
    Ok(())
}

/// Links the native account's settings (in `own`, its directory as the hub has it) into
/// account directory `dir` (share level `share`) and says what it did, and what it left out
/// and why: a native settings file that selects credentials
/// (`twr_adapters::account::make_links`), which twr never edits.
fn link(runtime: &str, dir: &Path, share: &str, own: &Native) -> anyhow::Result<()> {
    let share = match share {
        "none" => twr_core::account::Share::None,
        _ => twr_core::account::Share::Config,
    };
    let Some(native) = own.dir.clone() else {
        outln!(
            "no native {} directory in {} (HOME is not set): nothing linked",
            product(runtime),
            own.whose()
        );
        return Ok(());
    };
    let items = twr_adapters::account::link_items(runtime, share);
    let report = twr_adapters::account::make_links(runtime, &native, dir, items)
        .with_context(|| format!("linking {} to {}", tilde(dir), tilde(&native)))?;
    let n = tilde(&native);
    if !report.linked.is_empty() {
        outln!("linked to {n}: {}", report.linked.join(", "));
    }
    if !report.kept.is_empty() {
        outln!("kept (already there): {}", report.kept.join(", "));
    }
    if !report.missing.is_empty() {
        outln!("not in {n}, not linked: {}", report.missing.join(", "));
    }
    for (item, why) in &report.left_out {
        outln!("{}", left_out_line(item, why));
    }
    Ok(())
}

/// `twr account add`'s line for native item `item` that [`link`] left out (`why`).
fn left_out_line(item: &str, why: &twr_adapters::account::LeftOut) -> String {
    use twr_adapters::account::LeftOut;
    match why {
        LeftOut::Credentials(keys) => format!(
            "{item} sets {}, which would sign every account in as the native one: left out; \
             copy it without {} if you want it shared",
            keys.join(", "),
            if keys.len() == 1 {
                "that key"
            } else {
                "those keys"
            }
        ),
        LeftOut::Unchecked(e) => format!(
            "{item} can't be checked for credentials ({}): left out; copy it if you want it \
             shared",
            e.strip_prefix("it ").unwrap_or(e)
        ),
    }
}

/// How a process ended: `ended with exit 1`, `was ended by a signal`.
fn ended(status: &ExitStatus) -> String {
    match status.code() {
        Some(c) => format!("ended with exit {c}"),
        None => "was ended by a signal".into(),
    }
}

/// Runs `runtime`'s login (`twr_adapters::account::login_argv`, then `extra`) in this
/// terminal, in the foreground, with account `name`'s environment (`dir`; `None`: the native
/// account, with the hub's selector variable: `own`, [`native_env`]). The program is found as
/// a thread's spawn finds it.
async fn run_login(
    runtime: &str,
    name: &str,
    dir: Option<&Path>,
    device: bool,
    extra: &[String],
    own: &Native,
) -> anyhow::Result<ExitStatus> {
    let Some(argv) = twr_adapters::account::login_argv(runtime, device) else {
        bail!("{runtime} has no login command");
    };
    let mut args: Vec<String> = argv.iter().map(|s| s.to_string()).collect();
    args.extend(extra.iter().cloned());
    let mut cmd =
        twr_adapters::agents::agent_command(runtime, &args, &[], None, |k| std::env::var_os(k))?;
    let with = match dir {
        Some(d) => {
            let (set, remove) = twr_adapters::account::account_env(runtime, d).unwrap_or_default();
            let var = set.first().map(|(k, _)| k.clone()).unwrap_or_default();
            twr_adapters::process::apply_env_std(&mut cmd, set, &remove);
            format!(" with {var}={}", tilde(d))
        }
        None => {
            let (set, remove) = native_env(runtime, own)
                .map_err(|e| anyhow!("not logging {runtime}:{name} in: {e:#}"))?;
            let with = match set.first() {
                Some((k, v)) => format!(" with {k}={}", tilde(Path::new(v))),
                None => String::new(),
            };
            if !set.is_empty() || !remove.is_empty() {
                twr_adapters::process::apply_env_std(&mut cmd, set, &remove);
            }
            with
        }
    };
    outln!(
        "logging {runtime}:{name} in: {} {}{with} runs in this terminal; twr doesn't see what \
         you type",
        program(runtime),
        args.join(" ")
    );
    // In this terminal and its process group: the login's questions are the user's.
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.status()
        .await
        .map_err(|e| anyhow!("running {}'s login: {e}", program(runtime)))
}

/// Checks the login of `runtime`'s account `dir` (`None`: the native account, with the hub's
/// selector variable: `own`, [`native_env`]) here, with the account's environment: what the
/// hub's own check runs.
async fn probe_here(runtime: &str, dir: Option<&Path>, own: &Native) -> AgentInfo {
    let (env, remove) = match dir {
        Some(d) => twr_adapters::account::account_env(runtime, d).unwrap_or_default(),
        None => match native_env(runtime, own) {
            Ok(env) => env,
            Err(e) => {
                return AgentInfo {
                    runtime: runtime.to_string(),
                    found: true,
                    login: AgentLogin::Unknown,
                    note: Some(format!("{e:#}")),
                    ..Default::default()
                }
            }
        },
    };
    twr_adapters::agents::probe_account(
        runtime,
        &env,
        &remove,
        |k| std::env::var_os(k),
        twr_adapters::agents::COMMAND_TIMEOUT,
    )
    .await
}

/// After a login twr ran in directory `dir` (`None`: none is known) ended successfully: its
/// login note under the accounts root `root` (`twr_core::account::note_login`), written before
/// and whatever the hub's check finds, so that no session from before the login is resumed by
/// a move back, in any workspace (they share the root), its hub running or not. One that
/// cannot be written is said (the hub's own record of the login still counts in its
/// workspace).
fn note_login(root: &Path, runtime: &str, name: &str, dir: Option<&Path>) {
    let Some(dir) = dir else {
        errln!(
            "twr: {runtime}:{name}'s directory is not known, so the login is not noted for \
             other workspaces: a move back there may resume a session from before it"
        );
        return;
    };
    if let Err(e) = twr_core::account::note_login(root, dir) {
        errln!(
            "twr: cannot note {runtime}:{name}'s login in {} ({e}): a move back in another \
             workspace may resume a session from before it",
            tilde(&twr_core::account::login_note_path(root, dir))
        );
    }
}

/// After a login: has the hub check account `name` again and prints what it found (`work:
/// logged in (claude.ai)`), then whether `runtime`'s threads show their account now. The
/// hub's state counts only while it is of what was logged in ([`hub_state_of`]: the account
/// still the native one (`dir` `None`) or still in `dir`, checked after the login ended at
/// `ended`); else it is checked here, in `dir` (`own`: the native account's directory and
/// selector as the hub has them), and a change of the account meanwhile is said.
async fn after_login(
    socket: &Path,
    runtime: &str,
    name: &str,
    dir: Option<&Path>,
    ended: Option<DateTime<Utc>>,
    own: &Native,
) -> anyhow::Result<()> {
    // A login that ran (`ended`) is recorded as one (`after_login`): a session from before
    // it may be another login's, and a move back does not resume it.
    let mut params = json!({ "runtime": runtime, "account": name, "refresh": true });
    if ended.is_some() {
        params["after_login"] = json!(true);
    }
    let r = connect(socket)
        .await?
        .call("account.status", params)
        .await?;
    let entry = &r["runtimes"][runtime];
    let found = find(entry, name);
    let (said, changed) = match hub_state_of(found, dir, ended) {
        Ok(said) => (said, None),
        Err(changed) => (probed_text(&probe_here(runtime, dir, own).await), changed),
    };
    outln!("{name}: {said}");
    if let Some(why) = changed {
        let what = dir.map_or_else(|| tilde_native(own), tilde);
        outln!("{runtime}:{name} {why} during the login: the state above is the login in {what}");
    }
    outln!("{}", label_effect(runtime, entry));
    Ok(())
}

/// What the hub's entry `a` of the account logged in says of its login ([`login_text`]),
/// when it is of what was logged in: still the native account (`dir` `None`) or a named one in
/// `dir` ([`same_dir`]), checked after the login ended (`ended`; `None`: no login ran).
/// Else why not: `Err(Some(change))` when the account changed meanwhile (`was removed`,
/// `moved to another directory (~/x)`, ...), `Err(None)` when the hub has no check of it
/// since.
fn hub_state_of(
    a: Option<&Value>,
    dir: Option<&Path>,
    ended: Option<DateTime<Utc>>,
) -> Result<String, Option<String>> {
    let Some(a) = a else {
        return Err(Some("stopped being an account".into()));
    };
    if flag(a, "removed") {
        return Err(Some("was removed".into()));
    }
    match (dir, flag(a, "native")) {
        (None, true) => {}
        (None, false) => return Err(Some("stopped being the native account".into())),
        (Some(_), true) => return Err(Some("became the native account".into())),
        (Some(d), false) => match a["dir"].as_str().map(Path::new) {
            Some(now) if same_dir(now, d) => {}
            Some(now) => return Err(Some(format!("moved to another directory ({})", tilde(now)))),
            None => return Err(Some("lost its directory".into())),
        },
    }
    if a["login"].is_null() {
        return Err(None);
    }
    let checked = a["checked_at"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    match (ended, checked) {
        (None, _) => Ok(login_text(a)),
        (Some(end), Some(at)) if at >= end => Ok(login_text(a)),
        _ => Err(None),
    }
}

/// What `runtime`'s accounts (`entry`) do to its labels: `claude-code now has 2 logged-in
/// accounts (personal, work), so its threads show their account.`
fn label_effect(runtime: &str, entry: &Value) -> String {
    let names: Vec<&str> = accounts(entry)
        .into_iter()
        .filter(|a| counts(a))
        .map(|a| str_of(&a["name"]))
        .collect();
    let n = names.len();
    if flag(entry, "shown") {
        format!(
            "{runtime} now has {n} logged-in accounts ({}), so its threads show their account.",
            names.join(", ")
        )
    } else if n == 1 {
        format!("{runtime} has 1 logged-in account; labels stay hidden")
    } else {
        format!("{runtime} has {n} logged-in accounts; labels stay hidden")
    }
}

/// `twr account login` (see [`AccountCmd::Login`]).
async fn login(
    paths: &Paths,
    socket: &Path,
    runtime: &str,
    name: &str,
    device: bool,
    extra: &[String],
) -> anyhow::Result<()> {
    refuse_agent_action("log accounts in")?;
    let mut c = connect(socket).await?;
    let list = c
        .call("account.list", json!({ "runtime": runtime }))
        .await?;
    let natives = natives(&mut c).await?;
    drop(c);
    let own = natives.get(runtime);
    let entry = &list["runtimes"][runtime];
    let Some(a) = find(entry, name).filter(|a| !flag(a, "removed")) else {
        bail!(
            "{runtime} has no account {name:?} (`twr{w} account list`; `twr{w} account add \
             {runtime} {name}` adds it)",
            w = workspace_hint()
        );
    };
    let dir = if flag(a, "native") {
        // In the directory the hub's threads on it use, with the hub's selector variable.
        if let Some(note) = follow_note(runtime, &own) {
            outln!("{note}");
        }
        None
    } else {
        let Some(d) = a["dir"].as_str().map(PathBuf::from) else {
            bail!("the hub did not say which directory {runtime}:{name} has");
        };
        // As `add` has it: inside the accounts directory once resolved (one there), a
        // directory of this user's, private. The login gets the path as configured.
        if checked_dir(&paths.accounts_dir(), runtime, &d)
            .map_err(|e| anyhow!("not logging {runtime}:{name} in: {e:#}"))?
            .is_none()
        {
            bail!(
                "{} does not exist: `twr{} account add {runtime} {name}` makes it",
                tilde(&d),
                workspace_hint()
            );
        }
        refuse_shared_dir(
            runtime,
            name,
            a,
            &d,
            &format!("not logging {runtime}:{name} in"),
            &natives,
        )?;
        Some(d)
    };
    let ran = run_login(runtime, name, dir.as_deref(), device, extra, &own).await?;
    let end = Utc::now();
    if !ran.success() {
        bail!("{}'s login {}", program(runtime), ended(&ran));
    }
    // The native account's login is in its directory as the hub has it.
    let logged_in = dir.clone().or_else(|| own.dir.clone());
    note_login(&paths.accounts_dir(), runtime, name, logged_in.as_deref());
    after_login(socket, runtime, name, dir.as_deref(), Some(end), &own).await?;
    if let Some(note) = retained_note(runtime, name, a) {
        outln!("{note}");
    }
    Ok(())
}

/// What `twr account login` of `runtime:name` (account `a` of `account.list`) says of the
/// threads on it given its directory by another path than the one it signed in
/// (`retained_paths`): a runtime may key the login on the path (Claude Code's macOS
/// Keychain entry), so each needs `twr model <thread> --account <name>`, which moves it to the
/// configured path (a new session). `None` when there are none.
fn retained_note(runtime: &str, name: &str, a: &Value) -> Option<String> {
    let retained: Vec<(&str, &str)> = a["retained_paths"]
        .as_array()?
        .iter()
        .filter_map(|r| Some((r["thread"].as_str()?, r["path"].as_str()?)))
        .collect();
    if retained.is_empty() {
        return None;
    }
    let mut out = format!(
        "note: {} {} {runtime}:{name} by another path, which this login may not sign in:",
        retained
            .iter()
            .map(|(t, _)| *t)
            .collect::<Vec<_>>()
            .join(", "),
        if retained.len() == 1 {
            "reaches"
        } else {
            "reach"
        },
    );
    for (thread, path) in retained {
        out.push_str(&format!(
            "\n  {thread} ({}): twr{} model {thread} --account {name} moves it to the configured \
             path (a new session)",
            tilde(Path::new(path)),
            workspace_hint()
        ));
    }
    Some(out)
}

/// `twr account remove`'s request.
struct RemoveRequest {
    runtime: String,
    name: String,
    move_to: Option<String>,
    force: bool,
    logout: bool,
    purge: bool,
    yes: bool,
}

/// `twr account remove` (see [`AccountCmd::Remove`]). The hub's removal comes first, so a
/// removal it refuses (threads on the account, an account only config.toml declares) logs
/// nothing out and deletes nothing; then the logout, then the directory, each only in the
/// directory the hub says it removed the account with, and only when that is the one checked
/// (and asked about) before. Neither runs while a runtime still runs on the login (the hub's
/// `running`: a moved thread finishing its turn, one left on it): the CLI waits for them
/// ([`wait_off_account`], at most [`remove_wait`]), then refuses with what to run by hand;
/// nor after the hub failed to stop a moved thread's runtime (`dispatch_error`), nor when
/// only twr's record of an account config.toml declares went (`config`: the account stays).
/// The logout is checked again right before it runs: never in a directory a live account
/// has by then ([`live_owner`]: this one added back during the wait too), nor in a runtime's
/// own. Neither runs while another workspace may use the directory, asked before the removal
/// and again right before each ([`refuse_other_workspaces`]). A logout that fails is an
/// error, and the directory is not deleted then.
async fn remove(paths: &Paths, socket: &Path, req: RemoveRequest) -> anyhow::Result<()> {
    refuse_agent_action("remove accounts")?;
    let (rt, name) = (req.runtime.as_str(), req.name.as_str());
    let root = paths.accounts_dir();
    let mut c = connect(socket).await?;
    let mut dir: Option<PathBuf> = None;
    if req.logout || req.purge {
        let list = c.call("account.list", json!({ "runtime": rt })).await?;
        let entry = &list["runtimes"][rt];
        let natives = natives(&mut c).await?;
        let Some(a) = find(entry, name).filter(|a| !flag(a, "removed")) else {
            bail!(
                "{rt} has no account {name:?} (`twr{} account list`)",
                workspace_hint()
            );
        };
        if flag(a, "native") {
            bail!(
                "{rt}:{name} is {rt}'s own login ({}): twr never logs it out or deletes it",
                tilde_native(&natives.get(rt))
            );
        }
        dir = a["dir"].as_str().map(PathBuf::from);
        let doing = match (req.logout, req.purge) {
            (true, true) => "--logout and --purge are refused",
            (true, false) => "--logout is refused",
            _ => "--purge is refused",
        };
        if let Some(d) = &dir {
            refuse_shared_dir(rt, name, a, d, doing, &natives)?;
            if req.logout {
                checked_dir(&root, rt, d).map_err(|e| anyhow!("{doing}: {e:#}"))?;
            }
        }
        if req.purge {
            if a["source"] != "recorded" {
                bail!(
                    "{rt}:{name} is declared in config.toml ([accounts.{rt}.{name}]): --purge \
                     deletes only the directories of accounts twr recorded"
                );
            }
            let Some(d) = &dir else {
                bail!("the hub did not say which directory {rt}:{name} has");
            };
            let all = c.call("account.list", json!({})).await?;
            let live = live_dirs(&all, Some((rt, name)));
            purgeable(&root, rt, d, &Natives::of(&all), &live)?;
        }
        // The directory is the root's: another workspace may have an account in it too.
        if let Some(d) = &dir {
            refuse_other_workspaces(paths, d, doing).await?;
        }
        if let Some(d) = dir.as_ref().filter(|_| req.purge) {
            let mut ask = ask_on_stderr;
            match consent(req.yes, &mut ask) {
                Consent::Yes => {}
                Consent::Ask(ask) => {
                    let answer = ask(&format!(
                        "delete {} (the login in it too)? [y/N] ",
                        tilde(d)
                    ))?;
                    if !matches!(answer.trim(), "y" | "Y" | "yes") {
                        bail!("nothing removed");
                    }
                }
                Consent::NoTerminal => bail!(
                    "not deleting {} without confirmation (no terminal to ask on): pass --yes",
                    tilde(d)
                ),
            }
        }
    }
    let mut params = json!({ "runtime": rt, "account": name, "force": req.force });
    if let Some(to) = &req.move_to {
        params["move_to"] = json!(to);
    }
    let r = c.call("account.remove", params).await?;
    outln!("{}", localize(str_of(&r["message"])));
    let dispatch_error = r["dispatch_error"].as_str();
    if let Some(e) = dispatch_error {
        errln!("twr: the threads moved, but stopping their runtimes failed: {e}");
    }
    let Some(dir) = dir else {
        return Ok(());
    };
    let not = match (req.logout, req.purge) {
        (true, true) => "not logging it out or deleting anything",
        (true, false) => "not logging it out",
        _ => "not deleting anything",
    };
    removed_from(&r, rt, name, &dir).map_err(|e| anyhow!("{e}: {not}"))?;
    // Only twr's record of an account config.toml declares went: the account stays, its
    // login in use.
    if flag(&r, "config") {
        bail!(
            "{not}: config.toml still declares {rt}:{name} ([accounts.{rt}.{name}]), so the \
             account stays (only twr's record of it was removed)"
        );
    }
    // A runtime still on the login may read and write its directory: no logout or purge
    // under it, and none after a stop that failed (it may run on).
    let running = running_of(&r);
    let by_hand = || by_hand(rt, name, &dir, req.logout, req.purge);
    if dispatch_error.is_some() {
        bail!(
            "{not}: a runtime may still run on {rt}:{name}. Once it stops, {}",
            by_hand()
        );
    }
    if !running.is_empty() {
        let left = wait_off_account(&mut c, rt, name, &dir, running, remove_wait()).await?;
        if !left.is_empty() {
            // Where they run: on the login removed now, or (another runtime's login in
            // its directory) in that directory, each with its login.
            let this = format!("{rt}:{name}");
            let names: Vec<&str> = left.iter().map(|(t, _)| t.as_str()).collect();
            let on = match left.iter().all(|(_, login)| *login == this) {
                true => format!("on {this}"),
                false => format!(
                    "in {} ({})",
                    tilde(&dir),
                    left.iter()
                        .map(|(t, login)| format!("{t} on {login}, removed"))
                        .collect::<Vec<_>>()
                        .join("; ")
                ),
            };
            bail!(
                "{not}: {} still {} {on}. Once {} ({}), {}",
                names.join(", "),
                if names.len() == 1 { "runs" } else { "run" },
                if names.len() == 1 {
                    "it stops"
                } else {
                    "they stop"
                },
                names
                    .iter()
                    .map(|t| format!("twr stop {t}"))
                    .collect::<Vec<_>>()
                    .join("; "),
                by_hand()
            );
        }
    }
    if req.logout {
        // Checked again right before: the login and the directory as they are now (the hub's
        // own directories as it has them now).
        let all = c.call("account.list", json!({})).await?;
        if let Some(why) = native_here(&dir, &Natives::of(&all)) {
            bail!("{not}: {why}");
        }
        // The directory may be a live account's again (this one added back during the wait,
        // another added in it): its login is in use.
        if let Some(why) = live_owner(&live_dirs(&all, None), rt, name, &dir) {
            bail!("{not}: {why}");
        }
        // A runtime of any runtime still on a removed login in it (one the wait did not
        // know of).
        let threads = c.call("thread.list", Value::Null).await?;
        if let Some(why) = live_session(&threads, rt, name, &dir) {
            bail!("{not}: {why}. Once it stops, {}", by_hand());
        }
        refuse_other_workspaces(paths, &dir, not).await?;
        match checked_dir(&root, rt, &dir).map_err(|e| anyhow!("{not}: {e:#}"))? {
            None => outln!("{} is gone: nothing to log out", tilde(&dir)),
            Some(_) => logout(rt, name, &dir).await.map_err(|why| {
                let kept = match req.purge {
                    true => format!(" ({} is not deleted)", tilde(&dir)),
                    false => String::new(),
                };
                anyhow!("the account was removed, but logging it out failed: {why}{kept}")
            })?,
        }
    }
    if req.purge {
        // Checked again right before: what is deleted is the resolved directory, holding no
        // live account's directory, here or in another workspace.
        refuse_other_workspaces(paths, &dir, "not deleting anything").await?;
        let all = c.call("account.list", json!({})).await?;
        let threads = c.call("thread.list", Value::Null).await?;
        if let Some(why) = live_session(&threads, rt, name, &dir) {
            bail!(
                "not deleting anything: {why}. Once it stops, {}",
                self::by_hand(rt, name, &dir, false, true)
            );
        }
        match purgeable(&root, rt, &dir, &Natives::of(&all), &live_dirs(&all, None))? {
            None => outln!("{} is gone already", tilde(&dir)),
            Some(real) => {
                std::fs::remove_dir_all(&real)
                    .with_context(|| format!("deleting {}", real.display()))?;
                outln!("deleted {}", tilde(&dir));
            }
        }
    }
    Ok(())
}

/// Refuses unless the hub's `account.remove` answer `r` says it removed `rt:name` with
/// directory `dir`, the one checked (and asked about) before: what the hub removed, not what
/// was read before the question (another client may have removed the account and added it
/// again elsewhere meanwhile).
fn removed_from(r: &Value, rt: &str, name: &str, dir: &Path) -> Result<(), String> {
    match r["dir"].as_str().map(Path::new) {
        Some(removed) if same_dir(removed, dir) => Ok(()),
        Some(removed) => Err(format!(
            "{rt}:{name} was in {} when the hub removed it, not in {} as before",
            tilde(removed),
            tilde(dir)
        )),
        None => Err(format!(
            "the hub did not say which directory {rt}:{name} was removed with ({} before)",
            tilde(dir)
        )),
    }
}

/// How long `twr account remove` waits for the runtimes still on the login it removed before
/// refusing its `--logout` / `--purge` ([`wait_off_account`]): long enough for a turn to end.
/// A debug build (the tests') takes a shorter one from `TWR_TEST_REMOVE_WAIT_MS`.
fn remove_wait() -> std::time::Duration {
    const WAIT: std::time::Duration = std::time::Duration::from_secs(600);
    let test = std::env::var("TWR_TEST_REMOVE_WAIT_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|v| v.parse().ok());
    match test {
        Some(ms) => std::time::Duration::from_millis(ms).min(WAIT),
        None => WAIT,
    }
}

/// The threads of an `account.remove` answer `r` whose runtime still runs on the login it
/// removed (`running`): `(id, name)`.
fn running_of(r: &Value) -> Vec<(String, String)> {
    r["running"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let id = t["thread"].as_str()?;
            let name = t["thread_name"].as_str().unwrap_or(id);
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

/// Whether thread object `t` (`thread.get`, `thread.list`) says its runtime still runs on a
/// removed login in `dir`, the directory `rt`'s account `name` had (`runtime_account_removed`):
/// by the directory that runtime was started in when the hub says it, `dir` or one inside it,
/// whatever runtime and account it was started on (an earlier account there, of `rt` or
/// another runtime, removed with `--force` while its session lives, counts), else by the
/// runtime and the account's name.
fn still_on(t: &Value, rt: &str, name: &str, dir: &Path) -> bool {
    let on = &t["runtime_account_removed"];
    if !on.is_object() {
        return false;
    }
    match on["dir"].as_str().map(Path::new) {
        Some(d) => same_dir(d, dir) || within(&resolved_prefix(dir), d),
        None => on["runtime"] == rt && on["account"] == name,
    }
}

/// The login thread object `t`'s runtime still runs on (`runtime_account_removed`), as
/// `<runtime>:<account>`.
fn removed_login(t: &Value) -> String {
    let on = &t["runtime_account_removed"];
    format!("{}:{}", str_of(&on["runtime"]), str_of(&on["account"]))
}

/// Why a runtime of one of `threads` (`thread.list`) may still use `dir`, the directory `rt`'s
/// account `name` was removed with: it runs on a removed login in it ([`still_on`]), whatever
/// runtime; `None` when none does. Checked right before a logout or a purge: a runtime that
/// started or was found meanwhile.
fn live_session(threads: &Value, rt: &str, name: &str, dir: &Path) -> Option<String> {
    let t = threads
        .as_array()
        .into_iter()
        .flatten()
        .find(|t| still_on(t, rt, name, dir))?;
    let thread = t["name"].as_str().or(t["id"].as_str()).unwrap_or_default();
    Some(format!(
        "{thread} still runs on {} (removed), in {}",
        removed_login(t),
        tilde(dir)
    ))
}

/// Waits, at most `wait`, until none of `threads` (`(id, name)`) runs on the login removed
/// account `rt:name` had in `dir` any more ([`still_on`]: its runtime stopped, or a new
/// session started elsewhere; a deleted thread runs nowhere), asking `thread.get` every half
/// second, with a line for each one waited for (naming the login it runs on: another
/// runtime's in that directory too). Those that still do: `(name, <runtime>:<account>)`.
async fn wait_off_account(
    c: &mut twr_core::client::Client,
    rt: &str,
    name: &str,
    dir: &Path,
    threads: Vec<(String, String)>,
    wait: std::time::Duration,
) -> anyhow::Result<Vec<(String, String)>> {
    let deadline = tokio::time::Instant::now() + wait;
    let mut said = std::collections::HashSet::new();
    let mut left = threads;
    loop {
        let mut still = Vec::new();
        for (id, thread) in left {
            let t = match c.call("thread.get", json!({ "thread": id })).await {
                Ok(t) => t,
                Err(e) if super::is_not_found(&e) => continue,
                Err(e) => return Err(e),
            };
            if !still_on(&t, rt, name, dir) {
                continue;
            }
            let login = removed_login(&t);
            if said.insert(id.clone()) {
                match t["current_turn"].is_null() {
                    true => outln!("waiting for {thread}'s runtime on {login} to stop"),
                    false => outln!("waiting for {thread} to finish its turn on {login}"),
                }
            }
            still.push((id, thread, login));
        }
        let now = tokio::time::Instant::now();
        if still.is_empty() || now >= deadline {
            return Ok(still.into_iter().map(|(_, n, l)| (n, l)).collect());
        }
        let step = std::time::Duration::from_millis(500).min(deadline - now);
        tokio::time::sleep(step).await;
        left = still.into_iter().map(|(id, n, _)| (id, n)).collect();
    }
}

/// What to run by hand for `twr account remove`'s `--logout` (`logout`) and `--purge`
/// (`purge`) of `rt:name` in `dir` once nothing runs on it: the runtime's logout in the
/// account's environment as [`logout`] runs it (`env -u` each variable
/// `twr_adapters::account::account_env` removes, then the ones it sets), `rm -rf` of the
/// directory.
fn by_hand(rt: &str, name: &str, dir: &Path, logout: bool, purge: bool) -> String {
    let mut steps = Vec::new();
    if logout {
        if let Some(argv) = twr_adapters::account::logout_argv(rt) {
            let (set, remove) = twr_adapters::account::account_env(rt, dir).unwrap_or_default();
            let mut words = vec!["env".to_string()];
            words.extend(remove.iter().map(|k| format!("-u {k}")));
            words.extend(set.iter().map(|(k, v)| format!("{k}={}", sh_word(v))));
            words.push(program(rt).to_string());
            words.extend(argv.iter().map(|a| a.to_string()));
            steps.push(format!("log it out with `{}`", words.join(" ")));
        }
    }
    if purge {
        steps.push(format!(
            "delete its directory with `rm -rf {}`",
            sh_word(&dir.to_string_lossy())
        ));
    }
    format!("{} ({rt}:{name} is removed already)", steps.join(", then "))
}

/// Why `dir`, the directory removed account `rt:name` had, belongs to a live account of
/// `live` ([`live_dirs`]: `(label, directory)`) now, the same directory once resolved
/// ([`same_dir`]); `None` when to none.
fn live_owner(live: &[(String, PathBuf)], rt: &str, name: &str, dir: &Path) -> Option<String> {
    let (label, _) = live.iter().find(|(_, d)| same_dir(d, dir))?;
    Some(match *label == format!("{rt}:{name}") {
        true => format!("{rt}:{name} was added again in {}", tilde(dir)),
        false => format!("{} is {label}'s directory now", tilde(dir)),
    })
}

/// `s` as one shell word: as it is when it needs no quoting, else in single quotes.
fn sh_word(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+=:,@%".contains(c));
    match plain {
        true => s.to_string(),
        false => format!("'{}'", s.replace('\'', r"'\''")),
    }
}

/// The directories of the live accounts of an `account.list` answer (every runtime's named
/// accounts not removed, config's and twr's), but `except`'s (`(runtime, name)`): `(label,
/// directory)`.
fn live_dirs(list: &Value, except: Option<(&str, &str)>) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    for (rt, entry) in runtimes(list) {
        for a in accounts(entry) {
            let name = str_of(&a["name"]);
            if flag(a, "native") || flag(a, "removed") || except == Some((rt, name)) {
                continue;
            }
            if let Some(d) = a["dir"].as_str() {
                out.push((format!("{rt}:{name}"), PathBuf::from(d)));
            }
        }
    }
    out
}

/// `path` resolved as far as it exists: its longest existing ancestor resolved, the rest as
/// written; `path` itself (made absolute) when nothing of it resolves.
fn resolved_prefix(path: &Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut at = path.as_path();
    loop {
        if let Ok(real) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(real, |p, c| p.join(c));
        }
        match (at.parent(), at.file_name()) {
            (Some(parent), Some(c)) => {
                rest.push(c.to_owned());
                at = parent;
            }
            _ => return path,
        }
    }
}

/// Whether `path` is resolved directory `real` or inside it, as written or once resolved
/// ([`resolved_prefix`]).
fn within(real: &Path, path: &Path) -> bool {
    path.starts_with(real) || resolved_prefix(path).starts_with(real)
}

/// Whether `--purge` may delete `dir`, account directory of `runtime`: only a real directory
/// inside `<accounts root>/<runtime>/` (where `twr account add` makes them) once links are
/// followed (`twr_core::account::resolve_in_root`: not `<root>/<runtime>` linked elsewhere,
/// not `dir` a link), never one the user gave (`--dir`), never a runtime's own directory as
/// the hub has it (`natives`, [`native_here`]: the hub's `CODEX_HOME` pointing into the
/// accounts directory, say), and
/// never one that holds a runtime's own directory or a live account's (`live`, [`live_dirs`]:
/// `(label, directory)`). The resolved directory to delete; `None` when there is nothing there.
fn purgeable(
    root: &Path,
    runtime: &str,
    dir: &Path,
    natives: &Natives,
    live: &[(String, PathBuf)],
) -> anyhow::Result<Option<PathBuf>> {
    let parent = root.join(runtime);
    let refuse = || {
        anyhow!(
            "--purge deletes only account directories in {}, and {} is not one: delete it \
             yourself if you mean to",
            tilde(&parent),
            tilde(dir)
        )
    };
    if dir.parent() != Some(parent.as_path()) {
        return Err(refuse());
    }
    if dir.symlink_metadata().is_err() {
        return Ok(None);
    }
    let real =
        twr_core::account::resolve_in_root(root, runtime, dir, false).map_err(|_| refuse())?;
    if let Some(why) = native_here(&real, natives) {
        bail!("--purge does not delete {}: {why}", tilde(dir));
    }
    for rt in twr_core::account::ACCOUNT_RUNTIMES {
        let own = natives.get(rt);
        if let Some(n) = own.dir.as_deref().filter(|n| within(&real, n)) {
            bail!(
                "--purge does not delete {}: {rt}'s own directory in {} ({}) is in it",
                tilde(dir),
                own.whose(),
                tilde(n)
            );
        }
    }
    if let Some((label, d)) = live.iter().find(|(_, d)| within(&real, d)) {
        bail!(
            "--purge does not delete {}: {label}'s directory ({}) is in it",
            tilde(dir),
            tilde(d)
        );
    }
    match real.symlink_metadata() {
        Ok(m) if m.is_dir() => Ok(Some(real)),
        Ok(_) => Err(refuse()),
        Err(_) => Ok(None),
    }
}

/// Runs `runtime`'s logout with account `name`'s environment (`dir`), in this terminal. Why
/// it failed when it could not start or ended with a failure (the account is removed
/// already: [`remove`] says so).
async fn logout(runtime: &str, name: &str, dir: &Path) -> Result<(), String> {
    let Some(argv) = twr_adapters::account::logout_argv(runtime) else {
        return Ok(());
    };
    let cmd =
        twr_adapters::agents::agent_command(runtime, argv, &[], None, |k| std::env::var_os(k));
    let mut cmd = cmd.map_err(|e| format!("{e:#}"))?;
    let (set, remove) = twr_adapters::account::account_env(runtime, dir).unwrap_or_default();
    twr_adapters::process::apply_env_std(&mut cmd, set, &remove);
    outln!(
        "logging {runtime}:{name} out: {} {}",
        program(runtime),
        argv.join(" ")
    );
    match tokio::process::Command::from(cmd).status().await {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{}'s logout {}", program(runtime), ended(&s))),
        Err(e) => Err(format!("running {}'s logout: {e}", program(runtime))),
    }
}

/// How long [`refuse_other_workspaces`] waits for another workspace's hub to answer.
const OTHER_HUB_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Refuses `doing` (`--purge is refused`) on account directory `dir` while another workspace
/// of the root may use it: account directories are the root's ([`Paths::accounts_dir`]), so
/// every workspace's hub may have an account in `dir` (or in a directory inside it), or a
/// thread whose runtime runs on one. Each other workspace's hub is asked
/// ([`workspace_use`]); one that does not answer can't tell, so that refuses too. Nothing to
/// ask without other workspaces. (Not under a lock: a workspace may add an account in `dir`
/// between the question and the deletion.)
async fn refuse_other_workspaces(paths: &Paths, dir: &Path, doing: &str) -> anyhow::Result<()> {
    let root = paths.root();
    let here = paths.workspace_name();
    let mut names = vec![super::DEFAULT_WORKSPACE.to_string()];
    names.extend(
        root.named_workspaces()
            .with_context(|| format!("{doing}: listing {}", root.workspaces_dir().display()))?,
    );
    names.retain(|w| w != here);
    if names.is_empty() {
        return Ok(());
    }
    // The root's config: its `hub.socket` is the `default` workspace's socket.
    let config = twr_core::config::Config::load_from(&root).with_context(|| {
        format!("{doing}: loading the config to find the other workspaces' hubs")
    })?;
    for w in names {
        let socket = config.socket_path(&root.workspace(&w)?);
        if let Some(why) = workspace_use(&socket, &w, dir).await {
            bail!("{doing}: {why}");
        }
    }
    Ok(())
}

/// Why workspace `w`'s hub (on `socket`) may use account directory `dir`: an account of its
/// in it or inside it, not removed; a thread's runtime still on a removed login in it or
/// inside it (`runtime_account_removed`: by the directory that runtime was started in, its
/// `dir`, whatever directory the account's name leads to now; by the account's directory
/// when the hub does not say); or its hub does not answer (it isn't running, can't be
/// reached, serves another workspace), so it can't tell. `None` when it does not: threads
/// that keep a removed account with no runtime on it don't use its directory, as for this
/// workspace's own `--force --purge`.
async fn workspace_use(socket: &Path, w: &str, dir: &Path) -> Option<String> {
    let cant_tell = |why: String| {
        Some(format!(
            "{why}, so twr can't tell whether it uses {}: start it (`twr -w {w}`) and run this \
             again, or remove the account there",
            tilde(dir)
        ))
    };
    let mut c = match twr_core::client::Client::connect(socket).await {
        Ok(c) => c,
        Err(e) if super::nothing_listens(&e) => {
            return cant_tell(format!("workspace {w}'s hub isn't running"))
        }
        Err(e) => return cant_tell(format!("workspace {w}'s hub can't be reached ({e:#})")),
    };
    let asked = tokio::time::timeout(OTHER_HUB_WAIT, async {
        let hello = c.hello(json!({ "client": "cli" })).await?;
        if !super::hub_serves(&hello, w) {
            return anyhow::Ok(None);
        }
        let accounts = c.call("account.list", json!({})).await?;
        let threads = c.call("thread.list", Value::Null).await?;
        anyhow::Ok(Some((accounts, threads)))
    })
    .await;
    let (accounts, threads) = match asked {
        Ok(Ok(Some(answer))) => answer,
        Ok(Ok(None)) => {
            return cant_tell(format!(
                "the hub at {} serves another workspace than {w}",
                socket.display()
            ))
        }
        Ok(Err(e)) => return cant_tell(format!("workspace {w}'s hub failed to answer ({e:#})")),
        Err(_) => {
            return cant_tell(format!(
                "workspace {w}'s hub did not answer within {}s",
                OTHER_HUB_WAIT.as_secs()
            ))
        }
    };
    let real = resolved_prefix(dir);
    let on_removed: Vec<(&str, &str, &str, Option<&Path>)> = threads
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let on = &t["runtime_account_removed"];
            let thread = t["name"].as_str().or(t["id"].as_str())?;
            let ran_in = on["dir"].as_str().map(Path::new);
            Some((
                on["runtime"].as_str()?,
                on["account"].as_str()?,
                thread,
                ran_in,
            ))
        })
        .collect();
    // A runtime still in the directory its removed login had, whatever directory that
    // account's name leads to now (removed with --force and added again elsewhere while the
    // old session lives).
    for (rt, name, t, ran_in) in &on_removed {
        if let Some(d) = ran_in.filter(|d| same_dir(d, dir) || within(&real, d)) {
            return Some(format!(
                "workspace {w}'s thread {t} still runs on {rt}:{name} (removed), in {}",
                tilde(d)
            ));
        }
    }
    for (rt, entry) in runtimes(&accounts) {
        for a in self::accounts(entry) {
            let Some(d) = a["dir"].as_str().map(Path::new) else {
                continue;
            };
            if !(same_dir(d, dir) || within(&real, d)) {
                continue;
            }
            let name = str_of(&a["name"]);
            let label = format!("{rt}:{name}");
            if !flag(a, "removed") {
                return Some(format!(
                    "workspace {w} has account {label} in {} (remove it there first: \
                     `twr -w {w} account remove {rt} {name}`)",
                    tilde(d)
                ));
            }
            // Removed: only a runtime still on it uses the directory, as here (threads that
            // keep the account without a runtime don't). One whose directory is not known:
            // by the account's.
            let unknown = on_removed
                .iter()
                .find(|(r, n, _, ran_in)| *r == rt && *n == name && ran_in.is_none());
            if let Some((_, _, t, _)) = unknown {
                return Some(format!(
                    "workspace {w}'s thread {t} still runs on {label} (removed), in {}",
                    tilde(d)
                ));
            }
        }
    }
    None
}

/// `twr account name-native`.
async fn name_native(socket: &Path, runtime: &str, name: &str) -> anyhow::Result<()> {
    refuse_agent_action("name accounts")?;
    let mut c = connect(socket).await?;
    let r = c
        .call(
            "account.add",
            json!({ "runtime": runtime, "account": name, "native": true }),
        )
        .await?;
    if flag(&r, "config") {
        outln!("config.toml names {runtime}'s own login {name} already");
    } else if flag(&r, "recorded") {
        outln!(
            "{runtime}'s own login ({}) is now named {name}",
            tilde_native(&natives(&mut c).await?.get(runtime))
        );
    } else {
        outln!("{runtime}'s own login is named {name} already");
    }
    Ok(())
}

/// A thread's runtime as `twr thread list` and `twr model` write it (`thread` is a
/// `thread_json` object): `claude-code:work` while its runtime shows account labels, else
/// `claude-code`; an account that was removed always shows, `claude-code:work (removed)`.
pub(super) fn runtime_label(thread: &Value) -> String {
    let rt = str_of(&thread["runtime"]);
    let account = thread["account"].as_str().filter(|a| !a.is_empty());
    match account {
        Some(a) if flag(thread, "account_removed") => format!("{rt}:{a} (removed)"),
        Some(a) if flag(thread, "account_shown") => format!("{rt}:{a}"),
        _ => rt.to_string(),
    }
}

/// `twr info`'s lines for the accounts (`hub.info` `accounts`): one per runtime that shows
/// account labels, `claude-code 2 (personal, work)`.
pub(super) fn info_lines(accounts: &Value) -> Vec<String> {
    accounts
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, e)| flag(e, "shown"))
        .map(|(rt, e)| {
            let names: Vec<&str> = self::accounts(e)
                .into_iter()
                .filter(|a| counts(a))
                .map(|a| str_of(&a["name"]))
                .collect();
            format!("{rt} {} ({})", names.len(), names.join(", "))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, native: bool, login: Option<&str>) -> Value {
        json!({ "name": name, "native": native, "login": login, "source": "recorded",
                "removed": false, "threads": 0 })
    }

    /// A name is the account the hub resolves it to: never the native entry a live named
    /// account of that name shadows (`shadowed`), the native one otherwise.
    #[test]
    fn a_name_is_never_the_native_account_it_shadows() {
        let mut native = acct("default", true, None);
        native["shadowed"] = json!(true);
        let entry = json!({"accounts": [native, acct("default", false, None)]});
        assert_eq!(find(&entry, "default").unwrap()["native"], json!(false));
        let entry = json!({"accounts": [acct("default", true, None), acct("work", false, None)]});
        assert_eq!(find(&entry, "default").unwrap()["native"], json!(true));
        assert!(find(&entry, "other").is_none());
    }

    /// `account.remove`'s `running` and a thread object's `runtime_account_removed`: only a
    /// runtime on the removed account counts.
    #[test]
    fn removal_waits_on_the_runtimes_its_answer_names() {
        let r = json!({"running": [{"thread": "T1", "thread_name": "api"}, {"thread": "T2"}]});
        assert_eq!(
            running_of(&r),
            [("T1".into(), "api".into()), ("T2".into(), "T2".into())]
        );
        assert!(running_of(&json!({})).is_empty());
        let on =
            |rt: &str, a: &str| json!({"runtime_account_removed": {"runtime": rt, "account": a}});
        let d = Path::new("/acc/work");
        assert!(still_on(
            &on("claude-code", "work"),
            "claude-code",
            "work",
            d
        ));
        assert!(!still_on(
            &on("claude-code", "side"),
            "claude-code",
            "work",
            d
        ));
        assert!(!still_on(&on("codex", "work"), "claude-code", "work", d));
        assert!(!still_on(&json!({"id": "T1"}), "claude-code", "work", d));
        // With the directory the runtime was started in: by that directory, whatever account
        // it was started on.
        let in_dir = |a: &str, dir: &str| {
            json!({"runtime_account_removed":
                   {"runtime": "claude-code", "account": a, "dir": dir}})
        };
        assert!(still_on(
            &in_dir("old", "/acc/work"),
            "claude-code",
            "work",
            d
        ));
        assert!(!still_on(
            &in_dir("work", "/acc/elsewhere"),
            "claude-code",
            "work",
            d
        ));
        // ...whatever runtime it was started on (a Claude Code login removed with --force,
        // its directory then a Codex account's), and in a directory inside it.
        assert!(still_on(&in_dir("old", "/acc/work"), "codex", "x", d));
        assert!(still_on(
            &in_dir("old", "/acc/work/nested"),
            "codex",
            "x",
            d
        ));
        assert!(!still_on(&in_dir("old", "/acc/work2"), "codex", "x", d));
        assert!(!still_on(&in_dir("old", "/acc"), "codex", "x", d));
        // The final check before a logout or a purge: any thread of the list.
        let threads = json!([
            {"id": "T0", "name": "free"},
            {"id": "T1", "name": "w", "runtime_account_removed":
                {"runtime": "claude-code", "account": "old", "dir": "/acc/work"}},
        ]);
        assert_eq!(
            live_session(&threads, "codex", "x", d).as_deref(),
            Some("w still runs on claude-code:old (removed), in /acc/work")
        );
        assert_eq!(
            live_session(&threads, "codex", "x", Path::new("/acc/other")),
            None
        );
        assert_eq!(live_session(&json!([]), "codex", "x", d), None);
    }

    #[test]
    fn by_hand_names_the_logout_and_the_deletion() {
        let d = Path::new("/a/b c/work");
        assert_eq!(
            by_hand("codex", "work", d, true, true),
            "log it out with `env -u OPENAI_API_KEY -u CODEX_API_KEY -u CODEX_ACCESS_TOKEN \
             CODEX_HOME='/a/b c/work' codex logout`, then delete its directory with `rm -rf \
             '/a/b c/work'` (codex:work is removed already)"
        );
        // Every credential variable the account's environment removes is unset first, as the
        // logout twr runs has them unset (a stray one would reach another login).
        let claude = by_hand("claude-code", "work", Path::new("/a/w"), true, false);
        let (set, remove) =
            twr_adapters::account::account_env("claude-code", Path::new("/a/w")).unwrap();
        let unset: String = remove.iter().map(|k| format!("-u {k} ")).collect();
        let assign: String = set.iter().map(|(k, v)| format!("{k}={v} ")).collect();
        assert_eq!(
            claude,
            format!(
                "log it out with `env {unset}{assign}claude auth logout` (claude-code:work is \
                 removed already)"
            )
        );
        assert!(
            claude.contains("-u CLAUDE_SECURESTORAGE_CONFIG_DIR "),
            "{claude}"
        );
        assert!(
            claude.contains(" CLAUDE_CONFIG_DIR=/a/w ANTHROPIC_CONFIG_DIR=/a/w/anthropic claude "),
            "{claude}"
        );
        assert_eq!(
            by_hand("codex", "work", Path::new("/a/w"), false, true),
            "delete its directory with `rm -rf /a/w` (codex:work is removed already)"
        );
        assert_eq!(sh_word("it's"), r"'it'\''s'");
        assert_eq!(sh_word(""), "''");
    }

    /// The removed account's directory a live account has again, by resolved path: this
    /// account added back, or another.
    #[test]
    fn logout_refuses_a_directory_a_live_account_has() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir(&work).unwrap();
        let alias = tmp.path().join("alias");
        std::os::unix::fs::symlink(&work, &alias).unwrap();
        let live = |label: &str, d: &Path| vec![(label.to_string(), d.to_path_buf())];
        assert_eq!(live_owner(&[], "codex", "work", &work), None);
        assert_eq!(
            live_owner(
                &live("codex:other", &tmp.path().join("other")),
                "codex",
                "work",
                &work
            ),
            None
        );
        let again = live_owner(&live("codex:work", &alias), "codex", "work", &work).unwrap();
        assert!(
            again.starts_with("codex:work was added again in "),
            "{again}"
        );
        let other = live_owner(&live("claude-code:x", &work), "codex", "work", &alias).unwrap();
        assert!(
            other.ends_with(" is claude-code:x's directory now"),
            "{other}"
        );
    }

    /// `twr account login`'s note on the threads that reach the account by another path.
    #[test]
    fn login_notes_threads_on_a_retained_path() {
        assert_eq!(retained_note("claude-code", "work", &json!({})), None);
        assert_eq!(
            retained_note("claude-code", "work", &json!({"retained_paths": []})),
            None
        );
        let a = json!({"retained_paths": [{"thread": "api", "path": "/x/work"},
                                          {"thread": "web", "path": "/y/work"}]});
        assert_eq!(
            retained_note("claude-code", "work", &a).unwrap(),
            "note: api, web reach claude-code:work by another path, which this login may not \
             sign in:\n  api (/x/work): twr model api --account work moves it to the configured \
             path (a new session)\n  web (/y/work): twr model web --account work moves it to \
             the configured path (a new session)"
        );
    }

    #[test]
    fn aliases_name_runtimes() {
        assert_eq!(runtime_name("claude"), "claude-code");
        assert_eq!(runtime_name("grok"), "grok-build");
        assert_eq!(runtime_name("codex"), "codex");
        assert_eq!(runtime_name("acp"), "acp");
    }

    #[test]
    fn status_cells_say_the_last_check() {
        let mut a = acct("work", false, Some("logged_in"));
        a["method"] = json!("claude.ai");
        assert_eq!(status_cell(&a), "logged in (claude.ai)");
        assert_eq!(
            status_cell(&acct("default", true, None)),
            "never checked (native)"
        );
        assert_eq!(
            status_cell(&acct("w", false, Some("logged_out"))),
            "logged out"
        );
        assert_eq!(status_cell(&acct("w", false, Some("unknown"))), "unknown");
        assert_eq!(
            status_cell(&acct("w", false, Some("missing"))),
            "not installed"
        );
        let mut gone = acct("w", false, Some("missing"));
        gone["note"] = json!("its directory does not exist");
        assert_eq!(status_cell(&gone), "no directory");
        let mut removed = acct("w", false, Some("logged_in"));
        removed["removed"] = json!(true);
        assert_eq!(status_cell(&removed), "removed");
        assert!(!counts(&removed));
    }

    #[test]
    fn a_lone_unchecked_native_account_is_quiet() {
        let lone = json!({ "shown": false, "accounts": [acct("default", true, None)] });
        assert!(quiet(&lone));
        let checked =
            json!({ "shown": false, "accounts": [acct("default", true, Some("logged_in"))] });
        assert!(!quiet(&checked));
        let two = json!({ "shown": true,
            "accounts": [acct("default", true, None), acct("work", false, None)] });
        assert!(!quiet(&two));
        let t = accounts_table(&[("claude-code", &two)]);
        assert!(
            t.ends_with("claude-code shows account labels (2 accounts logged in)\n"),
            "{t}"
        );
        assert!(t.starts_with("RUNTIME      ACCOUNT  SOURCE  STATUS"), "{t}");
    }

    #[test]
    fn label_effects() {
        let two = json!({ "shown": true,
            "accounts": [acct("personal", true, Some("logged_in")), acct("work", false, None)] });
        assert_eq!(
            label_effect("claude-code", &two),
            "claude-code now has 2 logged-in accounts (personal, work), so its threads show \
             their account."
        );
        let one = json!({ "shown": false,
            "accounts": [acct("default", true, Some("logged_out")), acct("work", false, None)] });
        assert_eq!(
            label_effect("codex", &one),
            "codex has 1 logged-in account; labels stay hidden"
        );
    }

    #[test]
    fn runtime_labels_follow_the_label_rule() {
        let t = json!({ "runtime": "claude-code", "account": "work", "account_shown": false });
        assert_eq!(runtime_label(&t), "claude-code");
        let t = json!({ "runtime": "claude-code", "account": "work", "account_shown": true });
        assert_eq!(runtime_label(&t), "claude-code:work");
        let t = json!({ "runtime": "claude-code", "account": "work", "account_shown": false,
                        "account_removed": true });
        assert_eq!(runtime_label(&t), "claude-code:work (removed)");
        assert_eq!(runtime_label(&json!({ "runtime": "fake" })), "fake");
    }

    #[test]
    fn info_names_the_runtimes_that_show_labels() {
        let a = json!({
            "claude-code": { "shown": true, "accounts": [
                acct("personal", true, Some("logged_in")), acct("work", false, None),
                acct("old", false, Some("logged_out")) ] },
            "codex": { "shown": false, "accounts": [acct("default", true, None)] },
        });
        assert_eq!(info_lines(&a), ["claude-code 2 (personal, work)"]);
    }

    /// [`Natives`] a hub says: each runtime's directory `dir` gives, its selector unset.
    fn hub_natives(dir: impl Fn(&str) -> Option<PathBuf>) -> Natives {
        Natives(
            twr_core::account::ACCOUNT_RUNTIMES
                .iter()
                .map(|rt| {
                    let n = Native {
                        dir: dir(rt),
                        var: Some(HubVar::Unset),
                    };
                    (rt.to_string(), n)
                })
                .collect(),
        )
    }

    /// `account.list`'s `native_dir` and `native_var` read: the hub's directory and selector
    /// (set, unset, set to what it can't say); an entry without them (an older hub) is this
    /// environment's, the variable left alone.
    #[test]
    fn natives_are_the_hubs() {
        let entry = json!({
            "native_dir": "/hub/codex",
            "native_var": {"name": "CODEX_HOME", "set": true, "value": "/hub/codex"},
        });
        assert_eq!(
            Native::of("codex", &entry),
            Native {
                dir: Some("/hub/codex".into()),
                var: Some(HubVar::Set("/hub/codex".into())),
            }
        );
        let unset = json!({
            "native_dir": "/u/.codex",
            "native_var": {"name": "CODEX_HOME", "set": false},
        });
        assert_eq!(Native::of("codex", &unset).var, Some(HubVar::Unset));
        let unreadable = json!({
            "native_dir": null,
            "native_var": {"name": "CODEX_HOME", "set": true},
        });
        assert_eq!(
            Native::of("codex", &unreadable),
            Native {
                dir: None,
                var: Some(HubVar::Unreadable),
            }
        );
        let older = Native::of("codex", &json!({ "native": "default", "accounts": [] }));
        assert_eq!(
            older,
            Native {
                dir: native_dir("codex"),
                var: None,
            }
        );
        assert_eq!(older.whose(), "this environment");
        assert_eq!(Native::of("codex", &entry).whose(), "the hub's environment");
        // A whole answer: each runtime's, this environment's for one it lacks.
        let natives = Natives::of(&json!({ "runtimes": { "codex": entry } }));
        assert_eq!(natives.get("codex").dir, Some("/hub/codex".into()));
        assert_eq!(natives.get("claude-code").var, None);
    }

    /// The native account's login runs with the hub's selector: set to the hub's value, removed
    /// when the hub has it unset, untouched from an older hub, refused when it can't be said.
    #[test]
    fn the_native_login_gets_the_hubs_selector() {
        let n = |var| Native { dir: None, var };
        assert_eq!(
            native_env("codex", &n(Some(HubVar::Set("/hub/codex".into())))).unwrap(),
            (
                vec![("CODEX_HOME".to_string(), "/hub/codex".to_string())],
                vec![]
            )
        );
        assert_eq!(
            native_env("claude-code", &n(Some(HubVar::Set(String::new())))).unwrap(),
            (
                vec![("CLAUDE_CONFIG_DIR".to_string(), String::new())],
                vec![]
            )
        );
        assert_eq!(
            native_env("grok-build", &n(Some(HubVar::Unset))).unwrap(),
            (vec![], vec!["GROK_HOME".to_string()])
        );
        assert_eq!(native_env("codex", &n(None)).unwrap(), (vec![], vec![]));
        assert_eq!(
            native_env("fake", &n(Some(HubVar::Unset))).unwrap(),
            (vec![], vec![])
        );
        let e = native_env("codex", &n(Some(HubVar::Unreadable)))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("the hub's CODEX_HOME is set to what twr can't pass on"),
            "{e}"
        );
    }

    /// The note when this environment makes another directory a runtime's own than the hub:
    /// none when they agree, or from an older hub.
    #[test]
    fn the_follow_note_names_the_hubs_directory() {
        let here = native_dir("codex");
        let other = Native {
            dir: Some("/hub/codex".into()),
            var: Some(HubVar::Set("/hub/codex".into())),
        };
        let note = follow_note("codex", &other).unwrap();
        assert!(
            note.starts_with(
                "note: the hub's codex directory is /hub/codex (CODEX_HOME set there), not "
            ) && note.ends_with(" as here: twr follows the hub"),
            "{note}"
        );
        let same = Native {
            dir: here.clone(),
            var: Some(HubVar::Unset),
        };
        assert_eq!(follow_note("codex", &same), None);
        let older = Native {
            dir: Some("/elsewhere".into()),
            var: None,
        };
        assert_eq!(follow_note("codex", &older), None);
    }

    #[test]
    fn purge_stays_in_the_accounts_directory() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("accounts");
        let team = root.join("codex/team");
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&team).unwrap();
        std::fs::create_dir_all(elsewhere.join("team")).unwrap();
        std::fs::create_dir_all(root.join("claude-code/team")).unwrap();
        let none = &hub_natives(|_| None);
        assert_eq!(
            purgeable(&root, "codex", &team, none, &[]).unwrap(),
            Some(team.clone())
        );
        assert_eq!(
            purgeable(&root, "codex", &root.join("codex/gone"), none, &[]).unwrap(),
            None
        );
        // A runtime's own directory as the hub has it (its `CODEX_HOME` set to it, say):
        // never deleted.
        for rt in ["codex", "claude-code"] {
            let e = purgeable(
                &root,
                "codex",
                &team,
                &hub_natives(|r: &str| (r == rt).then(|| team.clone())),
                &[],
            )
            .unwrap_err()
            .to_string();
            assert!(
                e.contains(&format!("is {rt}'s own directory in the hub's environment")),
                "{e}"
            );
        }
        // Nor one that holds a runtime's own directory or a live account's, however named.
        std::fs::create_dir_all(team.join("nested")).unwrap();
        std::os::unix::fs::symlink(&team, base.join("alias")).unwrap();
        let e = purgeable(
            &root,
            "codex",
            &team,
            &hub_natives(|r: &str| (r == "grok-build").then(|| base.join("alias/nested/grok"))),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("grok-build's own directory in the hub's environment (")
                && e.ends_with("/alias/nested/grok) is in it"),
            "{e}"
        );
        let live = [("claude-code:inner".to_string(), base.join("alias/nested"))];
        let e = purgeable(&root, "codex", &team, none, &live)
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("claude-code:inner's directory (") && e.ends_with(") is in it"),
            "{e}"
        );
        let beside = [("codex:other".to_string(), root.join("codex/team-2"))];
        assert_eq!(
            purgeable(&root, "codex", &team, none, &beside).unwrap(),
            Some(team.clone())
        );
        std::fs::remove_file(base.join("alias")).unwrap();
        let refused = |dir: &Path| {
            let e = purgeable(&root, "codex", dir, none, &[])
                .unwrap_err()
                .to_string();
            assert!(
                e.contains("--purge deletes only account directories in"),
                "{dir:?}: {e}"
            );
        };
        for bad in [
            root.join("claude-code/team"),
            elsewhere.join("team"),
            root.join("codex"),
        ] {
            refused(&bad);
        }
        // A link as the account directory, or as `<root>/codex`: never followed.
        let link = root.join("codex/linked");
        symlink(elsewhere.join("team"), &link).unwrap();
        refused(&link);
        std::fs::remove_dir_all(root.join("codex")).unwrap();
        symlink(&elsewhere, root.join("codex")).unwrap();
        refused(&team);
        assert!(elsewhere.join("team").is_dir());
    }

    /// After a login the hub's state counts only while it is of what was logged in, checked
    /// since the login ended; else why (a change of the account) or nothing (no check since).
    #[test]
    fn after_a_login_the_hub_state_must_be_of_that_login() {
        let end: DateTime<Utc> = "2026-10-20T12:00:00Z".parse().unwrap();
        let dir = Path::new("/acct/work");
        let mut work = acct("work", false, Some("logged_in"));
        work["method"] = json!("claude.ai");
        work["dir"] = json!(dir);
        work["checked_at"] = json!("2026-10-20T12:00:01Z");
        assert_eq!(
            hub_state_of(Some(&work), Some(dir), Some(end)),
            Ok("logged in (claude.ai)".to_string())
        );
        assert_eq!(
            hub_state_of(Some(&work), Some(dir), None),
            Ok("logged in (claude.ai)".to_string())
        );
        // Checked before the login ended, or never: no state.
        let mut old = work.clone();
        old["checked_at"] = json!("2026-10-20T11:59:59Z");
        assert_eq!(hub_state_of(Some(&old), Some(dir), Some(end)), Err(None));
        let mut never = acct("work", false, None);
        never["dir"] = json!(dir);
        assert_eq!(hub_state_of(Some(&never), Some(dir), None), Err(None));
        // The account changed meanwhile.
        let mut moved = work.clone();
        moved["dir"] = json!("/elsewhere/work");
        assert_eq!(
            hub_state_of(Some(&moved), Some(dir), Some(end)),
            Err(Some("moved to another directory (/elsewhere/work)".into()))
        );
        let mut removed = work.clone();
        removed["removed"] = json!(true);
        assert_eq!(
            hub_state_of(Some(&removed), Some(dir), Some(end)),
            Err(Some("was removed".into()))
        );
        assert_eq!(
            hub_state_of(None, Some(dir), Some(end)),
            Err(Some("stopped being an account".into()))
        );
        let mut native = work.clone();
        native["native"] = json!(true);
        assert_eq!(
            hub_state_of(Some(&native), Some(dir), Some(end)),
            Err(Some("became the native account".into()))
        );
        assert_eq!(
            hub_state_of(Some(&native), None, Some(end)).as_deref(),
            Ok("logged in (claude.ai)")
        );
        assert_eq!(
            hub_state_of(Some(&work), None, Some(end)),
            Err(Some("stopped being the native account".into()))
        );
    }

    /// A logout or a purge acts only in the directory the hub removed the account with, when
    /// it is the one checked before.
    #[test]
    fn logout_and_purge_follow_the_removed_directory() {
        let dir = Path::new("/acct/claude-code/work");
        let r = json!({ "removed": true, "dir": "/acct/claude-code/work" });
        assert_eq!(removed_from(&r, "claude-code", "work", dir), Ok(()));
        let r = json!({ "removed": true, "dir": "/elsewhere/work" });
        assert_eq!(
            removed_from(&r, "claude-code", "work", dir),
            Err(
                "claude-code:work was in /elsewhere/work when the hub removed it, not in \
                 /acct/claude-code/work as before"
                    .into()
            )
        );
        for r in [
            json!({ "removed": true }),
            json!({ "removed": true, "dir": null }),
        ] {
            assert!(removed_from(&r, "claude-code", "work", dir)
                .unwrap_err()
                .starts_with("the hub did not say which directory claude-code:work"));
        }
    }

    /// A login's or logout's directory in the accounts directory is checked once resolved: a
    /// link on the way out of it is refused, and so is a directory other users can open where
    /// it leads.
    #[test]
    fn login_directories_are_checked_once_resolved() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let root = base.join("accounts");
        let work = root.join("codex/work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            checked_dir(&root, "codex", &work).unwrap(),
            Some(work.clone())
        );
        assert_eq!(
            checked_dir(&root, "codex", &root.join("codex/gone")).unwrap(),
            None
        );
        // `<root>/codex/out` a link elsewhere: refused, though what it leads to is private.
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&outside, root.join("codex/out")).unwrap();
        let e = checked_dir(&root, "codex", &root.join("codex/out"))
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a symlink"), "{e}");
        // A directory elsewhere (`--dir`) is checked where it is.
        assert_eq!(
            checked_dir(&root, "codex", &outside).unwrap(),
            Some(outside.clone())
        );
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o750)).unwrap();
        let e = checked_dir(&root, "codex", &outside)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is readable by others (mode 750)"), "{e}");
        // `<root>/codex` itself a link elsewhere: nothing in it is inside the root.
        std::fs::rename(root.join("codex"), base.join("moved")).unwrap();
        symlink(base.join("moved"), root.join("codex")).unwrap();
        let e = checked_dir(&root, "codex", &work).unwrap_err().to_string();
        assert!(e.contains("once links are followed"), "{e}");
    }

    #[test]
    fn status_notes_name_broken_links_and_stray_variables() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let native = home.join(".claude");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::write(native.join("settings.json"), "{}").unwrap();
        std::fs::write(native.join("CLAUDE.md"), "x").unwrap();
        let dir = home.join("acct");
        std::fs::create_dir_all(&dir).unwrap();
        twr_adapters::account::make_links("claude-code", &native, &dir, &["CLAUDE.md"]).unwrap();
        std::fs::write(dir.join("settings.json"), "{}").unwrap();
        let entry = json!({ "shown": true, "accounts": [
            acct("default", true, None),
            { "name": "work", "native": false, "dir": dir, "share": "config", "removed": false },
        ] });
        let notes = status_notes(
            &[("claude-code", &entry)],
            |k| (k == "ANTHROPIC_API_KEY").then(|| "sk".into()),
            &hub_natives(|_| Some(native.clone())),
        );
        assert_eq!(
            notes,
            [
                "work: settings.json is no longer linked (a copy of its own now)",
                "ANTHROPIC_API_KEY is set here; twr removes it for named accounts",
            ]
        );
        // A directory that is not the account's alone: why, and no link checks.
        let entry = json!({ "shown": true, "accounts": [
            { "name": "clash", "native": false, "dir": dir, "share": "config",
              "removed": false, "conflict": "its directory is codex:team's too" },
            { "name": "here", "native": false, "dir": native, "share": "config",
              "removed": false },
        ] });
        let notes = status_notes(
            &[("claude-code", &entry)],
            |_| None,
            &hub_natives(|rt| (rt == "codex").then(|| native.clone())),
        );
        assert_eq!(
            notes,
            [
                "clash: can't be used: its directory is codex:team's too".to_string(),
                format!(
                    "here: {} is codex's own directory in the hub's environment (CODEX_HOME, or \
                     its default)",
                    tilde(&native)
                ),
            ]
        );
    }

    /// What picks an account's credentials: the hub's refusal of settings shared with the
    /// native account, a native file left out for its credentials, an account's own file that
    /// sets some (allowed), and a key sign-in while the file is the native one.
    #[test]
    fn status_notes_name_the_settings_that_pick_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let native = tmp.path().join(".claude");
        std::fs::create_dir_all(&native).unwrap();
        let named = |name: &str, extra: Value| {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let mut a = json!({ "name": name, "native": false, "dir": dir, "share": "config",
                                "removed": false });
            a.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            (dir, a)
        };
        // The native settings set a key: one account without the file (left out at add),
        // one linked to it (the hub refuses it).
        std::fs::write(native.join("settings.json"), r#"{"apiKeyHelper": "/k"}"#).unwrap();
        let (_, out) = named("out", json!({}));
        let (linked_dir, linked) = named(
            "linked",
            json!({"shared_credentials": "its settings.json is the native account's"}),
        );
        std::os::unix::fs::symlink(
            native.join("settings.json"),
            linked_dir.join("settings.json"),
        )
        .unwrap();
        // Its own file with a helper: allowed, said.
        let (own_dir, own) = named("own", json!({}));
        std::fs::write(
            own_dir.join("settings.json"),
            r#"{"env": {"ANTHROPIC_AUTH_TOKEN": "t"}}"#,
        )
        .unwrap();
        let entry = json!({ "shown": true, "accounts": [
            acct("default", true, None), out, linked, own,
        ] });
        let notes = status_notes(
            &[("claude-code", &entry)],
            |_| None,
            &hub_natives(|_| Some(native.clone())),
        );
        assert_eq!(
            notes,
            [
                "linked: can't be used: its settings.json is the native account's",
                "out: settings.json is not linked: the native one sets apiKeyHelper, which would \
                 sign it in as the native account (copy it without them to share it)",
                "own: settings.json is no longer linked (a copy of its own now)",
                "own: its own settings.json sets env.ANTHROPIC_AUTH_TOKEN: the account signs in \
                 with that, not with its login",
            ]
        );
        // The native settings set nothing twr knows, but a linked account signs in with a
        // key: said.
        std::fs::write(native.join("settings.json"), "{}").unwrap();
        let (_, keyed) = named("linked", json!({"method": "api-key"}));
        let entry = json!({ "shown": true, "accounts": [acct("default", true, None), keyed] });
        let notes = status_notes(
            &[("claude-code", &entry)],
            |_| None,
            &hub_natives(|_| Some(native.clone())),
        );
        assert_eq!(
            notes,
            [
                "linked: signed in with api-key while its settings.json is the native one's: \
              check that file for what picks credentials, or give the account a copy of its own"
            ]
        );
    }
}

//! What each runtime that takes accounts needs for one ([`twr_core::account`]): the variable
//! that selects the account's config directory, the stray-credential variables removed when
//! a named account runs ([`account_env`]), the runtime's own directory ([`native_dir`],
//! [`own_native_dir`]), the
//! items a named account links to the native one ([`link_items`], [`make_links`],
//! [`check_links`]), the settings that select credentials, which keep a settings file from
//! being shared ([`credential_settings`], [`shared_credentials`]), and the login and logout
//! commands ([`login_argv`], [`logout_argv`]).
//!
//! Removal lists: every variable the runtime itself treats as selecting credentials, checked
//! against Claude Code 2.1.304, Codex 0.162.1 and grok 1.0.50 (2026-10-20). A stray
//! `ANTHROPIC_API_KEY` made a scratch Claude account report `authMethod: api_key`, a stray
//! `XAI_API_KEY` made grok say `You are using XAI_API_KEY.`; Codex's are removed as a
//! precaution (its app-server ignored them).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use twr_core::account::Share;

/// One runtime's account facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountSupport {
    /// The runtime (`claude-code`, `codex`, `grok-build`).
    pub runtime: &'static str,
    /// The variable that selects the config directory (`CLAUDE_CONFIG_DIR`, ...).
    pub var: &'static str,
    /// The variables a named account's process must not inherit.
    pub remove: &'static [&'static str],
}

/// Claude Code: `CLAUDE_CONFIG_DIR`.
const CLAUDE: AccountSupport = AccountSupport {
    runtime: crate::claude_code::NAME,
    var: "CLAUDE_CONFIG_DIR",
    remove: &[
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_REFRESH_TOKEN",
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "ANTHROPIC_PROFILE",
        "ANTHROPIC_IDENTITY_TOKEN",
        "ANTHROPIC_IDENTITY_TOKEN_FILE",
        "ANTHROPIC_FEDERATION_RULE_ID",
        "ANTHROPIC_ORGANIZATION_ID",
        "ANTHROPIC_SERVICE_ACCOUNT_ID",
        "ANTHROPIC_WORKSPACE_ID",
        "ANTHROPIC_SCOPE",
        "CLAUDE_CODE_FEDERATION_CACHE_DIR",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
    ],
};

/// Codex: `CODEX_HOME`.
const CODEX: AccountSupport = AccountSupport {
    runtime: crate::codex::NAME,
    var: "CODEX_HOME",
    remove: &["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"],
};

/// Grok Build: `GROK_HOME`.
const GROK: AccountSupport = AccountSupport {
    runtime: crate::grok_build::NAME,
    var: "GROK_HOME",
    remove: &["XAI_API_KEY", "GROK_AUTH_PROVIDER_COMMAND"],
};

/// Every runtime's facts.
const SUPPORT: &[AccountSupport] = &[CLAUDE, CODEX, GROK];

/// Claude Code's profile store root, set per account to `<dir>/anthropic`: `CLAUDE_CONFIG_DIR`
/// does not move it, and its Console and workload-identity profiles outrank a `/login` login.
pub const ANTHROPIC_CONFIG_DIR: &str = "ANTHROPIC_CONFIG_DIR";

/// `runtime`'s account facts; None for a runtime that takes no accounts.
pub fn support(runtime: &str) -> Option<&'static AccountSupport> {
    SUPPORT.iter().find(|s| s.runtime == runtime)
}

/// A named account's environment: the variables to set
/// ([`twr_core::adapter::SpawnConfig::env`]) and the ones to remove
/// ([`twr_core::adapter::SpawnConfig::env_remove`]).
pub type AccountEnv = (Vec<(String, String)>, Vec<String>);

/// The environment of a named account of `runtime` in `dir`: the variables to set (the
/// runtime's variable = `dir`; Claude Code also `ANTHROPIC_CONFIG_DIR = <dir>/anthropic`) and
/// the ones to remove (never one that is also set). None for a runtime that takes no accounts.
pub fn account_env(runtime: &str, dir: &Path) -> Option<AccountEnv> {
    let s = support(runtime)?;
    let shown = |p: &Path| p.to_string_lossy().into_owned();
    let mut set = vec![(s.var.to_string(), shown(dir))];
    if runtime == crate::claude_code::NAME {
        set.push((
            ANTHROPIC_CONFIG_DIR.to_string(),
            shown(&dir.join("anthropic")),
        ));
    }
    let remove = s
        .remove
        .iter()
        .filter(|r| !set.iter().any(|(k, _)| k == *r))
        .map(|r| r.to_string())
        .collect();
    Some((set, remove))
}

/// The directory `runtime` uses with no account variable set by twr: its variable as `var`
/// reads it when set and not empty (one the user exported before starting twr), else
/// `~/.claude`, `~/.codex`, `~/.grok` under `home`. None for a runtime that takes no accounts,
/// or with neither.
pub fn native_dir(
    runtime: &str,
    var: impl Fn(&str) -> Option<OsString>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let s = support(runtime)?;
    if let Some(v) = var(s.var).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(v));
    }
    let name = match runtime {
        crate::claude_code::NAME => ".claude",
        crate::codex::NAME => ".codex",
        _ => ".grok",
    };
    home.map(|h| h.join(name))
}

/// [`native_dir`] as this process's environment says: its variable, else under the user's
/// home ([`user_home`] with `$HOME` and the password database). What the hub's own
/// environment selects (`hub::accounts::own_native_dir`), next-message suggestions' Codex
/// home among them (`hub::suggest`), and what `twr account` falls back to.
pub fn own_native_dir(runtime: &str) -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k);
    native_dir(runtime, var, user_home(var, passwd_home).as_deref())
}

/// The user's home directory as Codex finds it: `$HOME` as `var` reads it when set and not
/// empty, else this account's home directory in the password database (`passwd`,
/// [`passwd_home`]), as Codex's `dirs::home_dir` falls back to it.
pub fn user_home(
    var: impl Fn(&str) -> Option<OsString>,
    passwd: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    var("HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(passwd)
}

/// This account's home directory in the password database (`getpwuid_r`).
pub fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = vec![0u8; 16 * 1024];
    // SAFETY: getpwuid_r writes the entry into `pwd` and its strings into `buf`, both ours and
    // as large as we say; `found` points at `pwd` or is null. Nothing is kept past `buf`.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let rc = libc::getpwuid_r(
            libc::geteuid(),
            &mut pwd,
            buf.as_mut_ptr().cast(),
            buf.len(),
            &mut found,
        );
        if rc != 0 || found.is_null() || pwd.pw_dir.is_null() {
            return None;
        }
        let dir = std::ffi::CStr::from_ptr(pwd.pw_dir).to_bytes();
        (!dir.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(dir)))
    }
}

/// Claude Code's items linked at [`Share::Config`].
const CLAUDE_CONFIG: &[&str] = &[
    "settings.json",
    "CLAUDE.md",
    "agents",
    "commands",
    "skills",
    "output-styles",
    "hooks",
    "plugins",
];

/// Codex's items linked at [`Share::Config`].
const CODEX_CONFIG: &[&str] = &[
    "config.toml",
    "AGENTS.md",
    "rules",
    "skills",
    "prompts",
    "hooks.json",
    "plugins",
];

/// Grok Build's items linked at [`Share::Config`].
const GROK_CONFIG: &[&str] = &["config.toml", "skills", "trusted_folders.toml"];

/// The items (file and directory names in the config directory) a named account of `runtime`
/// links to the native one at share level `share`: the user's settings and instructions at
/// `config`, never credentials or sessions; none at `none` or for another runtime.
pub fn link_items(runtime: &str, share: Share) -> &'static [&'static str] {
    match (share, runtime) {
        (Share::None, _) => &[],
        (Share::Config, crate::claude_code::NAME) => CLAUDE_CONFIG,
        (Share::Config, crate::codex::NAME) => CODEX_CONFIG,
        (Share::Config, crate::grok_build::NAME) => GROK_CONFIG,
        (Share::Config, _) => &[],
    }
}

/// The file of `runtime`'s config directory whose settings can select credentials (and
/// that [`Share::Config`] links): Claude Code's `settings.json`, Codex's and Grok Build's
/// `config.toml`. None for a runtime that takes no accounts.
pub fn settings_file(runtime: &str) -> Option<&'static str> {
    match runtime {
        crate::claude_code::NAME => Some("settings.json"),
        crate::codex::NAME | crate::grok_build::NAME => Some("config.toml"),
        _ => None,
    }
}

/// Claude Code's `settings.json` keys that select credentials: a helper that prints an API
/// key, and the Bedrock and Vertex credential commands. (`forceLoginMethod` and
/// `forceLoginOrgUUID` only restrict a login; `otelHeadersHelper` is telemetry's.)
const CLAUDE_SELECTORS: &[&str] = &[
    "apiKeyHelper",
    "awsAuthRefresh",
    "awsCredentialExport",
    "gcpAuthRefresh",
];

/// Codex's `[model_providers.<id>]` keys that select credentials: the variable holding the
/// API key, an inline bearer token, a command that mints one, AWS credentials.
/// (`preferred_auth_method` is gone in 0.162; `forced_login_method` and
/// `forced_chatgpt_workspace_id` only restrict a login; `cli_auth_credentials_store` says
/// where the account's own login is kept.)
const CODEX_PROVIDER_SELECTORS: &[&str] = &["env_key", "experimental_bearer_token", "auth", "aws"];

/// Grok Build's `[model.<id>]` keys that select credentials: an inline API key, the variable
/// holding one, a named `[auth_provider.<name>]` helper that mints one ("wins whenever
/// present" over the login).
const GROK_MODEL_SELECTORS: &[&str] = &["api_key", "env_key", "auth_provider"];

/// The settings of `runtime`'s [`settings_file`] at `path` that select credentials, as dotted
/// keys (`apiKeyHelper`, `env.ANTHROPIC_API_KEY`, `model_providers.corp.env_key`,
/// `auth.auth_provider_command`): shared with an account, they would sign it in with those
/// credentials instead of its own login, whatever twr removes from its environment.
///
/// - Claude Code (`settings.json`): [`CLAUDE_SELECTORS`], and `env` entries for every
///   variable a named account's environment sets or removes ([`account_env`]: the stray
///   credential variables, `CLAUDE_CONFIG_DIR`, `ANTHROPIC_CONFIG_DIR`), since Claude Code
///   applies `env` to itself.
/// - Codex (`config.toml`): [`CODEX_PROVIDER_SELECTORS`] of any `[model_providers.<id>]`.
/// - Grok Build (`config.toml`): `auth_provider_command` under `[auth]` or its alias
///   `[grok_com_config]`, and [`GROK_MODEL_SELECTORS`] of any `[model.<id>]`.
///
/// A key set to `null`, `""` or nothing counts as not set. Empty for a file that does not
/// exist or a runtime that takes no accounts; an error (why) for one that can't be read or
/// parsed, which twr can't check. Read without blocking and within
/// [`twr_core::fsread::SETTINGS_MAX`] ([`twr_core::fsread::read_settings`]: the hub reads
/// it under its core lock): a FIFO, a device, a directory or a larger file is one that can't
/// be read.
pub fn credential_settings(runtime: &str, path: &Path) -> Result<Vec<String>, String> {
    if settings_file(runtime).is_none() {
        return Ok(Vec::new());
    }
    match read_settings(path)? {
        Some(text) => settings_selectors(runtime, &text),
        None => Ok(Vec::new()),
    }
}

/// [`twr_core::fsread::read_settings`] of `path`, its error said as [`credential_settings`]
/// says it (`it can't be read (...)`).
fn read_settings(path: &Path) -> Result<Option<String>, String> {
    twr_core::fsread::read_settings(path).map_err(|e| format!("it can't be read ({e})"))
}

/// [`credential_settings`] of `runtime`'s settings file text `text` (a runtime that takes
/// accounts): an error (why) when it can't be parsed.
fn settings_selectors(runtime: &str, text: &str) -> Result<Vec<String>, String> {
    if runtime == crate::claude_code::NAME {
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("it can't be read as JSON ({e})"))?;
        return Ok(claude_selectors(&v));
    }
    let v: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("it can't be read as TOML ({})", e.message()))?;
    Ok(match runtime {
        crate::codex::NAME => tables_setting(&v, "model_providers", CODEX_PROVIDER_SELECTORS),
        _ => {
            let mut out: Vec<String> = ["auth", "grok_com_config"]
                .iter()
                .filter(|t| {
                    v.get(**t)
                        .and_then(|t| t.get("auth_provider_command"))
                        .is_some_and(toml_set)
                })
                .map(|t| format!("{t}.auth_provider_command"))
                .collect();
            out.extend(tables_setting(&v, "model", GROK_MODEL_SELECTORS));
            out
        }
    })
}

/// [`credential_settings`] of Claude Code's `settings.json` `v`.
fn claude_selectors(v: &serde_json::Value) -> Vec<String> {
    let set =
        |x: Option<&serde_json::Value>| x.is_some_and(|x| !x.is_null() && x.as_str() != Some(""));
    let mut out: Vec<String> = CLAUDE_SELECTORS
        .iter()
        .filter(|k| set(v.get(**k)))
        .map(|k| k.to_string())
        .collect();
    let env = v.get("env");
    let vars = CLAUDE
        .remove
        .iter()
        .copied()
        .chain([CLAUDE.var, ANTHROPIC_CONFIG_DIR]);
    out.extend(
        vars.filter(|k| set(env.and_then(|e| e.get(*k))))
            .map(|k| format!("env.{k}")),
    );
    out
}

/// Whether TOML value `v` sets something: not an empty string or list.
fn toml_set(v: &toml::Value) -> bool {
    match v {
        toml::Value::String(s) => !s.is_empty(),
        toml::Value::Array(a) => !a.is_empty(),
        _ => true,
    }
}

/// `<table>.<id>.<key>` for every key of `keys` set in a table `<table>.<id>` of `v`.
fn tables_setting(v: &toml::Table, table: &str, keys: &[&str]) -> Vec<String> {
    let Some(entries) = v.get(table).and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (id, entry) in entries {
        for k in keys {
            if entry.get(*k).is_some_and(toml_set) {
                out.push(format!("{table}.{id}.{k}"));
            }
        }
    }
    out
}

/// The [`settings_file`] of `runtime`'s account in `dir`, and why sharing it is refused, when
/// it is the native one of native directory `native` (the same file, through a link or
/// otherwise: compared as files, links followed): it sets credential settings
/// ([`LeftOut::Credentials`], [`credential_settings`]: the account would sign in with the
/// native account's credentials), or it can't be read ([`LeftOut::Unchecked`]: not a regular
/// file, larger than [`twr_core::fsread::SETTINGS_MAX`], unreadable; twr can't tell what the
/// runtime would read in it). None when either file is missing, the account's is its own, or
/// the native one sets none; one that is not JSON or TOML sets none here, as the runtime
/// reads no settings from it ([`make_links`] still leaves it out, unchecked).
pub fn shared_credentials(
    runtime: &str,
    dir: &Path,
    native: &Path,
) -> Option<(&'static str, LeftOut)> {
    use std::os::unix::fs::MetadataExt;
    let file = settings_file(runtime)?;
    let (at, want) = (dir.join(file), native.join(file));
    let (a, b) = (std::fs::metadata(&at).ok()?, std::fs::metadata(&want).ok()?);
    if (a.dev(), a.ino()) != (b.dev(), b.ino()) {
        return None;
    }
    let text = match read_settings(&want) {
        Ok(text) => text?,
        Err(why) => return Some((file, LeftOut::Unchecked(why))),
    };
    let keys = settings_selectors(runtime, &text).ok()?;
    (!keys.is_empty()).then_some((file, LeftOut::Credentials(keys)))
}

/// What [`make_links`] did, by item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// Linked to the native item now.
    pub linked: Vec<String>,
    /// Already in the account's directory (a file, a directory or any link): left alone.
    pub kept: Vec<String>,
    /// Not in the native directory: nothing to link.
    pub missing: Vec<String>,
    /// Not linked, and why: the native [`settings_file`] sets credential settings
    /// ([`LeftOut::Credentials`]) or can't be checked ([`LeftOut::Unchecked`]).
    pub left_out: Vec<(String, LeftOut)>,
}

/// Why [`make_links`] left a native item out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeftOut {
    /// It sets these credential settings ([`credential_settings`]).
    Credentials(Vec<String>),
    /// It can't be read or parsed (why), so twr can't check it.
    Unchecked(String),
}

/// Links each of `items` in account directory `dir` to the same item of native directory
/// `native` (a symbolic link to its absolute path), when the native one exists and `dir` has
/// nothing of that name, except `runtime`'s [`settings_file`] when the native one selects
/// credentials ([`credential_settings`]: every account sharing it would sign in with them) or
/// can't be checked: that one is left out ([`LinkReport::left_out`]). Never replaces anything
/// in `dir`, never writes a settings file, and reads no native item but that one. An error
/// when a link cannot be made (the links made before it stay).
pub fn make_links(
    runtime: &str,
    native: &Path,
    dir: &Path,
    items: &[&str],
) -> io::Result<LinkReport> {
    let native = std::path::absolute(native)?;
    let settings = settings_file(runtime);
    let mut report = LinkReport::default();
    for item in items {
        let target = native.join(item);
        let at = dir.join(item);
        if at.symlink_metadata().is_ok() {
            report.kept.push(item.to_string());
            continue;
        }
        if target.symlink_metadata().is_err() {
            report.missing.push(item.to_string());
            continue;
        }
        if settings == Some(*item) {
            let left_out = match credential_settings(runtime, &target) {
                Ok(keys) if keys.is_empty() => None,
                Ok(keys) => Some(LeftOut::Credentials(keys)),
                Err(why) => Some(LeftOut::Unchecked(why)),
            };
            if let Some(why) = left_out {
                report.left_out.push((item.to_string(), why));
                continue;
            }
        }
        std::os::unix::fs::symlink(&target, &at)?;
        report.linked.push(item.to_string());
    }
    Ok(report)
}

/// What an account directory has where [`make_links`] would link an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// A link to the native item.
    Linked,
    /// A regular file or directory: something (an editor saving by replacing the file, a
    /// manual `cp`) replaced the link, or it was never linked.
    Replaced,
    /// A link to something other than the native item.
    Elsewhere,
    /// Nothing.
    Absent,
}

/// The state of each of `items` in account directory `dir` against native directory `native`
/// (`twr account status`). A link that names the native item another way (through a link to
/// the native directory, `~/.claude` linked elsewhere, or a relative link) is still linked: the
/// two are compared once resolved.
pub fn check_links(native: &Path, dir: &Path, items: &[&str]) -> Vec<(String, LinkState)> {
    let native = std::path::absolute(native).unwrap_or_else(|_| native.to_path_buf());
    let resolved = |p: &Path| std::fs::canonicalize(p).ok();
    items
        .iter()
        .map(|item| {
            let at = dir.join(item);
            let want = native.join(item);
            let state = match at.symlink_metadata() {
                Err(_) => LinkState::Absent,
                Ok(m) if m.file_type().is_symlink() => match std::fs::read_link(&at) {
                    Ok(t) if t == want => LinkState::Linked,
                    Ok(_) if resolved(&at).is_some_and(|r| Some(r) == resolved(&want)) => {
                        LinkState::Linked
                    }
                    _ => LinkState::Elsewhere,
                },
                Ok(_) => LinkState::Replaced,
            };
            (item.to_string(), state)
        })
        .collect()
}

/// The arguments of `runtime`'s interactive login, run with the account's environment:
/// `claude auth login` (it takes no device flag: it already waits for a pasted code), `codex
/// login`, `grok login`, with `--device-auth` for the latter two when `device` (no browser
/// opens, over ssh). None for a runtime that takes no accounts.
pub fn login_argv(runtime: &str, device: bool) -> Option<Vec<&'static str>> {
    let mut argv = match runtime {
        crate::claude_code::NAME => return Some(vec!["auth", "login"]),
        crate::codex::NAME | crate::grok_build::NAME => vec!["login"],
        _ => return None,
    };
    if device {
        argv.push("--device-auth");
    }
    Some(argv)
}

/// The arguments of `runtime`'s logout: `claude auth logout`, `codex logout`, `grok logout`.
/// None for a runtime that takes no accounts.
pub fn logout_argv(runtime: &str) -> Option<&'static [&'static str]> {
    match runtime {
        crate::claude_code::NAME => Some(&["auth", "logout"]),
        crate::codex::NAME | crate::grok_build::NAME => Some(&["logout"]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn every_account_runtime_has_support() {
        for rt in twr_core::account::ACCOUNT_RUNTIMES {
            assert_eq!(support(rt).map(|s| s.runtime), Some(*rt));
        }
        assert!(support("acp").is_none());
        assert!(account_env("pty", Path::new("/a")).is_none());
    }

    #[test]
    fn account_env_per_runtime() {
        let dir = Path::new("/acct/work");
        let (set, remove) = account_env("claude-code", dir).unwrap();
        assert_eq!(
            set,
            [
                ("CLAUDE_CONFIG_DIR".to_string(), "/acct/work".to_string()),
                (
                    "ANTHROPIC_CONFIG_DIR".to_string(),
                    "/acct/work/anthropic".to_string()
                ),
            ]
        );
        assert_eq!(remove.len(), 16);
        for k in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_VERTEX",
        ] {
            assert!(remove.iter().any(|r| r == k), "{k}");
        }
        assert!(!remove.iter().any(|r| set.iter().any(|(k, _)| k == r)));

        let (set, remove) = account_env("codex", dir).unwrap();
        assert_eq!(set, [("CODEX_HOME".to_string(), "/acct/work".to_string())]);
        assert_eq!(
            strs(&remove),
            ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"]
        );

        let (set, remove) = account_env("grok-build", dir).unwrap();
        assert_eq!(set, [("GROK_HOME".to_string(), "/acct/work".to_string())]);
        assert_eq!(strs(&remove), ["XAI_API_KEY", "GROK_AUTH_PROVIDER_COMMAND"]);
    }

    #[test]
    fn native_dir_with_and_without_the_variable() {
        let home = Path::new("/home/u");
        let none = |_: &str| None;
        assert_eq!(
            native_dir("claude-code", none, Some(home)),
            Some(PathBuf::from("/home/u/.claude"))
        );
        assert_eq!(
            native_dir("codex", none, Some(home)),
            Some(PathBuf::from("/home/u/.codex"))
        );
        assert_eq!(
            native_dir("grok-build", none, Some(home)),
            Some(PathBuf::from("/home/u/.grok"))
        );
        assert_eq!(native_dir("codex", none, None), None);
        assert_eq!(native_dir("acp", none, Some(home)), None);
        let set = |k: &str| (k == "CODEX_HOME").then(|| OsString::from("/elsewhere/codex"));
        assert_eq!(
            native_dir("codex", set, Some(home)),
            Some(PathBuf::from("/elsewhere/codex"))
        );
        // Another runtime's variable does not count, nor an empty one.
        assert_eq!(
            native_dir("claude-code", set, Some(home)),
            Some(PathBuf::from("/home/u/.claude"))
        );
        let empty = |_: &str| Some(OsString::new());
        assert_eq!(
            native_dir("grok-build", empty, Some(home)),
            Some(PathBuf::from("/home/u/.grok"))
        );
    }

    #[test]
    fn the_home_is_home_else_the_password_database() {
        let acct = || Some(PathBuf::from("/home/acct"));
        let home = |v: &'static str| move |k: &str| (k == "HOME").then(|| OsString::from(v));
        assert_eq!(user_home(home("/h"), acct), Some(PathBuf::from("/h")));
        assert_eq!(user_home(home(""), acct), Some(PathBuf::from("/home/acct")));
        assert_eq!(
            user_home(|_: &str| None, acct),
            Some(PathBuf::from("/home/acct"))
        );
        assert_eq!(user_home(|_: &str| None, || None), None);
        // This account has a home in the password database.
        assert!(passwd_home().is_some_and(|h| h.is_absolute()));
    }

    #[test]
    fn link_sets() {
        assert!(link_items("claude-code", Share::None).is_empty());
        assert!(link_items("claude-code", Share::Config).contains(&"settings.json"));
        assert!(link_items("codex", Share::Config).contains(&"config.toml"));
        assert_eq!(
            link_items("grok-build", Share::Config),
            ["config.toml", "skills", "trusted_folders.toml"]
        );
        assert!(link_items("acp", Share::Config).is_empty());
        for rt in twr_core::account::ACCOUNT_RUNTIMES {
            for never in [".credentials.json", ".claude.json", "auth.json", "projects"] {
                assert!(
                    !link_items(rt, Share::Config).contains(&never),
                    "{rt} {never}"
                );
            }
        }
    }

    #[test]
    fn make_links_links_what_exists_and_keeps_the_rest() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("native");
        let dir = t.path().join("acct");
        std::fs::create_dir_all(native.join("skills/one")).unwrap();
        std::fs::write(native.join("settings.json"), "{}").unwrap();
        std::fs::write(native.join("CLAUDE.md"), "native").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "own").unwrap();
        let items = ["settings.json", "CLAUDE.md", "skills", "agents"];
        let r = make_links("claude-code", &native, &dir, &items).unwrap();
        assert_eq!(r.linked, ["settings.json", "skills"]);
        assert_eq!(r.kept, ["CLAUDE.md"]);
        assert_eq!(r.missing, ["agents"]);
        assert_eq!(
            std::fs::read_link(dir.join("settings.json")).unwrap(),
            native.join("settings.json")
        );
        assert!(dir.join("skills/one").is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.join("CLAUDE.md")).unwrap(),
            "own"
        );
        // Again: everything there is kept.
        let again = make_links("claude-code", &native, &dir, &items).unwrap();
        assert!(again.linked.is_empty());
        assert_eq!(again.kept, ["settings.json", "CLAUDE.md", "skills"]);
        assert_eq!(again.missing, ["agents"]);
    }

    /// Credential settings of each runtime's settings file: Claude Code's helpers and `env`
    /// entries for the variables a named account's environment sets or removes, Codex's
    /// model providers' credentials, Grok Build's external auth provider and models' keys.
    #[test]
    fn credential_settings_per_runtime() {
        let t = tempfile::tempdir().unwrap();
        let file = |name: &str, text: &str| {
            let p = t.path().join(name);
            std::fs::write(&p, text).unwrap();
            p
        };
        let claude = file(
            "settings.json",
            r#"{"apiKeyHelper": "~/bin/key", "awsAuthRefresh": "", "gcpAuthRefresh": null,
                "model": "opus", "forceLoginMethod": "claudeai",
                "env": {"ANTHROPIC_API_KEY": "sk-x", "ANTHROPIC_CONFIG_DIR": "/x",
                        "CLAUDE_CODE_USE_BEDROCK": "1", "DISABLE_TELEMETRY": "1"}}"#,
        );
        assert_eq!(
            credential_settings("claude-code", &claude).unwrap(),
            [
                "apiKeyHelper",
                "env.ANTHROPIC_API_KEY",
                "env.CLAUDE_CODE_USE_BEDROCK",
                "env.ANTHROPIC_CONFIG_DIR"
            ]
        );
        let plain = file("plain.json", r#"{"model": "opus", "env": {"FOO": "1"}}"#);
        assert!(credential_settings("claude-code", &plain)
            .unwrap()
            .is_empty());
        let codex = file(
            "codex.toml",
            "model = \"gpt-5\"\nmodel_provider = \"corp\"\n\
             [model_providers.corp]\nbase_url = \"https://x\"\nenv_key = \"CORP_KEY\"\n\
             [model_providers.local]\nbase_url = \"http://localhost\"\n\
             [model_providers.minted.auth]\ncommand = \"mint\"\n\
             [mcp_servers.s]\ncommand = \"s\"\nenv = { OPENAI_API_KEY = \"x\" }\n",
        );
        assert_eq!(
            credential_settings("codex", &codex).unwrap(),
            [
                "model_providers.corp.env_key",
                "model_providers.minted.auth"
            ]
        );
        let grok = file(
            "grok.toml",
            "[auth]\nauth_provider_command = \"/bin/mint\"\n\
             [grok_com_config]\nauth_provider_command = \"\"\n\
             [model.mine]\nbase_url = \"https://x\"\nenv_key = [\"MY_KEY\"]\n\
             [model.local]\nbase_url = \"http://localhost\"\nenv_key = []\n\
             [models]\ndefault = \"grok-4.6\"\n",
        );
        assert_eq!(
            credential_settings("grok-build", &grok).unwrap(),
            ["auth.auth_provider_command", "model.mine.env_key"]
        );
        // Grok Build's own config, as it is here: nothing.
        let plain = file(
            "plain.toml",
            "[ui]\nyolo = false\n[models]\ndefault = \"g\"\n",
        );
        assert!(credential_settings("grok-build", &plain)
            .unwrap()
            .is_empty());
        assert!(credential_settings("codex", &plain).unwrap().is_empty());
        // Missing: nothing; unreadable as its format: an error; no accounts: nothing.
        let gone = t.path().join("gone.json");
        assert!(credential_settings("claude-code", &gone)
            .unwrap()
            .is_empty());
        let bad = file("bad.json", "{");
        assert!(credential_settings("claude-code", &bad)
            .unwrap_err()
            .starts_with("it can't be read as JSON"));
        assert!(credential_settings("codex", &bad)
            .unwrap_err()
            .starts_with("it can't be read as TOML"));
        assert!(credential_settings("acp", &claude).unwrap().is_empty());
        assert_eq!(settings_file("claude-code"), Some("settings.json"));
        assert_eq!(settings_file("grok-build"), Some("config.toml"));
        assert_eq!(settings_file("pty"), None);
    }

    /// A native settings file that selects credentials (or can't be checked) is not linked,
    /// and says why; the rest is linked as before, and nothing is written into settings.
    #[test]
    fn make_links_leaves_out_settings_that_select_credentials() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("native");
        let dir = t.path().join("acct");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(native.join("CLAUDE.md"), "x").unwrap();
        let items = ["settings.json", "CLAUDE.md"];
        for (text, why) in [
            (
                r#"{"env": {"ANTHROPIC_API_KEY": "sk-native"}}"#,
                LeftOut::Credentials(vec!["env.ANTHROPIC_API_KEY".into()]),
            ),
            (
                r#"{"apiKeyHelper": "/bin/key"}"#,
                LeftOut::Credentials(vec!["apiKeyHelper".into()]),
            ),
        ] {
            std::fs::write(native.join("settings.json"), text).unwrap();
            let r = make_links("claude-code", &native, &dir, &items).unwrap();
            assert_eq!(r.left_out, [("settings.json".to_string(), why)]);
            assert!(dir.join("settings.json").symlink_metadata().is_err());
            assert_eq!(
                std::fs::read_to_string(native.join("settings.json")).unwrap(),
                text
            );
            std::fs::remove_file(dir.join("CLAUDE.md")).ok();
        }
        std::fs::write(native.join("settings.json"), "{ not json").unwrap();
        let r = make_links("claude-code", &native, &dir, &items).unwrap();
        assert!(
            matches!(&r.left_out[..], [(f, LeftOut::Unchecked(_))] if f == "settings.json"),
            "{r:?}"
        );
        // Grok Build's config.toml with an external auth provider.
        std::fs::write(
            native.join("config.toml"),
            "[auth]\nauth_provider_command = \"/bin/mint\"\n",
        )
        .unwrap();
        let r = make_links("grok-build", &native, &dir, &["config.toml"]).unwrap();
        assert_eq!(
            r.left_out,
            [(
                "config.toml".to_string(),
                LeftOut::Credentials(vec!["auth.auth_provider_command".into()])
            )]
        );
        // Clean: linked.
        std::fs::write(native.join("settings.json"), r#"{"model": "opus"}"#).unwrap();
        let r = make_links("claude-code", &native, &dir, &items).unwrap();
        assert_eq!(r.linked, ["settings.json"]);
        assert!(r.left_out.is_empty());
    }

    /// An account's settings file that is the native one (a link, or a link to where the
    /// native one leads) shares the native credential settings; its own file does not.
    #[test]
    fn shared_credentials_follow_the_link() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("native");
        let own = t.path().join("own");
        let linked = t.path().join("linked");
        for d in [&native, &own, &linked] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(
            native.join("settings.json"),
            r#"{"apiKeyHelper": "/bin/k"}"#,
        )
        .unwrap();
        std::fs::write(own.join("settings.json"), r#"{"apiKeyHelper": "/bin/own"}"#).unwrap();
        std::os::unix::fs::symlink(native.join("settings.json"), linked.join("settings.json"))
            .unwrap();
        assert_eq!(
            shared_credentials("claude-code", &linked, &native),
            Some((
                "settings.json",
                LeftOut::Credentials(vec!["apiKeyHelper".to_string()])
            ))
        );
        assert_eq!(shared_credentials("claude-code", &own, &native), None);
        assert_eq!(
            shared_credentials("claude-code", &t.path().join("x"), &native),
            None
        );
        std::fs::write(native.join("settings.json"), "{}").unwrap();
        assert_eq!(shared_credentials("claude-code", &linked, &native), None);
        // One that is not JSON sets nothing the runtime reads.
        std::fs::write(native.join("settings.json"), "{ not json").unwrap();
        assert_eq!(shared_credentials("claude-code", &linked, &native), None);
    }

    fn mkfifo(p: &Path) {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// A native settings file that is a FIFO (no writer: a blocking read would wait forever)
    /// or larger than the cap is not read: [`credential_settings`] says it can't be read at
    /// once, [`shared_credentials`] refuses an account linked to it, and [`make_links`] leaves
    /// it out, unchecked.
    #[test]
    fn settings_that_cant_be_read_safely_are_refused_without_blocking() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("native");
        let linked = t.path().join("linked");
        for d in [&native, &linked] {
            std::fs::create_dir_all(d).unwrap();
        }
        let settings = native.join("settings.json");
        std::os::unix::fs::symlink(&settings, linked.join("settings.json")).unwrap();
        let start = std::time::Instant::now();
        for (make, why) in [
            (
                &(|p: &Path| mkfifo(p)) as &dyn Fn(&Path),
                "it can't be read (it is a FIFO, not a regular file)",
            ),
            (
                &|p: &Path| {
                    std::fs::File::create(p)
                        .unwrap()
                        .set_len(twr_core::fsread::SETTINGS_MAX + 1)
                        .unwrap()
                },
                "it can't be read (it is larger than 1024 KiB)",
            ),
        ] {
            let _ = std::fs::remove_file(&settings);
            make(&settings);
            assert_eq!(
                credential_settings("claude-code", &settings),
                Err(why.to_string())
            );
            assert_eq!(
                shared_credentials("claude-code", &linked, &native),
                Some(("settings.json", LeftOut::Unchecked(why.to_string())))
            );
            let dir = t.path().join(format!("acct{}", start.elapsed().as_nanos()));
            std::fs::create_dir_all(&dir).unwrap();
            let r = make_links("claude-code", &native, &dir, &["settings.json"]).unwrap();
            assert_eq!(
                r.left_out,
                [(
                    "settings.json".to_string(),
                    LeftOut::Unchecked(why.to_string())
                )]
            );
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn check_links_states() {
        let t = tempfile::tempdir().unwrap();
        let native = t.path().join("native");
        let dir = t.path().join("acct");
        std::fs::create_dir_all(&native).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["settings.json", "CLAUDE.md", "hooks"] {
            std::fs::write(native.join(f), "{}").unwrap();
        }
        make_links("claude-code", &native, &dir, &["settings.json", "hooks"]).unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "copy").unwrap();
        std::fs::remove_file(dir.join("hooks")).unwrap();
        std::os::unix::fs::symlink(t.path(), dir.join("hooks")).unwrap();
        let states = check_links(
            &native,
            &dir,
            &["settings.json", "CLAUDE.md", "hooks", "agents"],
        );
        assert_eq!(
            states,
            [
                ("settings.json".to_string(), LinkState::Linked),
                ("CLAUDE.md".to_string(), LinkState::Replaced),
                ("hooks".to_string(), LinkState::Elsewhere),
                ("agents".to_string(), LinkState::Absent),
            ]
        );
    }

    /// Links that reach the native item another way (made while the native directory was
    /// named through a link, or relative) are linked; a dangling one is elsewhere.
    #[test]
    fn check_links_compares_resolved_paths() {
        let t = tempfile::tempdir().unwrap();
        let real = t.path().join("real-claude");
        let native = t.path().join("dot-claude");
        let dir = t.path().join("acct");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &native).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["settings.json", "CLAUDE.md", "hooks"] {
            std::fs::write(real.join(f), "x").unwrap();
        }
        // Through the real directory, relative, and to a native item that is gone.
        std::os::unix::fs::symlink(real.join("settings.json"), dir.join("settings.json")).unwrap();
        std::os::unix::fs::symlink("../real-claude/CLAUDE.md", dir.join("CLAUDE.md")).unwrap();
        std::os::unix::fs::symlink(real.join("agents"), dir.join("agents")).unwrap();
        std::os::unix::fs::symlink(t.path(), dir.join("hooks")).unwrap();
        let states = check_links(
            &native,
            &dir,
            &["settings.json", "CLAUDE.md", "agents", "hooks"],
        );
        assert_eq!(
            states,
            [
                ("settings.json".to_string(), LinkState::Linked),
                ("CLAUDE.md".to_string(), LinkState::Linked),
                ("agents".to_string(), LinkState::Elsewhere),
                ("hooks".to_string(), LinkState::Elsewhere),
            ]
        );
    }

    #[test]
    fn login_and_logout_commands() {
        assert_eq!(login_argv("claude-code", true), Some(vec!["auth", "login"]));
        assert_eq!(login_argv("codex", false), Some(vec!["login"]));
        assert_eq!(
            login_argv("codex", true),
            Some(vec!["login", "--device-auth"])
        );
        assert_eq!(
            login_argv("grok-build", true),
            Some(vec!["login", "--device-auth"])
        );
        assert_eq!(login_argv("acp", false), None);
        assert_eq!(logout_argv("claude-code"), Some(&["auth", "logout"][..]));
        assert_eq!(logout_argv("codex"), Some(&["logout"][..]));
        assert_eq!(logout_argv("grok-build"), Some(&["logout"][..]));
        assert_eq!(logout_argv("pty"), None);
    }
}

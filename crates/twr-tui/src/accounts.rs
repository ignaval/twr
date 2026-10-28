//! The hub's accounts as the TUI shows them (`account.list`): per runtime, whether its
//! threads show account labels, its native account's name, and its accounts with their last
//! login state. Fetched on connecting and again after each `account.*` control event
//! ([`crate::app::AppState::accounts_due`]), and after a failed fetch with a backoff
//! ([`crate::app::AppState::accounts_answered`]); empty for a hub older than accounts, which
//! then labels nothing.
//!
//! The label rule is the hub's (`AccountsView::shows_labels`, its `shown`): a runtime shows
//! labels while two or more of its accounts are registered and logged in. A thread on a
//! removed account says so whatever the rule ([`Accounts::label`]). The usage line's labels
//! follow the same rule ([`Accounts::usage_shown`]).

use std::collections::BTreeMap;

use serde_json::Value;

/// Every runtime's accounts, by runtime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Accounts {
    /// The runtimes `account.list` named.
    pub runtimes: BTreeMap<String, RuntimeAccounts>,
}

/// One runtime's accounts (`account.list`'s `runtimes.<runtime>`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeAccounts {
    /// Its threads show account labels (`shown`).
    pub shown: bool,
    /// Its native account's current name (`native`; `default` until named).
    pub native: String,
    /// Its accounts, removed ones included, as listed.
    pub accounts: Vec<Account>,
}

/// One account (`account.list`'s `runtimes.<runtime>.accounts[]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Account {
    /// Its name (the native account's current one for the native account).
    pub name: String,
    /// The runtime's native account.
    pub native: bool,
    /// Its last login state as the hub writes it (`logged_in`, `logged_out`, `missing`,
    /// `unknown`); `None`: never checked.
    pub login: Option<String>,
    /// How it is signed in (`claude.ai`, `api-key`, ...), when the check said.
    pub method: Option<String>,
    /// Why the state is what it is, when the check said.
    pub note: Option<String>,
    /// Removed (`twr account remove`): its threads cannot start until they move.
    pub removed: bool,
    /// The native account whose name a live named account has too (`shadowed`): the name is
    /// that one's ([`Account::answers_to`]).
    pub shadowed: bool,
}

impl Account {
    /// Whether name `name` stands for this account, as the hub resolves it
    /// (`twr_core::account::AccountInfo::answers_to`): its name, unless it is the native
    /// account a live named account of that name shadows.
    pub fn answers_to(&self, name: &str) -> bool {
        self.name == name && !self.shadowed
    }

    /// Its login as words: `logged in`, `logged out`, `not installed`, `no directory`,
    /// `unknown`; `None` when never checked (as `twr account list` says them).
    pub fn login_text(&self) -> Option<&'static str> {
        Some(match self.login.as_deref()? {
            "logged_in" => "logged in",
            "logged_out" => "logged out",
            "missing"
                if self
                    .note
                    .as_deref()
                    .is_some_and(|n| n.contains("directory")) =>
            {
                "no directory"
            }
            "missing" => "not installed",
            _ => "unknown",
        })
    }

    /// Whether a new session can be offered on it: not removed, and not known to be logged
    /// out or missing (never checked or `unknown`: it may be).
    pub fn usable(&self) -> bool {
        !self.removed
            && self
                .login
                .as_deref()
                .is_none_or(|l| l != "logged_out" && l != "missing")
    }

    /// What `/accounts` and `/account` say of it after its name: `removed`, else `native`
    /// (for the native account) and its login ([`Account::login_text`]), comma-separated;
    /// empty when there is nothing to say.
    pub fn state_text(&self) -> String {
        if self.removed {
            return "removed".into();
        }
        let mut parts = Vec::new();
        if self.native {
            parts.push("native");
        }
        parts.extend(self.login_text());
        parts.join(", ")
    }
}

impl Accounts {
    /// `account.list`'s answer `list` (`{runtimes: {rt: {shown, native, accounts}}}`); a
    /// failed call (null) or an older hub's answer gives none.
    pub fn from_list(list: &Value) -> Accounts {
        let text = |v: &Value| v.as_str().map(str::to_string);
        let runtimes = list["runtimes"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(rt, entry)| {
                let accounts = entry["accounts"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|a| {
                        Some(Account {
                            name: text(&a["name"])?,
                            native: a["native"].as_bool() == Some(true),
                            login: text(&a["login"]),
                            method: text(&a["method"]),
                            note: text(&a["note"]),
                            removed: a["removed"].as_bool() == Some(true),
                            shadowed: a["shadowed"].as_bool() == Some(true),
                        })
                    })
                    .collect();
                let entry = RuntimeAccounts {
                    shown: entry["shown"].as_bool() == Some(true),
                    native: text(&entry["native"]).unwrap_or_else(|| "default".into()),
                    accounts,
                };
                (rt.clone(), entry)
            })
            .collect();
        Accounts { runtimes }
    }

    /// `runtime`'s accounts, when the hub listed it.
    pub fn runtime(&self, runtime: &str) -> Option<&RuntimeAccounts> {
        self.runtimes.get(runtime)
    }

    /// The account a thread on `runtime` and `account` (`None`: the native one) runs on, as
    /// the UI names it: its name while `runtime` shows labels, `<name> (removed)` for a
    /// removed account whatever the rule (one the list no longer has at all, as the hub's
    /// `account_removed` says: config.toml dropped it, say); `None` otherwise (and for a
    /// runtime the hub did not list).
    pub fn label(&self, runtime: &str, account: Option<&str>) -> Option<String> {
        self.label_on(runtime, account, false)
    }

    /// [`Accounts::label`] of a thread on another box than the hub's own when `remote`: its
    /// native account is that box's own login, never the removed one of the list (the hub's).
    pub fn label_on(&self, runtime: &str, account: Option<&str>, remote: bool) -> Option<String> {
        let rt = self.runtime(runtime)?;
        let name = account.unwrap_or(&rt.native);
        let removed = match account {
            None => !remote && rt.accounts.iter().any(|a| a.native && a.removed),
            Some(n) => {
                !rt.accounts.is_empty()
                    && !rt.accounts.iter().any(|a| a.answers_to(n) && !a.removed)
            }
        };
        if removed {
            Some(format!("{name} (removed)"))
        } else if rt.shown {
            Some(name.to_string())
        } else {
            None
        }
    }

    /// What the usage line shows of `usage` at `now`: its entries name their account by the
    /// hub's rule, as threads' labels do (`twr_core::usage::UsageView::shown_by`: while the
    /// runtime's `shown`, the native account under its `native` name). Nothing listed (a hub
    /// older than accounts, or before the first answer): the old rule
    /// (`UsageView::shown`: while a runtime has entries for more than one account).
    pub fn usage_shown(
        &self,
        usage: &twr_core::usage::UsageView,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<twr_core::usage::Shown> {
        if self.runtimes.is_empty() {
            return usage.shown(now);
        }
        let shown = usage.shown_by(now, |runtime| {
            self.runtime(runtime)
                .filter(|r| r.shown)
                .map(|r| r.native.clone())
        });
        twr_core::usage::one_unlabeled(shown, |runtime, account| {
            self.account(runtime, account).is_some_and(Account::usable)
        })
    }

    /// `runtime`'s account `account` (`None`: the native one), when listed (by name, the one
    /// that is not removed first: a removed native account keeps its name; never the native
    /// account a live named one of that name shadows, [`Account::answers_to`]).
    pub fn account(&self, runtime: &str, account: Option<&str>) -> Option<&Account> {
        let rt = self.runtime(runtime)?;
        match account {
            None => rt.accounts.iter().find(|a| a.native),
            Some(n) => rt
                .accounts
                .iter()
                .find(|a| a.answers_to(n) && !a.removed)
                .or_else(|| rt.accounts.iter().find(|a| a.answers_to(n))),
        }
    }

    /// What `/accounts` says: each runtime's accounts, with what [`Account::state_text`] says
    /// of each, a runtime whose only account is its native one by its name alone
    /// (`claude-code: personal (native, logged in), work (logged in) · codex: default`).
    pub fn summary(&self) -> String {
        let runtimes: Vec<String> = self
            .runtimes
            .iter()
            .map(|(rt, entry)| {
                let alone = matches!(entry.accounts.as_slice(), [a] if a.native && !a.removed)
                    || entry.accounts.is_empty();
                let names: Vec<String> = if alone {
                    vec![entry.native.clone()]
                } else {
                    entry
                        .accounts
                        .iter()
                        .map(|a| match a.state_text() {
                            s if s.is_empty() => a.name.clone(),
                            s => format!("{} ({s})", a.name),
                        })
                        .collect()
                };
                format!("{rt}: {}", names.join(", "))
            })
            .collect();
        runtimes.join(" · ")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// An `account.list` answer: claude-code with `personal` (native) and `work`, logged in
    /// when `both`, else `work` logged out; codex with its native account, never checked.
    pub(crate) fn list(both: bool) -> Value {
        json!({"runtimes": {
            "claude-code": {"shown": both, "native": "personal", "accounts": [
                {"name": "personal", "native": true, "login": "logged_in", "removed": false,
                 "threads": 1},
                {"name": "work", "native": false,
                 "login": if both { "logged_in" } else { "logged_out" }, "removed": false,
                 "threads": 2},
            ]},
            "codex": {"shown": false, "native": "default", "accounts": [
                {"name": "default", "native": true, "login": null, "removed": false},
            ]},
        }})
    }

    /// The native account fell back to `default` next to a named `default` (`shadowed`):
    /// the name is the named account's, as the hub resolves it.
    #[test]
    fn a_name_is_never_the_native_account_it_shadows() {
        let l = json!({"runtimes": {"codex": {"shown": true, "native": "default", "accounts": [
            {"name": "default", "native": true, "shadowed": true, "login": "logged_in"},
            {"name": "default", "native": false, "login": "logged_out"},
            {"name": "personal", "native": false, "login": "logged_in"},
        ]}}});
        let a = Accounts::from_list(&l);
        let named = a.account("codex", Some("default")).unwrap();
        assert!(!named.native && named.answers_to("default"));
        assert_eq!(named.login.as_deref(), Some("logged_out"));
        assert!(a.account("codex", None).unwrap().shadowed);
        // Removed, the named one is still what the name stands for (removed): never the
        // native account in its place.
        let mut l = l;
        l["runtimes"]["codex"]["accounts"][1]["removed"] = json!(true);
        let a = Accounts::from_list(&l);
        assert!(!a.account("codex", Some("default")).unwrap().native);
        assert_eq!(
            a.label("codex", Some("default")).as_deref(),
            Some("default (removed)")
        );
    }

    /// Labels follow the hub's `shown`, the native account under its name; a removed account
    /// is named whatever the rule; an older hub's (null) answer labels nothing.
    #[test]
    fn labels_follow_shown_and_name_removed_accounts() {
        let a = Accounts::from_list(&list(true));
        assert_eq!(
            a.label("claude-code", Some("work")).as_deref(),
            Some("work")
        );
        assert_eq!(a.label("claude-code", None).as_deref(), Some("personal"));
        assert_eq!(a.label("codex", None), None);
        assert_eq!(a.label("pty", None), None);
        let a = Accounts::from_list(&list(false));
        assert_eq!(a.label("claude-code", Some("work")), None);
        let mut l = list(false);
        l["runtimes"]["claude-code"]["accounts"][1]["removed"] = json!(true);
        let a = Accounts::from_list(&l);
        assert_eq!(
            a.label("claude-code", Some("work")).as_deref(),
            Some("work (removed)")
        );
        assert_eq!(a.label("claude-code", None), None);
        // One the list no longer has (config.toml dropped it, no record): removed too.
        assert_eq!(
            a.label("claude-code", Some("gone")).as_deref(),
            Some("gone (removed)")
        );
        assert_eq!(
            a.label("codex", Some("gone")).as_deref(),
            Some("gone (removed)")
        );
        // The removed native account keeps its name; a named account with the default name
        // is not it.
        let l = json!({"runtimes": {"codex": {"shown": false, "native": "mine", "accounts": [
            {"name": "mine", "native": true, "removed": true},
            {"name": "default", "native": false, "removed": false},
        ]}}});
        let a = Accounts::from_list(&l);
        assert_eq!(a.label("codex", None).as_deref(), Some("mine (removed)"));
        // On another box the native account is the box's own login.
        assert_eq!(a.label_on("codex", None, true), None);
        assert_eq!(a.label("codex", Some("default")), None);
        assert_eq!(Accounts::from_list(&Value::Null), Accounts::default());
        assert_eq!(
            Accounts::from_list(&Value::Null).label("claude-code", None),
            None
        );
    }

    #[test]
    fn the_summary_names_each_runtimes_accounts() {
        assert_eq!(
            Accounts::from_list(&list(true)).summary(),
            "claude-code: personal (native, logged in), work (logged in) · codex: default"
        );
        let mut l = list(false);
        l["runtimes"]["claude-code"]["accounts"][1]["removed"] = json!(true);
        assert_eq!(
            Accounts::from_list(&l).summary(),
            "claude-code: personal (native, logged in), work (removed) · codex: default"
        );
    }
}

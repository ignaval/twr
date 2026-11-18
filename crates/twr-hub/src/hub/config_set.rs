//! `config.set {key, value}`: one key of twr's config file (`config.toml`, the hub's
//! [`super::HubOptions::config_file`]) set through the hub, so that the orchestrator can change
//! twr's configuration when the user asks it to without any access to the file itself
//! (`docs/orchestrator.md`, "Changing twr's configuration").
//!
//! `key` is a dotted path, `tui.suggest`, with a part that holds anything but ASCII letters,
//! digits, `_` and `-` quoted (`projects."my.api".codex_auto`, `\"` and `\\` inside);
//! `value` a JSON boolean, number or string. Lists and tables are refused, and so is a key
//! whose value in the file is one. The new text is computed with
//! [`config_edit::set_key`] first, which changes only that key's value (every other byte of the
//! file, comments included, stays) and checks that twr still loads the result: a key twr does
//! not know, a value of the wrong type or one twr refuses is `bad_request` with the reason,
//! before anything is recorded; so is a value the file has already. The file is written over
//! exactly the text that was read ([`config_edit::write_text`]: locked, in place, checked with
//! `boxes.toml` beside it), and never `boxes.toml`.
//!
//! Who may change what ([`classify`]):
//!
//! - **Refused** for every caller ([`refusal`]): `boxes.*` (they live in `boxes.toml`, with
//!   tokens, ssh destinations and commands), `hub.tls`, every `env` entry (`threads.env.*`,
//!   `projects.*.env.*`), the command lines (`notify.command`, `speech.command`,
//!   `voice.command`, `runtimes.*.acp`, `runtimes.*.attach`), a runtime given as a command line
//!   (`acp:...`, `pty:...`) for `default_runtime`, `orchestrator.runtime` or
//!   `projects.*.runtime`, and any key or value holding a private key or a token of a known
//!   format (`project_read::secret_marker`). The message says what the user does instead.
//! - **Direct** ([`DIRECT`]): harmless display and voice preferences, applied at once, also
//!   when the orchestrator asks (like `set_model`).
//! - **Card**: every other key, a key added to the config later included. The user's call
//!   applies it at once; the orchestrator's goes on the confirm card (`hub::proposal`,
//!   `Proposed::Config`) with what the file had (`before`), and only the user's Confirm writes
//!   it, after checking that the file still has that value.
//!
//! Callers: the user applies directly; a paired device needs `admin` (as `desktop.config.set`
//! does: configuration can name commands the hub runs); an agent can't call it
//! (`server::AGENT_METHODS`). The orchestrator, for a direct key and a card key alike, only in
//! a turn the user's message started, that message being the user's latest and not taken back,
//! and not a message from a device without `admin` ([`Core::config_check`]): a hub notice, a
//! watch or a worker's report never changes the configuration (`relay_forbidden`). A card key
//! goes on the card once per turn.
//!
//! Every change is recorded as `config.changed {key, value, before, by, effect}` on the
//! orchestrator's thread (when there is one), which the TUI, `twr` and transcripts show as a
//! line. `effect` says when the change takes effect ([`Effect`]): nothing reloads the file while
//! it runs, so a key the hub reads applies when the hub restarts, a `[tui]` key when a TUI
//! starts, and so on.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::config_edit::{self, Current, Scalar};
use twr_core::event::{ConfigChanged, EventBody, Source};
use twr_core::proto::{codes, RpcError};
use twr_decisions::code::card::CardValue;

use super::project_read::secret_marker;
use super::proposal::Proposed;
use super::withdraw::OrchCall;
use super::{internal, rpc, Caller, Core, Hub, Inner};

/// When a change to a key takes effect, as twr reads its config: each program reads it when it
/// starts, and nothing reloads it while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Effect {
    /// `[tui]`: each TUI reads it when it starts; one already open keeps what it read.
    TuiStart,
    /// `[speech]`: the speaker (`twr speak`, or a TUI's own) reads it when it starts.
    SpeakerStart,
    /// `[speech] voice` and `rate`: as [`Effect::SpeakerStart`], but a voice or rate chosen
    /// with `/read voice` or `/read rate`, kept on the speaker's machine, comes first.
    SpeakerPref,
    /// `[voice]`'s recognizer settings: `twr voice transcribe` reads them for each recording.
    NextRecording,
    /// `[voice]`'s listening settings: the desktop app reads them (`twr voice status`) when
    /// voice mode starts.
    VoiceStart,
    /// `[orchestrator] runtime`, `model`, `effort`, `account`: they pick the first
    /// orchestrator only; the current one keeps its own.
    FirstOrchestrator,
    /// `[orchestrator] access`: now, the orchestrator restarting with it after its running
    /// turn (`hub::orch_access`).
    OrchestratorAccess,
    /// Everything else: the hub reads it when it starts.
    HubRestart,
}

impl Effect {
    /// The words `config.set` and `config.changed` use.
    pub(super) fn text(self) -> &'static str {
        match self {
            Effect::TuiStart => "the next time a TUI starts (one already open keeps what it read)",
            Effect::SpeakerStart => {
                "the next time read-aloud starts (`twr speak`, or a TUI that reads aloud)"
            }
            Effect::SpeakerPref => {
                "the next time read-aloud starts (`twr speak`, or a TUI that reads aloud); a \
                 voice or rate chosen with /read voice or /read rate stays ahead of it"
            }
            Effect::NextRecording => "with the next recording voice mode recognizes",
            Effect::VoiceStart => "the next time voice mode starts in the desktop app",
            Effect::FirstOrchestrator => {
                "only for a first orchestrator, when the hub makes one: the current orchestrator \
                 keeps its own (`twr orchestrator use` switches it)"
            }
            Effect::OrchestratorAccess => {
                "now: the orchestrator restarts with it (after its current turn, if one is running)"
            }
            Effect::HubRestart => "when the hub restarts (`twr hub stop`, then `twr`)",
        }
    }

    /// When a change of `key` (its parts) takes effect.
    fn of(key: &[&str]) -> Effect {
        match key {
            ["tui", ..] => Effect::TuiStart,
            ["speech", "voice" | "rate"] => Effect::SpeakerPref,
            ["speech", ..] => Effect::SpeakerStart,
            ["voice", "pause_ms" | "listen_while_speaking"] => Effect::VoiceStart,
            ["voice", ..] => Effect::NextRecording,
            ["orchestrator", "runtime" | "model" | "effort" | "account"] => {
                Effect::FirstOrchestrator
            }
            ["orchestrator", "access"] => Effect::OrchestratorAccess,
            _ => Effect::HubRestart,
        }
    }
}

/// The keys the orchestrator changes at once, in a turn of the user's message that asks for it
/// (like `set_model`): display and voice preferences that change what the user sees or hears,
/// never what an agent may do, and never a command, a path twr runs or a secret. Only these:
/// every other key goes on the confirm card, a key added to twr later included. When each one
/// takes effect: [`Effect::of`].
pub(super) const DIRECT: &[&str] = &[
    "tui.mouse",
    "tui.animate",
    "tui.card_order",
    "tui.compact_width",
    "tui.suggest",
    "tui.summaries",
    "tui.summary_every",
    "tui.colors",
    "tui.notify.terminal",
    "tui.notify.when",
    "speech.voice",
    "speech.rate",
    "speech.volume",
    "speech.turn_from",
    "speech.code",
    "speech.paths",
    "speech.catchup_secs",
    "voice.model",
    "voice.language",
    "voice.pause_ms",
    "voice.timeout_secs",
    "voice.listen_while_speaking",
    "threads.thinking",
];

/// What `config.set` does with a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Class {
    /// Applied at once, whoever asks.
    Direct,
    /// Applied at once for the user; the orchestrator's goes on the confirm card.
    Card,
    /// Never changed through the hub: why, and what the user does instead.
    Refused(String),
}

/// Why `key` (its parts) may not be set to `value` through the hub, if it may not ([`Class`]).
pub(super) fn refusal(key: &[String], value: &Scalar) -> Option<String> {
    const BY_HAND: &str = "the user edits config.toml by hand";
    let parts: Vec<&str> = key.iter().map(String::as_str).collect();
    let why = match parts.as_slice() {
        ["boxes", ..] => Some(
            "boxes hold runner tokens, ssh destinations and commands, and live in boxes.toml: \
             the user changes them with `twr box` (add, token, remove) or edits the file"
                .to_string(),
        ),
        ["hub", "tls", ..] => Some(format!("the hub's TLS certificate and key: {BY_HAND}")),
        ["threads", "env", ..] | ["projects", _, "env", ..] => Some(format!(
            "env values are passed to agents and often hold API keys: {BY_HAND}"
        )),
        ["notify" | "speech" | "voice", "command", ..] | ["runtimes", _, "acp" | "attach", ..] => {
            Some(format!("a command line twr runs: {BY_HAND}"))
        }
        _ => None,
    };
    if why.is_some() {
        return why;
    }
    let runtime_key = matches!(
        parts.as_slice(),
        ["default_runtime"] | ["orchestrator", "runtime"] | ["projects", _, "runtime"]
    );
    if let (true, Scalar::Str(s)) = (runtime_key, value) {
        if twr_core::devices::is_command_runtime(s) {
            return Some(format!(
                "a runtime given as a command line (acp:..., pty:...) runs that command: \
                 {BY_HAND}, or names claude-code, codex, grok-build or a [runtimes.<name>] preset"
            ));
        }
    }
    let value_text = match value {
        Scalar::Str(s) => Some(s.as_str()),
        _ => None,
    };
    for text in parts.iter().copied().chain(value_text) {
        if let Some(what) = secret_marker(text.as_bytes()) {
            return Some(format!(
                "it looks like it holds {what}: a secret typed into your chat stays in your \
                 transcript, so twr never takes one this way; {BY_HAND}"
            ));
        }
    }
    None
}

/// What `config.set` does with `key` (its parts) set to `value`, and when that takes effect.
pub(super) fn classify(key: &[String], value: &Scalar) -> (Class, Effect) {
    let parts: Vec<&str> = key.iter().map(String::as_str).collect();
    let effect = Effect::of(&parts);
    if let Some(why) = refusal(key, value) {
        return (Class::Refused(why), effect);
    }
    let direct = DIRECT
        .iter()
        .any(|d| d.split('.').eq(parts.iter().copied()));
    (if direct { Class::Direct } else { Class::Card }, effect)
}

/// Longest key `config.set` takes, in bytes.
const KEY_MAX: usize = 256;

/// Most parts a key may have.
const KEY_PARTS_MAX: usize = 8;

/// Whether `c` may be in a key part written without quotes.
fn bare_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Whether `c` would not show as itself in a key on a card or a conversation line: a control
/// or format character, a separator other than the space, a variation selector (what Rust's
/// `Debug` escapes after a letter).
fn hidden(c: char) -> bool {
    if matches!(c, '"' | '\\' | '\'') {
        return false;
    }
    let s: String = ['a', c].iter().collect();
    s.escape_debug().to_string() != s
}

/// The parts of dotted key `key` (`tui.suggest`, `projects."my.api".codex_auto`), or why it is
/// none.
pub(super) fn parse_key(key: &str) -> Result<Vec<String>, String> {
    let bad = || {
        format!(
            "{key:?} is not a config key: write it with dots, as in tui.suggest, and quote a part \
             that holds anything but letters, digits, _ and -, as in projects.\"my.api\".codex_auto"
        )
    };
    if key.is_empty() || key.len() > KEY_MAX {
        return Err(bad());
    }
    let mut parts = Vec::new();
    let mut rest = key;
    loop {
        if let Some(q) = rest.strip_prefix('"') {
            let mut part = String::new();
            let mut it = q.char_indices();
            let mut end = None;
            while let Some((i, c)) = it.next() {
                match c {
                    '"' => {
                        end = Some(i + 1);
                        break;
                    }
                    '\\' => match it.next() {
                        Some((_, e @ ('"' | '\\'))) => part.push(e),
                        _ => return Err(bad()),
                    },
                    c if hidden(c) => return Err(bad()),
                    c => part.push(c),
                }
            }
            let Some(end) = end else {
                return Err(bad());
            };
            if part.is_empty() {
                return Err(bad());
            }
            parts.push(part);
            rest = &q[end..];
        } else {
            let end = rest.find(|c: char| !bare_char(c)).unwrap_or(rest.len());
            if end == 0 {
                return Err(bad());
            }
            parts.push(rest[..end].to_string());
            rest = &rest[end..];
        }
        if rest.is_empty() {
            break;
        }
        rest = rest.strip_prefix('.').ok_or_else(bad)?;
    }
    if parts.len() > KEY_PARTS_MAX {
        return Err(bad());
    }
    Ok(parts)
}

/// Key `parts` as `config.set` writes it: each part bare when it can be, else quoted.
pub(super) fn key_text(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if !p.is_empty() && p.chars().all(bare_char) {
                p.clone()
            } else {
                format!("\"{}\"", p.replace('\\', "\\\\").replace('"', "\\\""))
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// What `config.set` says of a list or a table.
const NOT_A_SCALAR: &str = "edit config.toml by hand (lists and tables aren't supported yet)";

/// `value` as a scalar to write.
fn scalar_of(value: &Value) -> Result<Scalar, RpcError> {
    match value {
        Value::Bool(b) => Ok(Scalar::Bool(*b)),
        Value::String(s) => Ok(Scalar::Str(s.clone())),
        Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Ok(Scalar::Int(i)),
            (None, Some(f)) => Ok(Scalar::Float(f)),
            (None, None) => Err(rpc(
                codes::BAD_REQUEST,
                format!("{n} is not a number twr takes"),
            )),
        },
        Value::Array(_) | Value::Object(_) => Err(rpc(codes::BAD_REQUEST, NOT_A_SCALAR)),
        Value::Null => Err(rpc(
            codes::BAD_REQUEST,
            "a value is required: a boolean, a number or a string (to unset a key, the user \
             edits config.toml by hand)",
        )),
    }
}

/// `s` as JSON.
pub(super) fn scalar_json(s: &Scalar) -> Value {
    match s {
        Scalar::Bool(b) => json!(b),
        Scalar::Int(i) => json!(i),
        Scalar::Float(f) => json!(f),
        Scalar::Str(s) => json!(s),
    }
}

/// `s` as a confirm card shows it.
pub(super) fn card_value(v: &Value) -> CardValue {
    match v {
        Value::String(s) => CardValue::Str(s.clone()),
        other => CardValue::Literal(other.to_string()),
    }
}

/// What the file's `text` has at `key`, as `config.set` compares and reports it: `Ok(None)`
/// when unset, `bad_request` for a list, a table or a date, or a file that does not parse.
fn file_value(text: &str, key: &[String]) -> Result<Option<Scalar>, RpcError> {
    let (table, leaf) = split(key);
    match config_edit::current(text, &table, leaf).map_err(|e| rpc(codes::BAD_REQUEST, e))? {
        Current::Unset => Ok(None),
        Current::Scalar(s) => Ok(Some(s)),
        Current::Other => Err(rpc(codes::BAD_REQUEST, NOT_A_SCALAR)),
    }
}

/// Key `key`'s table path and its last part.
fn split(key: &[String]) -> (Vec<&str>, &str) {
    let (leaf, table) = key.split_last().expect("a key has a part");
    (table.iter().map(String::as_str).collect(), leaf)
}

/// What setting `key` to `value` in the file's `text` gives: the value as written (an integer
/// becomes a float where the file has a float), what the file had, and the new text. A key
/// whose value is a list or a table, a value the file has already, and one twr would refuse
/// (`config_edit::set_key`) are `bad_request`.
fn edit(
    text: &str,
    key: &[String],
    value: Scalar,
) -> Result<(Scalar, Option<Scalar>, String), RpcError> {
    let name = key_text(key);
    let before = file_value(text, key)?;
    let value = match (&before, value) {
        (Some(Scalar::Float(_)), Scalar::Int(i)) => Scalar::Float(i as f64),
        (_, v) => v,
    };
    if before.as_ref() == Some(&value) {
        return Err(rpc(
            codes::BAD_REQUEST,
            format!(
                "{name} is already {} in config.toml: nothing to change",
                value.text()
            ),
        ));
    }
    let (table, leaf) = split(key);
    let new =
        config_edit::set_key(text, &table, leaf, &value).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
    Ok((value, before, new))
}

/// Writes `new` over `old`, the file's text when it was read ([`config_edit::write_text`]).
pub(super) fn write_over(file: &Path, new: &str, old: &str) -> Result<(), RpcError> {
    config_edit::write_text(file, new, &config_edit::text_hash(old)).map_err(|e| {
        if e.starts_with("Configuration changed elsewhere") {
            rpc(
                codes::CONFLICT,
                "config.toml changed while this was being written: nothing was changed; try again",
            )
        } else {
            rpc(
                codes::BAD_REQUEST,
                format!("config.toml was not changed: {e}"),
            )
        }
    })
}

/// What the orchestrator is told about a key that goes on the confirm card.
const NEEDS_CONFIRMATION_CONFIG: &str = "Not applied yet: this key changes how twr works, so \
    the user confirms it. It goes on a confirm card when your turn ends, with everything else \
    you proposed in this turn, and only the user can confirm it. Tell the user: \"I've put that \
    on a card for you to confirm.\" and when it takes effect once confirmed (effect).";

/// The config file's text as staged by a confirm card's config actions ([`Core::stage_config`]):
/// what the file had when the first one read it, and the text with every one staged so far.
#[derive(Debug)]
pub(super) struct ConfigEdit {
    pub(super) file: PathBuf,
    pub(super) old: String,
    pub(super) new: String,
}

/// `config.show`'s `preferences`: the [`DIRECT`] keys as config.toml has them now (`null`:
/// unset, twr's default applies), since the rest of that view is what the hub loaded when it
/// started, and it shows no `[speech]` or `[voice]`; `null` without a file that can be read.
pub(super) fn preferences(hub: &Inner) -> Value {
    let Some(text) = hub
        .config_file
        .as_deref()
        .and_then(|f| config_edit::read_text(f).ok())
    else {
        return Value::Null;
    };
    let mut out = serde_json::Map::new();
    for key in DIRECT {
        let parts: Vec<&str> = key.split('.').collect();
        let (leaf, table) = parts.split_last().expect("a key has a part");
        match config_edit::current(&text, table, leaf) {
            Ok(Current::Scalar(v)) => {
                out.insert(key.to_string(), scalar_json(&v));
            }
            Ok(Current::Unset) => {
                out.insert(key.to_string(), Value::Null);
            }
            Ok(Current::Other) => {}
            Err(_) => return Value::Null,
        }
    }
    Value::Object(out)
}

/// The access level a change of key `key` (its parts) to `value` sets the orchestrator to, when
/// it is `orchestrator.access` (`hub::orch_access`).
pub(super) fn access_change(
    key: &[String],
    value: &Value,
) -> Option<twr_core::config::OrchestratorAccess> {
    (key.len() == 2 && key[0] == "orchestrator" && key[1] == "access")
        .then(|| super::orch_access::parse(value))
        .flatten()
}

/// What `config.set` takes.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigSetParams {
    /// The dotted key.
    pub key: String,
    /// The value: a boolean, a number or a string.
    pub value: Value,
}

impl Hub {
    /// `config.set {key, value}` (see the module docs). Returns `{status: "applied", key,
    /// value, before, effect}`, or, for the orchestrator and a key that goes on the confirm
    /// card, `{status: "needs_confirmation", key, value, before, effect, message}`.
    pub async fn config_set(
        &self,
        key: &str,
        value: Value,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.config_set_bound(key, value, caller, None).await
    }

    /// [`Hub::config_set`], the orchestrator's call bound to `received`, the orchestrator as it
    /// was when the request came in (`hub::withdraw`).
    pub(crate) async fn config_set_bound(
        &self,
        key: &str,
        value: Value,
        caller: &Caller,
        received: Option<&OrchCall>,
    ) -> Result<Value, RpcError> {
        let (hub, key, caller, received) = (
            self.clone(),
            key.to_string(),
            caller.clone(),
            received.cloned(),
        );
        // The file is read, locked and written: off the async threads.
        tokio::task::spawn_blocking(move || {
            let inner = &hub.inner;
            inner
                .core()
                .config_set(inner, &key, &value, &caller, received.as_ref())
        })
        .await
        .map_err(internal)?
    }
}

impl Core {
    /// [`Hub::config_set`] under the core lock.
    fn config_set(
        &mut self,
        hub: &Arc<Inner>,
        key: &str,
        value: &Value,
        caller: &Caller,
        received: Option<&OrchCall>,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let parts = parse_key(key).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let name = key_text(&parts);
        let scalar = scalar_of(value)?;
        hub.require_scope(caller, "change twr's configuration", "admin")?;
        let (class, effect) = classify(&parts, &scalar);
        if let Class::Refused(why) = &class {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("{name} is not changed through twr: {why}"),
            ));
        }
        if caller.is_orchestrator() {
            self.config_check(hub, received)?;
        }
        let file = hub.config_file.clone().ok_or_else(|| {
            rpc(
                codes::BAD_REQUEST,
                "this hub runs without a config file: there is nothing to change",
            )
        })?;
        let text = config_edit::read_text(&file).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let (scalar, before, new) = edit(&text, &parts, scalar)?;
        let (value, before) = (scalar_json(&scalar), before.as_ref().map(scalar_json));
        let mut out = json!({
            "key": name,
            "value": value,
            "before": before,
            "effect": effect.text(),
        });
        if caller.is_orchestrator() && class == Class::Card {
            if self.config_proposed(&parts) {
                return Err(rpc(
                    codes::RELAY_FORBIDDEN,
                    format!("you already proposed a change to {name} in this turn; ask the user"),
                ));
            }
            self.orch.record_proposal(Proposed::Config {
                key: parts,
                value,
                before,
            });
            out["status"] = json!("needs_confirmation");
            out["message"] = json!(NEEDS_CONFIRMATION_CONFIG);
            return Ok(out);
        }
        write_over(&file, &new, &text)?;
        let by = if caller.is_orchestrator() {
            "orchestrator"
        } else {
            "user"
        };
        if let Some(orch) = self.orch.thread.clone() {
            let turn = caller
                .is_orchestrator()
                .then(|| self.registry.get(&orch).and_then(|v| v.current_turn))
                .flatten();
            let rec = EventBody::ConfigChanged(ConfigChanged {
                key: name.clone(),
                value: value.clone(),
                before: before.clone(),
                by: by.into(),
                effect: effect.text().into(),
                ..Default::default()
            });
            // The file is written already: a failure to record it is only logged.
            if let Err(e) = self.append(hub, &orch, turn, Source::Hub, rec) {
                tracing::warn!(error = %e, key = %name, "could not record a config change");
            }
        }
        // The orchestrator's access applies now (`hub::orch_access`); only the user gets
        // here with it (the orchestrator's goes on the card).
        if let Some(to) = access_change(&parts, &value) {
            out["restart"] = json!(self.apply_access(hub, to, caller.source()));
        }
        out["status"] = json!("applied");
        Ok(out)
    }

    /// Whether confirm-card action `key` = `value` still means something: the file does not
    /// have that value already (a file that can't be read: it may).
    pub(super) fn config_still_proposed(hub: &Inner, key: &[String], value: &Value) -> bool {
        let Some(file) = &hub.config_file else {
            return false;
        };
        let Ok(text) = config_edit::read_text(file) else {
            return true;
        };
        !matches!(file_value(&text, key), Ok(Some(v)) if scalar_json(&v) == *value)
    }

    /// Stages confirm-card action `key` = `value` (proposed when the file had `before`) as
    /// the user's, against the text `edit` holds (the file as the card's first config action
    /// read it, with the earlier ones applied): refused again as `config.set` would refuse
    /// it, and a conflict when the key's value is no longer `before`. Returns the record to
    /// commit with the card once the file is written ([`Core::confirm_card`]).
    pub(super) fn stage_config(
        hub: &Inner,
        edit_state: &mut Option<ConfigEdit>,
        key: &[String],
        value: &Value,
        before: Option<&Value>,
    ) -> Result<ConfigChanged, RpcError> {
        let name = key_text(key);
        let scalar = scalar_of(value)?;
        let (class, effect) = classify(key, &scalar);
        if let Class::Refused(why) = class {
            return Err(rpc(codes::BAD_REQUEST, why));
        }
        if edit_state.is_none() {
            let file = hub
                .config_file
                .clone()
                .ok_or_else(|| rpc(codes::BAD_REQUEST, "this hub runs without a config file"))?;
            let old = config_edit::read_text(&file).map_err(|e| rpc(codes::BAD_REQUEST, e))?;
            *edit_state = Some(ConfigEdit {
                file,
                new: old.clone(),
                old,
            });
        }
        let state = edit_state.as_mut().expect("read above");
        let now = file_value(&state.new, key)?.as_ref().map(scalar_json);
        if now.as_ref() != before {
            return Err(rpc(
                codes::CONFLICT,
                "config.toml changed since the card was raised",
            ));
        }
        let (scalar, _, new) = edit(&state.new, key, scalar)?;
        state.new = new;
        Ok(ConfigChanged {
            key: name,
            value: scalar_json(&scalar),
            before: before.cloned(),
            by: "user".into(),
            effect: effect.text().into(),
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(key: &str) -> Vec<String> {
        parse_key(key).unwrap()
    }

    #[test]
    fn keys_parse_and_print_back() {
        assert_eq!(parts("tui.suggest"), vec!["tui", "suggest"]);
        assert_eq!(parts("default_runtime"), vec!["default_runtime"]);
        assert_eq!(
            parts(r#"projects."my.api".codex_auto"#),
            vec!["projects", "my.api", "codex_auto"]
        );
        assert_eq!(parts(r#""a\"b\\c""#), vec!["a\"b\\c"]);
        for k in [
            "tui.suggest",
            r#"projects."my.api".codex_auto"#,
            r#""a\"b\\c".x"#,
        ] {
            assert_eq!(key_text(&parts(k)), k);
        }
        // Spelled bare where it can be.
        assert_eq!(key_text(&parts(r#""tui".suggest"#)), "tui.suggest");
        for bad in [
            "",
            ".",
            "tui.",
            ".tui",
            "tui..suggest",
            "tui suggest",
            "tui.suggest = 1",
            "tui.\"open",
            "tui.\"\"",
            "tui.\"a\\nb\"",
            "tui.\"a\u{7}\"",
            "projects.\"a\u{202E}b\".base",
            "projects.\"a\u{200B}\".base",
            "a.b.c.d.e.f.g.h.i",
            "tui.sügg",
        ] {
            assert!(parse_key(bad).is_err(), "{bad:?}");
        }
    }

    fn class(key: &str, value: Scalar) -> Class {
        classify(&parts(key), &value).0
    }

    #[test]
    fn the_key_classes() {
        let s = |v: &str| Scalar::Str(v.into());
        for k in DIRECT {
            assert_eq!(class(k, Scalar::Bool(true)), Class::Direct, "{k}");
        }
        for (k, v) in [
            ("threads.default_mode", s("auto")),
            ("orchestrator.access", s("full")),
            ("orchestrator.runtime", s("codex")),
            ("codex.auto", s("never")),
            ("project_roots", s("x")),
            ("default_runtime", s("gemini")),
            ("projects.api.runtime", s("codex")),
            ("projects.env.codex_auto", s("ask")),
            ("tui.suggest_model.codex", s("gpt-6-luna")),
            ("speech.engine", s("say")),
            ("voice.whisper", s("/opt/whisper-cli")),
            ("hub.listen", s("0.0.0.0:7878")),
            ("runtimes.gemini.review.mode", s("plan")),
            ("a_key_twr_adds_later", Scalar::Bool(true)),
            // Not one of the direct keys, spelled as one key part.
            (r#""tui.mouse""#, Scalar::Bool(true)),
        ] {
            assert_eq!(class(k, v), Class::Card, "{k}");
        }
        for (k, v) in [
            ("boxes.lab.token", s("x")),
            ("boxes.gpu.projects.api.runtime", s("codex")),
            ("hub.tls.cert", s("a.pem")),
            ("hub.tls", s("a.pem")),
            ("threads.env.API_KEY", s("x")),
            ("projects.api.env.TOKEN", s("x")),
            ("notify.command", s("x")),
            ("speech.command", s("x")),
            ("voice.command", s("x")),
            ("runtimes.gemini.acp", s("gemini --acp")),
            ("runtimes.gemini.attach", s("gemini")),
            ("default_runtime", s("acp:curl x | sh")),
            ("orchestrator.runtime", s("pty:bash")),
            ("projects.api.runtime", s(" acp")),
            (
                "projects.api.base",
                s("ghp_0123456789abcdefghijklmnopqrstuvwxyz"),
            ),
            ("tui.colors", s("-----BEGIN OPENSSH PRIVATE KEY-----")),
        ] {
            assert!(matches!(class(k, v), Class::Refused(_)), "{k}");
        }
    }

    #[test]
    fn every_direct_key_is_a_key_twr_takes() {
        // A value of the right type for each: twr loads the result.
        let value = |k: &str| match k {
            "tui.mouse"
            | "tui.animate"
            | "tui.suggest"
            | "tui.summaries"
            | "voice.listen_while_speaking"
            | "threads.thinking" => Scalar::Bool(true),
            "tui.compact_width" | "speech.rate" | "speech.catchup_secs" | "voice.timeout_secs" => {
                Scalar::Int(100)
            }
            "voice.pause_ms" => Scalar::Int(1500),
            "speech.volume" => Scalar::Float(0.5),
            "tui.card_order" => Scalar::Str("attention".into()),
            "tui.summary_every" => Scalar::Str("10m".into()),
            "tui.colors" => Scalar::Str("16".into()),
            "tui.notify.terminal" => Scalar::Str("bell".into()),
            "tui.notify.when" => Scalar::Str("always".into()),
            "speech.voice" => Scalar::Str("Ava".into()),
            "speech.turn_from" => Scalar::Str("mine".into()),
            "speech.code" => Scalar::Str("skip".into()),
            "speech.paths" => Scalar::Str("full".into()),
            "voice.model" => Scalar::Str("/m.bin".into()),
            "voice.language" => Scalar::Str("es".into()),
            other => panic!("no sample value for {other}"),
        };
        for k in DIRECT {
            let p = parts(k);
            let (s, before, new) = edit("", &p, value(k)).unwrap_or_else(|e| panic!("{k}: {e:?}"));
            assert_eq!(before, None, "{k}");
            assert_eq!(s, value(k), "{k}");
            twr_core::config::Config::from_toml_str(&new).unwrap();
        }
    }

    #[test]
    fn edits_refuse_lists_tables_no_ops_and_bad_values() {
        let text = "[tui]\nsuggest = true # mine\n[speech]\nvolume = 1.0\nread = [\"replies\"]\n";
        let p = parts("tui.suggest");
        let (_, before, new) = edit(text, &p, Scalar::Bool(false)).unwrap();
        assert_eq!(before, Some(Scalar::Bool(true)));
        assert_eq!(new, text.replace("suggest = true", "suggest = false"));
        let e = edit(text, &p, Scalar::Bool(true)).unwrap_err();
        assert!(e.message.contains("already true"), "{e:?}");
        let e = edit(text, &parts("speech.read"), Scalar::Str("x".into())).unwrap_err();
        assert_eq!(e.message, NOT_A_SCALAR);
        let e = edit(text, &parts("tui"), Scalar::Bool(true)).unwrap_err();
        assert_eq!(e.message, NOT_A_SCALAR);
        let e = edit(text, &parts("tui.bogus"), Scalar::Bool(true)).unwrap_err();
        assert!(e.message.contains("unknown field `bogus`"), "{e:?}");
        let e = edit(text, &parts("tui.card_order"), Scalar::Str("x".into())).unwrap_err();
        assert!(
            e.message.starts_with("twr does not take tui.card_order"),
            "{e:?}"
        );
        // An integer where the file has a float is written as a float.
        let (s, _, new) = edit(text, &parts("speech.volume"), Scalar::Int(0)).unwrap();
        assert_eq!(s, Scalar::Float(0.0));
        assert!(new.contains("volume = 0.0\n"), "{new}");
        assert_eq!(scalar_of(&json!([1])).unwrap_err().message, NOT_A_SCALAR);
        assert!(scalar_of(&Value::Null).is_err());
        assert_eq!(scalar_of(&json!(0.5)).unwrap(), Scalar::Float(0.5));
        assert_eq!(scalar_of(&json!(5)).unwrap(), Scalar::Int(5));
    }

    #[test]
    fn effects() {
        let e = |k: &str| classify(&parts(k), &Scalar::Bool(true)).1;
        assert_eq!(e("tui.suggest"), Effect::TuiStart);
        assert_eq!(e("tui.notify.terminal"), Effect::TuiStart);
        assert_eq!(e("speech.rate"), Effect::SpeakerPref);
        assert_eq!(e("speech.code"), Effect::SpeakerStart);
        assert_eq!(e("voice.model"), Effect::NextRecording);
        assert_eq!(e("voice.pause_ms"), Effect::VoiceStart);
        assert_eq!(e("threads.thinking"), Effect::HubRestart);
        assert_eq!(e("orchestrator.model"), Effect::FirstOrchestrator);
        assert_eq!(e("orchestrator.access"), Effect::OrchestratorAccess);
    }
}

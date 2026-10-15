//! Grok Build's ACP dialect (`grok agent stdio`): the vendor-neutral [`crate::acp`]
//! translator plus Grok's extensions.
//!
//! **Verified against a signed-in grok 1.0.44 (2026-09-30)**: every shape below was recorded
//! from the real binary (`tests/fixtures/grok/real_*_v1044.both.jsonl`, replayed by the tests)
//! except where marked otherwise. The older `synthetic_*` fixtures (written by hand from the
//! ACP spec before grok could be signed in) still cover edge cases a recording cannot provoke
//! on demand (replaced requests, `$/cancel_request`, failed `session/set_mode`).
//!
//! The standard flow (handshake, `session/new|resume|load`, updates, permissions, cancel,
//! stop reasons, modes, the relay rule) is [`crate::acp::translate`]'s; what grok 1.0.44 does
//! with it was verified: `session/resume` is advertised and used (no history is replayed, the
//! result has no `sessionId`, and the session remembers the earlier conversation); a manual
//! `session/load` replayed history as expected (user, thought, message and tool chunks);
//! `reject_once` (and `session/cancel`) end the prompt with `stopReason: cancelled`, so a
//! denied permission ends the turn as interrupted; `session/cancel` also kills a running shell
//! command. grok 1.0.44 offers `allow-edits-session` / `always-allow` (`allow_always`), which
//! twr never selects. Responses to ids twr did not issue (grok sends `"id":"skills-reload"`)
//! are ignored.
//!
//! Grok's layer ([`Grok`]):
//! - `_meta` on `session/new|resume|load` is `{yoloMode: <twr mode is bypassPermissions>}`
//!   plus `rules` (the system prompt addition; grok 1.0.44 ignores the top-level `--rules` flag
//!   in `agent stdio` but honors `_meta.rules`) and `agentProfile` (an inline agent definition
//!   whose `tools` list is the only toolset the model gets: the real tool restriction, see
//!   [`crate::grok_build`]). On resume grok keeps the rules and profile the session was
//!   created with (verified); twr sends them again anyway.
//! - The model (`TranslatorOptions::model`) is always set with
//!   `session/set_config_option{configId: "model", value}` (grok 1.0.44 ignores `agent -m` for
//!   ACP sessions; an unknown model id fails the session).
//! - The reasoning effort (`TranslatorOptions::effort`) is set next with the standard request:
//!   grok 1.0.44 reports it as the `configOptions` entry `reasoning_effort` (category
//!   `thought_level`, values `xhigh`, `high`, `medium`, `low`; verified 2026-10-03:
//!   `session/set_config_option` sets it, an unknown value is refused with "unknown
//!   reasoning_effort value", and the top-level `--reasoning-effort` flag is ignored by `agent
//!   stdio`, like `-m`).
//! - Modes: grok 1.0.44 reports no `modes` on `session/new|resume`, accepts any
//!   `session/set_mode` id with `{}`, and only emits `current_mode_update` for plan mode
//!   (`plan`, then `default` when a plan is approved). So the permission mode is pinned by the
//!   command line (`--permission-mode`) and `_meta.yoloMode`, each verified to make grok send
//!   `session/request_permission` even when the user's `config.toml` says always-approve, and
//!   checked with the one signal grok gives: `_x.ai/sessions/changed` carries `yolo` for the
//!   session once a prompt runs. `yolo: true` when twr did not ask for bypass cancels the
//!   running prompt and fails the session (every waiting turn ends with an error, the driver
//!   stops). A report that arrives while a new session opens (its id not known yet) is kept
//!   and checked once the id is known ([`Grok::veto`]: before the session becomes ready and
//!   before every prompt). Should a grok report `modes`, the standard checks apply.
//! - `_x.ai/ask_user_question` (verified) → one `attention.raised{question}` per question
//!   (options = its labels), answered `{outcome: accepted, answers: {<question>: <answer>}}`
//!   or `{outcome: cancelled}`.
//! - `_x.ai/exit_plan_mode` (verified) → `attention.raised{permission}` "approve the plan":
//!   `allow` → `{outcome: approved}` (grok leaves plan mode), anything else → `{outcome:
//!   rejected}` (grok treats any outcome but `approved` / `abandoned` as "revise the plan" and
//!   stays in plan mode); twr stopping answers it with an error (grok keeps plan mode). The
//!   user's approval is the consent that lets a `plan` session continue in the `default`
//!   mode grok reports next (`ext.grok.mode_changed` on the turn's end); anything wider still
//!   fails the session (see [`crate::acp::translate`], "Plan-exit consent").
//! - grok's first `tool_call` has no ACP `kind`: `_meta."x.ai/tool".kind` stands in, and
//!   `_meta."x.ai/tool".name` is `ext.grok.tool`.
//! - Usage ([`twr_core::usage`]): `_x.ai/billing` (no turn; grok 1.0.46) answers the
//!   account's credit period, `{config: {currentPeriod: {type: USAGE_PERIOD_TYPE_WEEKLY, start,
//!   end}, creditUsagePercent, ...}, subscription_tier}` ([`billing_windows`]); read when
//!   [`TranslatorOptions::usage`] is set (the adapter sets it).
//! - Everything else grok sends under `_x.ai/*` / `x.ai/*` (setup phases, queue, usage, hooks,
//!   MCP status, ...) is ignored.
//! - `thread.started` `ext.grok` adds `verified_with`, the profile's `tools` and the
//!   `denied_tools`; the version is `initialize`'s `_meta.agentVersion`.

use serde_json::{json, Map, Value};
use twr_core::usage::{UsageWindow, MONTH, SEVEN_DAY};

pub use crate::acp::translate::{
    error_text, is_bypass, mode_acceptable, mode_level, narrowing_target, str_at, transient_reason,
    Emitted, ModeLevel, Options, Translated, INPUT_SUMMARY_MAX, OUTPUT_SUMMARY_MAX,
    OVERSIZED_ERROR, PLAN_MAX, PROTOCOL_VERSION, RISKY_KINDS,
};
use crate::acp::translate::{Dialect, ExtRequest, SessionCtx, ToolMeta};
#[cfg(test)]
use twr_core::event::{EventBody, TurnResult};

/// Key of this runtime's extras (`data.ext.grok`).
pub const EXT: &str = "grok";

/// The grok version the ACP flow was last verified against (`ext.grok.verified_with`).
pub const VERIFIED_WITH: &str = "1.0.44";

/// Grok's plan-approval request.
pub const EXIT_PLAN_METHOD: &str = "_x.ai/exit_plan_mode";

/// Grok's question request.
pub const ASK_METHOD: &str = "_x.ai/ask_user_question";

/// Grok's read of the account's credit usage.
pub const BILLING_METHOD: &str = "_x.ai/billing";

/// Grok's session-list notification (carries `yolo` per session).
pub const SESSIONS_CHANGED: &str = "_x.ai/sessions/changed";

/// Static facts about the session a translator serves.
#[derive(Debug, Clone, Default)]
pub struct TranslatorOptions {
    /// Emit `agent.thinking`.
    pub thinking: bool,
    /// Runtime process id (`ext.twr.pid`).
    pub pid: Option<u32>,
    /// Program the process was started as (`ext.twr.exe`).
    pub exe: Option<String>,
    /// Working directory sent as the session `cwd` (absolute, per ACP).
    pub cwd: String,
    /// ACP `mcpServers` entries.
    pub mcp_servers: Vec<Value>,
    /// Session to resume instead of starting one.
    pub resume_session: Option<String>,
    /// twr permission mode (`SpawnConfig::mode`, empty = `default`).
    pub mode: String,
    /// Model set with `session/set_config_option` once the session is open.
    pub model: Option<String>,
    /// Reasoning effort set with `session/set_config_option{configId: "reasoning_effort"}`
    /// after the model.
    pub effort: Option<String>,
    /// System prompt addition (`_meta.rules`).
    pub rules: Option<String>,
    /// Inline agent definition (`_meta.agentProfile`) restricting the toolset.
    pub agent_profile: Option<Value>,
    /// Built-in tools denied with `--deny` (`ext.grok.denied_tools`).
    pub denied_tools: Vec<String>,
    /// Read the account's credit usage ([`BILLING_METHOD`]) when the session is ready and as
    /// it gets old.
    pub usage: bool,
}

/// Grok Build's extensions to ACP (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Grok {
    /// twr permission mode (`yoloMode`).
    mode: String,
    rules: Option<String>,
    agent_profile: Option<Value>,
    denied_tools: Vec<String>,
    /// Last `yolo` grok reported for the session (`_x.ai/sessions/changed`).
    yolo: Option<bool>,
    /// `yolo` reported for sessions while the session id was not known yet (a new session
    /// opening): looked up once it is ([`Dialect::veto`]). At most [`YOLO_SEEN_MAX`] entries.
    yolo_seen: Vec<(String, bool)>,
    /// More than [`YOLO_SEEN_MAX`] sessions were reported before the id was known: what was
    /// dropped could be this session's, so the session does not run.
    yolo_overflow: bool,
    /// Read the credit usage ([`TranslatorOptions::usage`]).
    usage: bool,
}

/// Sessions whose `yolo` is remembered while a new session opens.
pub const YOLO_SEEN_MAX: usize = 1024;

/// The window and plan in an `_x.ai/billing` answer: the current credit period (a weekly one
/// is [`SEVEN_DAY`], a monthly one [`MONTH`], another named by its length), used
/// `creditUsagePercent`, resetting at the period's `end`; the plan is `subscription_tier`.
/// grok's answer is protobuf JSON, which leaves a zero out: a period without
/// `creditUsagePercent` is 0% used.
pub fn billing_windows(result: &Value) -> (Vec<UsageWindow>, Option<String>) {
    let plan = result
        .get("subscription_tier")
        .and_then(Value::as_str)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let config = &result["config"];
    let period = &config["currentPeriod"];
    if !period.is_object() {
        return (Vec::new(), plan);
    }
    let used = &config["creditUsagePercent"];
    let used = match used {
        Value::Null => Some(0.0),
        Value::String(s) => s.parse::<f64>().ok().filter(|f| f.is_finite()),
        v => crate::usage::pct(v),
    };
    let Some(used) = used else {
        return (Vec::new(), plan);
    };
    let start = crate::usage::rfc3339(&period["start"]);
    let end = crate::usage::rfc3339(&period["end"]);
    let kind = str_at(period, "type");
    let window = if kind.ends_with("_WEEKLY") {
        UsageWindow {
            window: SEVEN_DAY.into(),
            used_pct: used,
            resets_at: end,
            minutes: None,
            observed_at: None,
        }
    } else if kind.ends_with("_MONTHLY") {
        UsageWindow {
            window: MONTH.into(),
            used_pct: used,
            resets_at: end,
            minutes: None,
            observed_at: None,
        }
    } else {
        match (start, end) {
            (Some(s), Some(e)) if e > s => {
                let minutes = u64::try_from((e - s).num_minutes()).unwrap_or(0);
                UsageWindow::of_minutes(minutes, used, Some(e))
            }
            _ => return (Vec::new(), plan),
        }
    };
    (vec![window], plan)
}

/// The stateful ACP ↔ twr translator for one `grok agent stdio` process.
pub type Translator = crate::acp::translate::Translator<Grok>;

impl Translator {
    /// A translator for a fresh process.
    pub fn new(opts: TranslatorOptions) -> Self {
        let grok = Grok {
            mode: opts.mode.clone(),
            rules: opts.rules,
            agent_profile: opts.agent_profile,
            denied_tools: opts.denied_tools,
            yolo: None,
            yolo_seen: Vec::new(),
            yolo_overflow: false,
            usage: opts.usage,
        };
        let core = Options {
            thinking: opts.thinking,
            pid: opts.pid,
            exe: opts.exe,
            cwd: opts.cwd,
            mcp_servers: opts.mcp_servers,
            resume_session: opts.resume_session,
            mode: opts.mode,
            model: opts.model,
            effort: opts.effort,
        };
        crate::acp::translate::Translator::with_dialect(core, grok)
    }

    /// The last `yolo` (always-approve) state grok reported for the session.
    pub fn yolo(&self) -> Option<bool> {
        self.dialect().yolo
    }
}

impl Dialect for Grok {
    fn ext_key(&self) -> &'static str {
        EXT
    }

    fn log_name(&self) -> &'static str {
        super::NAME
    }

    fn name(&self) -> &str {
        "grok"
    }

    fn title(&self) -> &str {
        "Grok"
    }

    fn session_meta(&self) -> Option<Value> {
        let mut meta = json!({"yoloMode": is_bypass(&self.mode)});
        if let Some(r) = self.rules.as_deref().filter(|r| !r.is_empty()) {
            meta["rules"] = json!(r);
        }
        if let Some(p) = &self.agent_profile {
            meta["agentProfile"] = p.clone();
        }
        Some(meta)
    }

    fn agent_version(&self, r: &Value) -> Option<String> {
        r.pointer("/_meta/agentVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn model_request(
        &self,
        _open: &Value,
        sid: &str,
        model: &str,
    ) -> Result<(&'static str, Value), String> {
        Ok((
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "model", "value": model}),
        ))
    }

    fn extension_request(&self, method: &str) -> Option<ExtRequest> {
        match method {
            EXIT_PLAN_METHOD => Some(ExtRequest::PlanApproval),
            ASK_METHOD => Some(ExtRequest::Question),
            _ => None,
        }
    }

    /// `_x.ai/sessions/changed`: grok's view of its sessions, with `yolo` (always-approve).
    /// Always-approve twr did not ask for ends the session. Reports that arrive while a new
    /// session opens (its id not known yet) are kept and checked once it is ([`Grok::veto`]).
    fn on_notification(
        &mut self,
        method: &str,
        params: &Value,
        ctx: SessionCtx<'_>,
    ) -> Option<String> {
        if method != SESSIONS_CHANGED {
            return None;
        }
        let entries = params
            .get("upserted")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let sid = e.get("sessionId").and_then(Value::as_str)?;
                let yolo = e.get("yolo").and_then(Value::as_bool)?;
                Some((sid.to_string(), yolo))
            });
        match ctx.session_id {
            Some(sid) => {
                for (s, yolo) in entries {
                    if s == sid {
                        self.yolo = Some(yolo);
                    }
                }
            }
            None => {
                for (s, yolo) in entries {
                    self.yolo_seen.retain(|(x, _)| *x != s);
                    if self.yolo_seen.len() >= YOLO_SEEN_MAX {
                        self.yolo_overflow = true;
                        self.yolo_seen.remove(0);
                    }
                    self.yolo_seen.push((s, yolo));
                }
                return None;
            }
        }
        self.veto(ctx)
    }

    /// The session must not run while grok reports always-approve (yolo) for it and twr did
    /// not ask for bypass, including a report that came while the session was opening.
    fn veto(&self, ctx: SessionCtx<'_>) -> Option<String> {
        if is_bypass(ctx.requested_mode) {
            return None;
        }
        let sid = ctx.session_id?;
        let early = || {
            self.yolo_seen
                .iter()
                .rev()
                .find(|(s, _)| s == sid)
                .map(|(_, y)| *y)
        };
        if self.yolo.or_else(early) == Some(true) {
            return Some(format!(
                "grok reports always-approve (yolo) for the session although twr asked for mode \
                 {:?}: tools would run without asking",
                ctx.requested_mode
            ));
        }
        if self.yolo.is_none() && early().is_none() && self.yolo_overflow {
            return Some(format!(
                "grok reported more than {YOLO_SEEN_MAX} sessions while this one opened; twr \
                 cannot tell whether it is in always-approve (yolo)"
            ));
        }
        None
    }

    /// The mode is fixed by `--permission-mode` and `_meta.yoloMode` (grok 1.0.44 reports no
    /// `modes`).
    fn mode_pinned(&self) -> bool {
        true
    }

    /// grok's own description of the tool (its first `tool_call` has no ACP kind). A
    /// `use_tool` call runs an MCP server's tool: `rawInput` `{tool_name: "<server>__<tool>",
    /// tool_input}`.
    fn tool_meta(&self, u: &Value) -> ToolMeta {
        let Some(t) = u.pointer("/_meta/x.ai~1tool") else {
            return ToolMeta::default();
        };
        let name = str_at(t, "name").to_string();
        let raw = &u["rawInput"];
        let mcp_call = (name == "use_tool" && !str_at(raw, "tool_name").is_empty()).then(|| {
            (
                str_at(raw, "tool_name").to_string(),
                raw.get("tool_input").cloned().unwrap_or(Value::Null),
            )
        });
        ToolMeta {
            kind: Some(acp_kind_of(str_at(t, "kind"))),
            name,
            mcp_call,
        }
    }

    /// An MCP call's result: `rawOutput` `{type: "MCP", server_name, tool_name, output:
    /// {OkayOutput: "<text>"}}`, whose text is the tool's.
    fn tool_output(&self, u: &Value) -> Option<String> {
        let raw = u.get("rawOutput")?;
        if str_at(raw, "type") != "MCP" {
            return None;
        }
        raw.get("output")?
            .as_object()?
            .values()
            .find_map(Value::as_str)
            .map(str::to_string)
    }

    fn ready_ext(&self, ext: &mut Map<String, Value>) {
        ext.insert("verified_with".into(), json!(VERIFIED_WITH));
        if let Some(tools) = self.agent_profile.as_ref().and_then(|p| p.get("tools")) {
            ext.insert("tools".into(), tools.clone());
        }
        if !self.denied_tools.is_empty() {
            ext.insert("denied_tools".into(), json!(self.denied_tools));
        }
    }

    fn usage_request(&self) -> Option<(&'static str, Value)> {
        self.usage.then(|| (BILLING_METHOD, json!({})))
    }

    fn usage_windows(&self, result: &Value) -> (Vec<UsageWindow>, Option<String>) {
        billing_windows(result)
    }

    fn auth_hint(&self) -> &str {
        " (sign in with `grok login`, or set XAI_API_KEY)"
    }
}

/// The ACP tool kind for grok's own tool kind (`_meta."x.ai/tool".kind`).
fn acp_kind_of(xkind: &str) -> &'static str {
    match xkind {
        "execute" => "execute",
        "edit" | "write" => "edit",
        "read" => "read",
        "search" | "grep" => "search",
        "delete" => "delete",
        "move" => "move",
        "fetch" => "fetch",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modes(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mode_acceptance() {
        let ok = |report: &str, requested: &str| mode_acceptable(report, requested);
        // Known modes: the same level or narrower.
        assert!(!ok("always-approve", "auto"));
        assert!(!ok("auto", "acceptEdits"));
        assert!(!ok("acceptEdits", "default"));
        assert!(!ok("default", "plan"));
        assert!(ok("ask", "default"));
        assert!(ok("default", "manual"));
        assert!(ok("autoEdit", "acceptEdits"));
        assert!(ok("plan", "dontAsk"));
        assert!(ok("default", "auto"));
        // A bypass mode only when one was requested; then anything goes.
        assert!(ok("yolo", "bypassPermissions"));
        assert!(ok("made-up", "bypassPermissions"));
        assert!(!ok("yolo", "manual"));
        // Unknown modes are only fine when they are exactly what was asked for; a known
        // restricted mode is narrower than any requested one.
        assert!(!ok("careful", "default"));
        assert!(ok("careful", "careful"));
        assert!(!ok("default", "careful"));
        assert!(ok("plan", "careful"));
        assert!(!ok("yolo", "careful"));
        assert!(!ok("yolo", "always-ask"));
        assert!(!ok("always-ask", "default"));
        // Nothing requested: anything but a known bypass mode.
        assert!(ok("careful", ""));
        assert!(ok("auto", ""));
        assert!(!ok("yolo", ""));
        assert!(!ok("Always Approve", ""));
        assert!(!ok("", ""));
        assert_eq!(mode_level("yolo"), Some(ModeLevel::Bypass));
        assert_eq!(mode_level("readOnly"), Some(ModeLevel::Restricted));
        assert_eq!(mode_level("nope"), None);
    }

    fn opened(mode: &str) -> Translator {
        let mut tr = Translator::new(TranslatorOptions {
            cwd: "/w".into(),
            mode: mode.into(),
            ..Default::default()
        });
        tr.start();
        tr.translate(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        );
        tr
    }

    fn session(modes: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"s","modes":{modes}}}}}"#)
    }

    /// A failed `session/set_mode` never lets a queued prompt run in the wider mode.
    #[test]
    fn failed_set_mode_fails_the_session() {
        let mut tr = opened("default");
        assert!(tr.send("task", 1).writes.is_empty());
        let out = tr.translate(&session(
            r#"{"currentModeId":"always-approve","availableModes":[{"id":"ask"},{"id":"always-approve"}]}"#,
        ));
        assert!(out.writes[0].contains("session/set_mode"));
        let out = tr.translate(
            r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}"#,
        );
        assert!(
            out.fatal.as_deref().unwrap().contains("always-approve"),
            "{out:?}"
        );
        assert!(
            !out.writes.iter().any(|w| w.contains("session/prompt")),
            "{out:?}"
        );
        assert!(!out
            .events
            .iter()
            .any(|e| matches!(e.body, EventBody::ThreadStarted(_))));
        assert!(out
            .events
            .iter()
            .any(|e| matches!(&e.body, EventBody::TurnEnded(t) if t.result == TurnResult::Error)));
        assert!(!tr.ready());
        assert!(tr.send("again", 2).writes.is_empty());
    }

    /// A session wider than the requested mode with nothing narrower to switch to fails;
    /// one narrower (or equal) runs.
    #[test]
    fn a_wider_session_with_no_narrower_mode_fails() {
        let mut tr = opened("acceptEdits");
        tr.send("task", 1);
        let out = tr.translate(&session(
            r#"{"currentModeId":"auto","availableModes":[{"id":"auto"},{"id":"yolo"}]}"#,
        ));
        assert!(out.fatal.is_some(), "{out:?}");
        assert!(out.writes.is_empty(), "{out:?}");
        // A narrower session is left alone.
        let mut tr = opened("auto");
        tr.send("task", 1);
        let out = tr.translate(&session(
            r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"plan"}]}"#,
        ));
        assert!(out.fatal.is_none());
        assert!(out.writes[0].contains("session/prompt"), "{out:?}");
        // Setting a wider requested mode may fail: the narrower session still runs.
        let mut tr = opened("auto");
        tr.send("task", 1);
        let out = tr.translate(&session(
            r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"auto"}]}"#,
        ));
        assert!(out.writes[0].contains("session/set_mode"));
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-1,"message":"no"}}"#);
        assert!(out.fatal.is_none());
        assert!(out.writes[0].contains("session/prompt"), "{out:?}");
    }

    /// A session that widens its own mode during a prompt fails at once, the prompt cancelled
    /// (switching back would leave the prompt running in the wider mode until grok answered);
    /// one that widens it between prompts is switched back before the next prompt, and fails
    /// when that fails.
    #[test]
    fn a_later_widening_fails_the_prompt_or_is_switched_back() {
        let widen = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"current_mode_update","currentModeId":"always-approve"}}}"#;
        let modes = r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"always-approve"}]}"#;
        let mut tr = opened("default");
        tr.translate(&session(modes));
        assert!(tr.ready());
        tr.send("one", 1);
        let out = tr.translate(widen);
        assert!(out.fatal.is_some(), "{out:?}");
        assert!(out.writes[0].contains("session/cancel"), "{out:?}");
        assert!(!out.writes.iter().any(|w| w.contains("session/set_mode")));

        let mut tr = opened("default");
        tr.translate(&session(modes));
        tr.send("one", 1);
        tr.translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
        let out = tr.translate(widen);
        assert!(out.fatal.is_none(), "{out:?}");
        assert!(out.writes[0].contains("\"modeId\":\"default\""), "{out:?}");
        // Prompt 2 waits for the mode to be back.
        let out = tr.send("two", 2);
        assert!(
            !out.writes.iter().any(|w| w.contains("session/prompt")),
            "{out:?}"
        );
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
        assert!(out.writes[0].contains("session/prompt"), "{out:?}");
        assert_eq!(tr.current_mode(), Some("default"));
        // Switching back fails: the session ends.
        tr.translate(r#"{"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}"#);
        let out = tr.translate(widen);
        assert!(out.writes[0].contains("session/set_mode"), "{out:?}");
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":6,"error":{"code":-1,"message":"no"}}"#);
        assert!(out.fatal.is_some(), "{out:?}");
    }

    /// ACP results carry the whole configuration. A model change reporting a wider mode is
    /// switched back before any prompt; a mode request whose result still reports it fails
    /// the session without a prompt.
    #[test]
    fn a_wider_mode_in_a_model_result_is_checked() {
        let mut tr = Translator::new(TranslatorOptions {
            cwd: "/w".into(),
            mode: "default".into(),
            model: Some("fast".into()),
            ..Default::default()
        });
        tr.start();
        tr.translate(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        );
        tr.send("task", 1);
        let out = tr.translate(&session(
            r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"yolo"}]}"#,
        ));
        assert!(
            out.writes[0].contains("session/set_config_option"),
            "{out:?}"
        );
        let out = tr.translate(
            r#"{"jsonrpc":"2.0","id":3,"result":{"configOptions":[{"id":"model","category":"model","currentValue":"fast"}],"modes":{"currentModeId":"yolo"}}}"#,
        );
        assert_eq!(tr.current_mode(), Some("yolo"));
        assert_eq!(out.writes.len(), 1, "{out:?}");
        assert!(out.writes[0].contains("\"modeId\":\"default\""), "{out:?}");
        assert!(!tr.ready());
        // The mode request "succeeds", but its result still reports yolo.
        let out =
            tr.translate(r#"{"jsonrpc":"2.0","id":4,"result":{"modes":{"currentModeId":"yolo"}}}"#);
        assert!(out.fatal.as_deref().unwrap().contains("yolo"), "{out:?}");
        assert!(
            !out.writes.iter().any(|w| w.contains("session/prompt")),
            "{out:?}"
        );
        assert!(!tr.ready());
    }

    /// A configuration reported in a prompt result or a `config_option_update` between prompts
    /// (a mode option, even for a session that reported `modes`) is checked before the next
    /// prompt; a `config_option_update` during a prompt fails the session.
    #[test]
    fn a_wider_mode_reported_later_holds_the_next_prompt() {
        let option = |v: &str| {
            format!(
                r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"config_option_update","configOptions":[{{"id":"mode","category":"mode","currentValue":"{v}","options":[{{"value":"default"}},{{"value":"yolo"}}]}}]}}}}}}"#
            )
        };
        // In a prompt result, while the next prompt is queued.
        {
            let widen = r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn","modes":{"currentModeId":"yolo"}}}"#;
            let mut tr = opened("default");
            tr.translate(&session(
                r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"yolo"}]}"#,
            ));
            tr.send("one", 1);
            tr.send("two", 2);
            let out = tr.translate(widen);
            assert!(
                !out.writes.iter().any(|w| w.contains("session/prompt")),
                "{widen}: {out:?}"
            );
            assert_eq!(tr.current_mode(), Some("yolo"));
            let out = tr.translate(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
            assert!(out.writes[0].contains("session/prompt"), "{widen}: {out:?}");
            assert_eq!(tr.current_mode(), Some("default"));
        }
        // In a `config_option_update` between prompts.
        let mut tr = opened("default");
        tr.translate(&session(
            r#"{"currentModeId":"default","availableModes":[{"id":"default"},{"id":"yolo"}]}"#,
        ));
        tr.send("one", 1);
        tr.translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
        let out = tr.translate(&option("yolo"));
        assert!(
            out.writes[0].contains("session/set_config_option"),
            "{out:?}"
        );
        let out = tr.send("two", 2);
        assert!(
            !out.writes.iter().any(|w| w.contains("session/prompt")),
            "{out:?}"
        );
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":4,"result":{}}"#);
        assert!(out.writes[0].contains("session/prompt"), "{out:?}");
        // The same during a prompt: it is cancelled and the session fails.
        let out = tr.translate(&option("yolo"));
        assert!(out.fatal.is_some(), "{out:?}");
        assert!(out.writes[0].contains("session/cancel"), "{out:?}");
    }

    /// grok 1.0.44 reports always-approve only as `yolo` in `_x.ai/sessions/changed`: when
    /// twr did not ask for it the running prompt is cancelled and the session fails.
    #[test]
    fn a_yolo_session_is_cancelled_and_failed() {
        let changed = |yolo: bool| {
            format!(
                r#"{{"jsonrpc":"2.0","method":"_x.ai/sessions/changed","params":{{"upserted":[{{"sessionId":"other","yolo":true}},{{"sessionId":"s","yolo":{yolo},"activity":"working"}}],"removed":[]}}}}"#
            )
        };
        let mut tr = opened("default");
        tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
        assert!(tr.ready());
        tr.send("go", 1);
        // Another session's yolo, and ours off: nothing happens.
        let out = tr.translate(&changed(false));
        assert!(out.fatal.is_none() && out.writes.is_empty(), "{out:?}");
        assert_eq!(tr.yolo(), Some(false));
        let out = tr.translate(&changed(true));
        assert!(out.fatal.as_deref().unwrap().contains("yolo"), "{out:?}");
        assert!(out.writes[0].contains("session/cancel"), "{out:?}");
        let ended: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::TurnEnded(t) => Some((e.turn, t.result)),
                _ => None,
            })
            .collect();
        assert_eq!(ended, [(Some(1), TurnResult::Error)]);
        // The real response does not end the turn again.
        assert!(tr
            .translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"cancelled"}}"#)
            .events
            .is_empty());
        // Bypass asked for: yolo is expected.
        let mut tr = opened("bypassPermissions");
        tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
        tr.send("go", 1);
        assert!(tr.translate(&changed(true)).fatal.is_none());
    }

    /// An unknown model fails the session before any prompt runs.
    #[test]
    fn a_rejected_model_fails_the_session() {
        let mut tr = Translator::new(TranslatorOptions {
            cwd: "/w".into(),
            model: Some("nope".into()),
            ..Default::default()
        });
        tr.start();
        tr.send("task", 1);
        tr.translate(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        );
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
        assert_eq!(
            serde_json::from_str::<Value>(&out.writes[0]).unwrap()["params"],
            json!({"sessionId": "s", "configId": "model", "value": "nope"})
        );
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32602,"message":"Invalid params","data":"unknown model id"}}"#);
        let fatal = out.fatal.unwrap();
        assert!(
            fatal.contains("\"nope\"") && fatal.contains("unknown model id"),
            "{fatal}"
        );
        assert!(!tr.ready());
    }

    /// The configuration grok 1.0.44 reports, with effort `current` (recorded 2026-10-03).
    fn grok_config(current: &str) -> String {
        format!(
            r#"{{"configOptions":[{{"id":"model","name":"Model","category":"model","type":"select","currentValue":"grok-4.7","options":[{{"value":"grok-4.7"}},{{"value":"grok-4.7-build-fast"}}]}},{{"id":"reasoning_effort","name":"Reasoning Effort","category":"thought_level","type":"select","currentValue":"{current}","options":[{{"value":"xhigh"}},{{"value":"high"}},{{"value":"medium"}},{{"value":"low"}}]}}]}}"#
        )
    }

    /// The effort is set after the model, on the `reasoning_effort` option the latest
    /// configuration reports; the session becomes ready with it (`ext.grok.effort`), and a
    /// value grok refuses fails the session before any prompt.
    #[test]
    fn the_effort_is_set_after_the_model() {
        let open = |effort: &str| {
            let mut tr = Translator::new(TranslatorOptions {
                cwd: "/w".into(),
                model: Some("grok-4.7".into()),
                effort: Some(effort.into()),
                ..Default::default()
            });
            tr.start();
            tr.send("task", 1);
            tr.translate(
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
            );
            let session = grok_config("high").replacen('{', r#"{"sessionId":"s","#, 1);
            let out = tr.translate(&format!(r#"{{"jsonrpc":"2.0","id":2,"result":{session}}}"#));
            let model: Value = serde_json::from_str(&out.writes[0]).unwrap();
            assert_eq!(model["params"]["configId"], "model");
            let out = tr.translate(&format!(
                r#"{{"jsonrpc":"2.0","id":3,"result":{}}}"#,
                grok_config("high")
            ));
            let req: Value = serde_json::from_str(&out.writes[0]).unwrap();
            assert_eq!(req["method"], "session/set_config_option");
            assert_eq!(
                req["params"],
                json!({"sessionId": "s", "configId": "reasoning_effort", "value": effort})
            );
            assert!(!tr.ready());
            tr
        };
        let mut tr = open("low");
        let out = tr.translate(&format!(
            r#"{{"jsonrpc":"2.0","id":4,"result":{}}}"#,
            grok_config("low")
        ));
        assert!(tr.ready(), "{out:?}");
        let started = out
            .events
            .iter()
            .find_map(|e| match &e.body {
                twr_core::event::EventBody::ThreadStarted(p) => p.ext.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(started["grok"]["effort"], "low");
        assert_eq!(started["grok"]["model"], "grok-4.7");
        assert!(out.writes.iter().any(|w| w.contains("session/prompt")));

        let mut tr = open("max");
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":4,"error":{"code":-32602,"message":"Invalid params","data":"unknown reasoning_effort value"}}"#);
        let fatal = out.fatal.unwrap();
        assert!(
            fatal.contains("reasoning effort \"max\"")
                && fatal.contains("unknown reasoning_effort value"),
            "{fatal}"
        );
        assert!(!tr.ready());
    }

    /// A session that reports no effort option cannot run with an effort: it fails.
    #[test]
    fn an_effort_without_an_effort_option_fails_the_session() {
        let mut tr = Translator::new(TranslatorOptions {
            cwd: "/w".into(),
            effort: Some("high".into()),
            ..Default::default()
        });
        tr.start();
        tr.translate(
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
        );
        let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
        let fatal = out.fatal.unwrap();
        assert!(fatal.contains("thought_level"), "{fatal}");
        assert!(!tr.ready());
    }

    /// An unreadable prompt response still ends the turn; tool ids are
    /// per prompt.
    #[test]
    fn oversized_prompt_response_and_pruned_tools() {
        let mut tr = opened("default");
        tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
        tr.send("go", 1);
        tr.translate(r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"ls","kind":"read"}}}"#);
        assert_eq!(tr.tools.len(), 1);
        assert!(tr
            .oversized(r#"{"jsonrpc":"2.0","id":99,"result""#, 1)
            .events
            .is_empty());
        let out = tr.oversized(
            r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason""#,
            40_000_000,
        );
        let ended: Vec<_> = out
            .events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::TurnEnded(t) => Some((e.turn, t.result, t.ext.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(ended.len(), 1);
        assert_eq!((ended[0].0, ended[0].1), (Some(1), TurnResult::Error));
        assert_eq!(
            ended[0].2.as_ref().unwrap()["twr"]["error"],
            OVERSIZED_ERROR
        );
        assert!(tr.tools.is_empty());
        // The real response, should it come, ends nothing twice.
        assert!(tr
            .translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#)
            .events
            .is_empty());
    }

    #[test]
    fn bypass_modes() {
        for m in [
            "always-approve",
            "bypassPermissions",
            "yolo",
            "Always Approve",
            "full-access",
            "danger-full-access",
        ] {
            assert!(is_bypass(m), "{m}");
        }
        // Whole ids only: no substring matching (`always-ask` is not a bypass mode).
        for m in [
            "default",
            "ask",
            "auto",
            "plan",
            "acceptEdits",
            "",
            "always-ask",
            "approve",
            "bypass-nothing",
            "yolo-not",
        ] {
            assert!(!is_bypass(m), "{m}");
        }
    }

    #[test]
    fn mode_selection() {
        let avail = modes(&["default", "auto", "always-approve"]);
        assert_eq!(narrowing_target("", &avail).as_deref(), Some("default"));
        assert_eq!(narrowing_target("auto", &avail).as_deref(), Some("auto"));
        // A twr mode the agent does not offer: the widest offered one that is accepted for it
        // (auto is wider than acceptEdits).
        assert_eq!(
            narrowing_target("acceptEdits", &avail).as_deref(),
            Some("default")
        );
        assert_eq!(
            narrowing_target(
                "manual",
                &modes(&["plan", "default", "auto2", "always-approve"])
            )
            .as_deref(),
            Some("default")
        );
        let ask = modes(&["ask", "yolo"]);
        assert_eq!(narrowing_target("", &ask).as_deref(), Some("ask"));
        // A mode twr does not know is never chosen: nothing to switch to (the caller refuses).
        let odd = modes(&["yolo", "careful"]);
        assert_eq!(narrowing_target("", &odd), None);
        // ... unless it is exactly the requested one.
        assert_eq!(
            narrowing_target("careful", &odd).as_deref(),
            Some("careful")
        );
        // An unknown requested mode not offered: only a restricted mode fits.
        assert_eq!(narrowing_target("always-ask", &odd), None);
        assert_eq!(
            narrowing_target("always-ask", &modes(&["default", "plan", "yolo"])).as_deref(),
            Some("plan")
        );
        // Plan requested but not offered: default is wider, so nothing fits.
        assert_eq!(narrowing_target("plan", &avail), None);
        assert_eq!(narrowing_target("", &[]), None);
    }

    #[test]
    fn error_texts() {
        let e = json!({"code": -32000, "message": "Authentication required", "data": "no auth method id provided"});
        assert_eq!(
            error_text(&e),
            "Authentication required (no auth method id provided)"
        );
        assert!(transient_reason(&json!({"message": "Rate limit exceeded"})).is_some());
        assert!(transient_reason(&e).is_none());
    }
}

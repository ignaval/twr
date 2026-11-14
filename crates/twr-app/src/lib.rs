//! A presentation-independent projection of twr's existing client fold. The desktop host
//! uses this over stdio; neither the web renderer nor Electron reimplements hub reducers.
use serde_json::{json, Value};
use std::collections::HashSet;
use twr_core::{client_state::ClientFold, config::CardOrder, reduce::stack_order};
pub mod runtime;

#[derive(Default)]
pub struct Presentation {
    pub pins: HashSet<String>,
    pub held: bool,
    pub retry_requests: HashSet<String>,
    pub incomplete: HashSet<String>,
    pub card_order: CardOrder,
    shown: Option<twr_core::card_order::Shown>,
}
impl Presentation {
    pub fn project(&mut self, fold: &ClientFold) -> Value {
        self.incomplete = fold.capped.keys().cloned().collect();
        let target: Vec<_> =
            stack_order(&fold.registry, &fold.attention, &self.pins, self.card_order)
                .into_iter()
                .map(|id| twr_core::card_order::Card {
                    items: fold
                        .attention
                        .for_thread(&id, &self.pins)
                        .iter()
                        .map(|a| a.attn_id.clone())
                        .collect(),
                    parked: fold
                        .registry
                        .get(&id)
                        .is_some_and(|t| t.status().as_str() == "parked"),
                    id,
                })
                .collect();
        let shown = twr_core::card_order::settle(
            &target,
            (self.card_order == CardOrder::Stable)
                .then_some(self.shown.as_ref())
                .flatten(),
            std::time::Instant::now(),
            self.held,
        );
        let mut result = view(fold);
        result["went_on"] = json!(fold.went_on);
        let threads = result["threads"].as_array_mut().expect("thread array");
        threads.sort_by_key(|t| {
            shown
                .layout
                .order
                .iter()
                .position(|id| Some(id.as_str()) == t["id"].as_str())
                .unwrap_or(usize::MAX)
        });
        self.shown = Some(shown);
        result
    }
}

pub fn view(fold: &ClientFold) -> Value {
    let pins = HashSet::new();
    let threads: Vec<Value> =
        stack_order(&fold.registry, &fold.attention, &pins, CardOrder::Stable)
            .iter()
            .filter_map(|id| fold.registry.get(id))
            .filter(|t| !t.retired)
            .map(|t| {
                let mut value = serde_json::to_value(t).expect("serializable thread");
                value["status"] = json!(t.status().as_str());
                value["status_label"] = json!(t.status_label());
                value["is_orchestrator"] = json!(t.is_current_orchestrator());
                let color = t.color.unwrap_or(0) as usize % twr_core::palette::THREAD_PALETTE.len();
                let (r, g, b) =
                    twr_core::palette::cube_rgb(twr_core::palette::THREAD_PALETTE[color]);
                value["accent"] = json!(format!("#{r:02x}{g:02x}{b:02x}"));
                // A running loop: its badge and line, as every client draws them.
                if let Some(l) = &t.r#loop {
                    value["loop_badge"] = json!(twr_core::loops::badge(l.kind));
                    value["loop_line"] = json!(twr_core::loops::line(l));
                }
                value
            })
            .collect();
    let attention: Vec<Value> = fold
        .attention
        .ordered(&pins)
        .into_iter()
        .map(|a| {
            let mut value = serde_json::to_value(a).expect("serializable attention");
            value["incomplete"] = json!(fold.capped.contains_key(&a.attn_id));
            value
        })
        .collect();
    json!({"threads": threads, "attention": attention,
        "decisions": fold.decisions, "decisions_seen": fold.decisions_seen,
        "head": fold.last_gseq, "usage": fold.usage})
}

/// Safe display metadata for conversation updates, using the same classifier as the TUI.
pub fn present_event(value: &mut Value) {
    if value["kind"] != "conversation.update" {
        return;
    }
    let data = &value["data"];
    let ext = serde_json::from_value(data["ext"].clone()).unwrap_or(None);
    let item = data["ext"]["twr"]["attn_id"].as_str().is_some()
        || data["ext"]["attn_id"].as_str().is_some();
    let routine = !item
        && data["user_message"].as_str().is_none()
        && twr_core::routine::Routine::of(&ext, data["text"].as_str().unwrap_or("")).is_some();
    let twr = &data["ext"]["twr"];
    let stall = data["text"]
        .as_str()
        .and_then(|text| text.split_once(" is stalled: "))
        .is_some_and(|(name, _)| !name.is_empty() && !name.contains(' '))
        && data["user_message"].as_str().is_none()
        && twr["resolved"] != true
        && twr["outcome"].is_null();
    let stall_id = stall
        .then(|| twr["attn_id"].as_str().map(str::to_owned))
        .flatten();
    value["routine"] = json!(routine);
    if let Some(id) = stall_id {
        value["stall"] = json!(id);
    }
}

/// Only user-facing operations. No agent/orchestrator identity, hooks, or attach leases
/// are exposed to the web renderer. Interactive attach is performed by the real CLI.
pub fn allowed_method(method: &str) -> bool {
    matches!(
        method,
        "device.pair_begin"
            | "device.pair_complete"
            | "device.pair_cancel"
            | "device.list"
            | "device.revoke"
            | "hub.info"
            | "config.show"
            | "hub.shutdown"
            | "hub.glance"
            | "say"
            | "suggest.next"
            | "summary.thread"
            | "thread.pull"
            | "thread.new"
            | "thread.spawn"
            | "thread.spawn_status"
            | "thread.list"
            | "thread.get"
            | "thread.activity"
            | "thread.send"
            | "thread.upload"
            | "thread.send_cancel"
            | "thread.stop"
            | "thread.park"
            | "thread.resume"
            | "thread.trim"
            | "thread.compact"
            | "thread.set_mode"
            | "thread.set_model"
            | "thread.rename"
            | "thread.delete"
            | "thread.queue"
            | "thread.events"
            | "thread.transcript"
            | "conversation.search"
            | "thread.event_context"
            | "attention.list"
            | "attention.respond"
            | "attention.dismiss"
            | "attention.annotate"
            | "message.withdraw"
            | "message.send_now"
            | "usage.get"
            | "usage.threads"
            | "orchestrator.show"
            | "orchestrator.use"
            | "orchestrator.reset"
            | "orchestrator.watch"
            | "orchestrator.unwatch"
            | "orchestrator.watches"
            | "context.read"
            | "context.set"
            | "memory.list"
            | "memory.read"
            | "memory.save"
            | "memory.forget"
            | "job.start"
            | "job.list"
            | "job.log"
            | "job.cancel"
            | "shell.run"
            | "shell.cd"
            | "trash.list"
            | "trash.restore"
            | "trash.empty"
            | "log.stats"
            | "log.compact"
            | "account.list"
            | "account.status"
            | "account.add"
            | "account.remove"
            | "box.agents"
            | "box.projects"
            | "box.install"
            | "box.restart"
            | "box.token"
            | "runner.manage"
            | "runner.status"
            | "runner.connect"
            | "runner.recover"
            | "runner.revoke"
            | "runner.forget"
            | "box.add"
            | "speech.request"
            | "speech.control"
            | "speech.status"
            | "decisions.list"
            | "control.read"
            | "blob.get"
            | "notify.looking"
            | "loop.request"
            | "loop.setup.get"
            | "loop.setup.update"
            | "loop.settings.get"
            | "loop.settings.set"
            | "loop.settings.reset"
            | "loop.get"
            | "loop.report"
            | "loop.list"
            | "loop.control"
    )
}

pub fn validate_call(method: &str, params: &Value) -> anyhow::Result<()> {
    anyhow::ensure!(
        allowed_method(method),
        "Unsupported desktop operation: {method}"
    );
    if matches!(
        method,
        "attention.respond" | "attention.dismiss" | "loop.setup.update"
    ) {
        anyhow::ensure!(
            params["revision"].as_u64().is_some_and(|r| r > 0),
            "Review the current request before answering it (a revision is required)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Every loop method goes through the desktop, and the setup card's change needs the
    /// revision it was made on, as an answer does.
    #[test]
    fn loop_methods_are_allowed() {
        for m in twr_core::devices::METHOD_SCOPES
            .iter()
            .flat_map(|(_, m)| m.iter())
            .filter(|m| m.starts_with("loop."))
        {
            assert!(allowed_method(m), "{m}");
        }
        assert!(validate_call("loop.setup.update", &json!({"attn_id": "a"})).is_err());
        assert!(
            validate_call("loop.setup.update", &json!({"attn_id": "a", "revision": 3})).is_ok()
        );
    }
    /// A thread in a loop is presented with its loop's badge and line; one without has neither.
    #[test]
    fn presented_threads_carry_the_loop_line() {
        use twr_core::event::{EventBody, LoopStarted};
        use twr_core::loops::config::Profile;
        let mut fold = ClientFold::default();
        let mut api = twr_core::testkit::ThreadScript::new("t1");
        let mut web = twr_core::testkit::ThreadScript::new("t2");
        for ev in [
            api.created("api"),
            web.created("web"),
            api.event(EventBody::LoopStarted(Box::new(LoopStarted {
                id: "l1".into(),
                profile: Profile::builtin("low").unwrap(),
                ..Default::default()
            }))),
        ] {
            fold.apply(&ev, None);
        }
        let out = Presentation::default().project(&fold);
        let thread = |name: &str| {
            out["threads"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .cloned()
                .unwrap()
        };
        assert_eq!(thread("api")["loop_badge"], "REVIEW LOOP");
        assert_eq!(thread("api")["loop_line"], "low · scope");
        assert!(thread("web").get("loop_badge").is_none());
        assert!(thread("web").get("loop_line").is_none());
    }
    #[test]
    fn snapshot_projection_keeps_stalls_resolved_while_the_app_was_closed() {
        let mut fold = ClientFold::default();
        fold.went_on.insert("resolved-stall".into());
        let snapshot: ClientFold =
            serde_json::from_value(serde_json::to_value(&fold).unwrap()).unwrap();
        let mut presentation = Presentation::default();
        assert_eq!(
            presentation.project(&snapshot)["went_on"],
            json!(["resolved-stall"])
        );
    }
    #[test]
    fn request_and_user_message_updates_are_never_routine() {
        let mut value = json!({"kind":"conversation.update","data":{"text":"api background work finished.","about_thread":"api","ext":{"twr":{"routine":"interim","attn_id":"permission"}}}});
        present_event(&mut value);
        assert_eq!(value["routine"], false);
        value["data"]["ext"]["twr"]
            .as_object_mut()
            .unwrap()
            .remove("attn_id");
        present_event(&mut value);
        assert_eq!(value["routine"], true);
        value["data"]["user_message"] = json!("Do this");
        present_event(&mut value);
        assert_eq!(value["routine"], false);
        value["data"]["user_message"] = Value::Null;
        value["data"]["ext"]["twr"]["routine"] = Value::Null;
        present_event(&mut value);
        assert_eq!(value["routine"], false);
    }
    #[test]
    fn approvals_require_the_revision_shown_and_no_identity_operations() {
        assert!(validate_call(
            "attention.respond",
            &json!({"attn_id":"a","answer":"allow"})
        )
        .is_err());
        assert!(validate_call(
            "attention.respond",
            &json!({"attn_id":"a","revision":42,"answer":"allow"})
        )
        .is_ok());
        for method in ["hello", "attach.begin", "speech.attach", "unknown"] {
            assert!(validate_call(method, &json!({})).is_err());
        }
    }
    #[test]
    fn empty_projection_is_a_valid_disconnected_workspace() {
        let v = view(&ClientFold::default());
        assert_eq!(v["threads"], json!([]));
        assert_eq!(v["attention"], json!([]));
        assert_eq!(v["head"], 0);
    }
}

//! Read-only conversation search and exact event navigation, including archived output.
use super::{internal, rpc, Hub};
use crate::store;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use twr_core::{
    event::HUB_THREAD,
    proto::{codes, RpcError},
    Event,
};

const SCAN: usize = 512;
fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(text) => strings.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_strings(item, strings);
            }
        }
        Value::Object(items) => {
            for item in items.values() {
                collect_strings(item, strings);
            }
        }
        _ => {}
    }
}
fn searchable_text(event: &Event) -> anyhow::Result<String> {
    let data = event.body.data()?;
    let fields: &[&str] = match event.kind() {
        "turn.started" => &["prompt"],
        "agent.text" if data["delta"] == true && data["final"] != true => &[],
        "agent.text" => &["text"],
        "agent.tool_call" => &["tool", "input_summary"],
        "agent.tool_result" => &["output_summary"],
        "conversation.update" => &["text", "user_message"],
        "report.ready" => &["summary"],
        // Search values as plain text, preserving quotes and newlines, without field names.
        "decision.made" => {
            let mut strings = Vec::new();
            collect_strings(&data, &mut strings);
            return Ok(strings.join("\n"));
        }
        _ => &[],
    };
    Ok(fields
        .iter()
        .filter_map(|field| data[*field].as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}
fn excerpt(text: &str, query: &str) -> Option<String> {
    excerpt_window(text, query, 70, 300)
}
fn excerpt_window(text: &str, query: &str, before: usize, limit: usize) -> Option<String> {
    // Find byte offsets in the original string: lowercasing may change Unicode byte lengths.
    let folded = text.to_lowercase();
    let offset = folded.find(query)?;
    let mut folded_bytes = 0;
    let mut original_chars = 0usize;
    for character in text.chars() {
        if folded_bytes >= offset {
            break;
        }
        folded_bytes += character.to_lowercase().map(char::len_utf8).sum::<usize>();
        original_chars += 1;
    }
    let start = original_chars.saturating_sub(before);
    let body: String = text.chars().skip(start).take(limit).collect();
    Some(format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if text.chars().count() > start + limit {
            "…"
        } else {
            ""
        }
    ))
}
fn bounded_json(value: &mut Value, budget: &mut usize) {
    match value {
        Value::String(text) => {
            if text.len() > *budget {
                let mut end = (*budget).min(text.len());
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str("… [display truncated]");
                *budget = 0;
            } else {
                *budget -= text.len();
            }
        }
        Value::Array(values) => {
            let mut keep = 0;
            for item in values.iter_mut() {
                if *budget < 16 {
                    break;
                }
                *budget -= 16;
                bounded_json(item, budget);
                keep += 1;
            }
            if keep < values.len() {
                values.truncate(keep);
                values.push(Value::String("… [display truncated]".into()));
            }
        }
        Value::Object(values) => {
            let keys: Vec<_> = values.keys().cloned().collect();
            let mut truncated = false;
            for key in keys {
                if *budget < key.len() + 16 {
                    values.remove(&key);
                    truncated = true;
                    continue;
                }
                *budget -= key.len() + 16;
                bounded_json(values.get_mut(&key).unwrap(), budget);
            }
            if truncated {
                values.insert("_display_truncated".into(), Value::Bool(true));
            }
        }
        _ => {
            *budget = budget.saturating_sub(32);
        }
    }
}

impl Hub {
    fn search_thread_name(&self, thread: &str) -> String {
        let name = self.inner.core().thread_name(thread);
        if name != thread {
            return name;
        }
        store::latest_thread_name(&self.inner.reader(), thread)
            .ok()
            .flatten()
            .unwrap_or(name)
    }
    fn search_thread(&self, query: &str) -> Result<String, RpcError> {
        if query == HUB_THREAD {
            return Ok(query.to_owned());
        }
        let core = self.inner.core();
        if core.registry.is_deleted(query) {
            Ok(query.to_owned())
        } else {
            core.resolve(query)
        }
    }
    fn hydrate_search_event(&self, event: Event, archived: bool) -> Event {
        let full = if archived {
            self.inner
                .archive_reader()
                .as_ref()
                .and_then(|conn| super::append::hydrated_from_store(conn, &event))
        } else {
            super::append::hydrated_from_store(&self.inner.reader(), &event)
        };
        // A compaction may move the blob after candidates were read from the hot store.
        full.or_else(|| {
            if archived {
                super::append::hydrated_from_store(&self.inner.reader(), &event)
            } else {
                self.inner
                    .archive_reader()
                    .as_ref()
                    .and_then(|conn| super::append::hydrated_from_store(conn, &event))
            }
        })
        .unwrap_or(event)
    }
    pub fn conversation_search(
        &self,
        query: &str,
        thread: Option<&str>,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Value, RpcError> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || query.len() > 2048 {
            return Err(rpc(codes::BAD_REQUEST, "query must contain 1–2048 bytes"));
        }
        if before.is_some_and(|cursor| cursor.is_empty() || cursor.len() > 64) {
            return Err(rpc(codes::BAD_REQUEST, "invalid search cursor"));
        }
        let thread = thread
            .map(|thread| self.search_thread(thread))
            .transpose()?;
        let before = before.unwrap_or("~~~~~~~~~~~~~~~~~~~~~~~~~~");
        let hot =
            store::search_candidates(&self.inner.reader(), thread.as_deref(), before, SCAN + 1)
                .map_err(internal)?;
        let old = self
            .inner
            .archive_reader()
            .as_ref()
            .map(|conn| store::search_candidates(conn, thread.as_deref(), before, SCAN + 1))
            .transpose()
            .map_err(internal)?
            .unwrap_or_default();
        let mut rows: HashMap<String, (Event, bool)> = old
            .into_iter()
            .map(|event| (event.id.clone(), (event, true)))
            .collect();
        rows.extend(
            hot.into_iter()
                .map(|event| (event.id.clone(), (event, false))),
        );
        let mut rows: Vec<_> = rows.into_values().collect();
        rows.sort_by(|a, b| b.0.id.cmp(&a.0.id));
        let available = rows.len();
        let mut items = Vec::new();
        let mut next = None;
        let mut scanned = 0;
        let mut scanned_bytes = 0usize;
        for (event, archived) in rows.into_iter().take(SCAN) {
            scanned += 1;
            next = Some(event.id.clone());
            let event = self.hydrate_search_event(event, archived);
            let text = searchable_text(&event).map_err(internal)?;
            scanned_bytes = scanned_bytes.saturating_add(text.len());
            if let Some(excerpt) = excerpt(&text, &query) {
                items.push(json!({"event_id":event.id,"thread":event.thread,"seq":event.seq,"kind":event.kind(),"ts":event.ts,"excerpt":excerpt,"thread_name":self.search_thread_name(&event.thread)}));
                if items.len() >= limit.clamp(1, 100) {
                    break;
                }
            }
            if scanned_bytes >= 32 * 1024 * 1024 {
                break;
            }
        }
        let more = scanned < available;
        Ok(json!({"items":items,"next":next,"more":more,"workspace":self.workspace()}))
    }
    pub fn thread_event_context(
        &self,
        thread: &str,
        event_id: &str,
        before: usize,
        after: usize,
    ) -> Result<Value, RpcError> {
        self.thread_event_context_matching(thread, event_id, before, after, None)
    }
    pub fn thread_event_context_matching(
        &self,
        thread: &str,
        event_id: &str,
        before: usize,
        after: usize,
        query: Option<&str>,
    ) -> Result<Value, RpcError> {
        if query.is_some_and(|query| query.len() > 2048) {
            return Err(rpc(
                codes::BAD_REQUEST,
                "query must contain at most 2048 bytes",
            ));
        }
        let thread = self.search_thread(thread)?;
        let hot = store::event_by_id(&self.inner.reader(), &thread, event_id).map_err(internal)?;
        let (selected, archived) = match hot {
            Some(event) => (event, false),
            None => (
                self.inner
                    .archive_reader()
                    .as_ref()
                    .map(|conn| store::event_by_id(conn, &thread, event_id))
                    .transpose()
                    .map_err(internal)?
                    .flatten()
                    .ok_or_else(|| rpc(codes::NOT_FOUND, "event not found in this thread"))?,
                true,
            ),
        };
        let before = before.min(30);
        let after = after.min(30);
        let mut events = Vec::new();
        for (previous, limit) in [(true, before), (false, after)] {
            let hot = store::event_neighbors(
                &self.inner.reader(),
                &thread,
                selected.seq,
                previous,
                limit,
            )
            .map_err(internal)?;
            let old = self
                .inner
                .archive_reader()
                .as_ref()
                .map(|conn| store::event_neighbors(conn, &thread, selected.seq, previous, limit))
                .transpose()
                .map_err(internal)?
                .unwrap_or_default();
            let mut seen: HashSet<u64> = hot.iter().map(|event| event.seq).collect();
            let mut rows: Vec<_> = hot.into_iter().map(|event| (event, false)).collect();
            rows.extend(
                old.into_iter()
                    .filter(|event| seen.insert(event.seq))
                    .map(|event| (event, true)),
            );
            rows.sort_by_key(|(event, _)| {
                if previous {
                    u64::MAX - event.seq
                } else {
                    event.seq
                }
            });
            events.extend(rows.into_iter().take(limit));
        }
        events.push((selected.clone(), archived));
        events.sort_by_key(|(event, _)| event.seq);
        let events: Vec<_> = events
            .into_iter()
            .map(|(event, archived)| {
                let chosen = event.id == selected.id;
                let event = self.hydrate_search_event(event, archived);
                let mut value = serde_json::to_value(event).map_err(internal)?;
                if let Some(data) = value["data"].as_object_mut() {
                    data.retain(|key, _| key != "ext" && !key.ends_with("_ref"));
                    if chosen {
                        if let Some(query) = query
                            .filter(|query| !query.trim().is_empty())
                            .map(|query| query.trim().to_lowercase())
                        {
                            for field in [
                                "text",
                                "prompt",
                                "user_message",
                                "output_summary",
                                "input_summary",
                                "summary",
                            ] {
                                if let Some(text) = data
                                    .get(field)
                                    .and_then(Value::as_str)
                                    .filter(|text| text.len() > 128 * 1024)
                                {
                                    if let Some(window) =
                                        excerpt_window(text, &query, 8 * 1024, 32 * 1024)
                                    {
                                        data.insert(field.into(), Value::String(window));
                                        data.insert("_display_truncated".into(), Value::Bool(true));
                                    }
                                }
                            }
                        }
                    }
                }
                let mut budget = if chosen { 256 * 1024 } else { 16 * 1024 };
                bounded_json(&mut value["data"], &mut budget);
                Ok(value)
            })
            .collect::<Result<_, RpcError>>()?;
        let event = events
            .iter()
            .find(|event| event["id"] == selected.id)
            .cloned()
            .unwrap();
        Ok(json!({"thread":thread,"workspace":self.workspace(),"event":event,"events":events}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn search_ignores_stream_deltas_and_finds_user_command_text() {
        use twr_core::{config::Config, event::*, Source};
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(super::super::HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        for (delta, is_final) in [(true, false), (false, true)] {
            hub.append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AgentText(AgentText {
                    text: "needle streamed".into(),
                    delta,
                    is_final,
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        let hits = hub.conversation_search("needle", None, None, 30).unwrap();
        assert_eq!(hits["items"].as_array().unwrap().len(), 1);
        hub.append(
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::ConversationUpdate(ConversationUpdate {
                user_message: Some("unique user command".into()),
                text: "Applied".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert_eq!(
            hub.conversation_search("unique user", None, None, 30)
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let value = json!({"provider":"quoted \"value\"\nnext line"});
        let mut text = Vec::new();
        collect_strings(&value, &mut text);
        assert!(!text.join(" ").contains("provider"));
    }
    #[tokio::test]
    async fn searches_hydrated_messages_tools_decisions_with_stable_pages_and_exact_context() {
        use twr_core::{config::Config, event::*, Source};
        let dir = tempfile::tempdir().unwrap();
        let mut options =
            super::super::HubOptions::new(dir.path().join("twr.db"), Config::default());
        options.workspace = Some("review".into());
        let hub = Hub::open(options).await.unwrap();
        let message = hub
            .append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AgentText(AgentText {
                    text: "needle message".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        let tool = hub
            .append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AgentToolResult(AgentToolResult {
                    output_summary: "needle tool output".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        let decision = hub
            .append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::DecisionMade(DecisionMade {
                    chosen: "needle decision".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        let full = "prefix ".repeat(30000) + "overflowneedle";
        let overflow = hub
            .append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AgentText(AgentText {
                    text: full.clone(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        assert!(
            overflow.body.data().unwrap()["text"]
                .as_str()
                .unwrap()
                .len()
                < full.len()
        );
        let found = hub
            .conversation_search("overflowneedle", None, None, 30)
            .unwrap();
        assert_eq!(found["items"][0]["event_id"], overflow.id);
        assert!(found["items"][0]["excerpt"]
            .as_str()
            .unwrap()
            .contains("overflowneedle"));
        let matching = hub
            .thread_event_context_matching(HUB_THREAD, &overflow.id, 0, 0, Some(" OVERFLOWNEEDLE "))
            .unwrap();
        let visible = matching["event"]["data"]["text"].as_str().unwrap();
        assert!(visible.contains("overflowneedle"));
        assert!(visible.len() < 32 * 1024);
        assert_eq!(matching["event"]["data"]["_display_truncated"], true);
        assert!(matching["event"]["data"].get("text_ref").is_none());
        let first = hub
            .conversation_search("needle", Some(HUB_THREAD), None, 2)
            .unwrap();
        assert_eq!(first["workspace"], "review");
        assert_eq!(first["items"].as_array().unwrap().len(), 2);
        assert_eq!(first["items"][1]["event_id"], decision.id);
        assert_eq!(first["more"], true);
        let second = hub
            .conversation_search("needle", Some(HUB_THREAD), first["next"].as_str(), 2)
            .unwrap();
        assert_eq!(second["items"][0]["event_id"], tool.id);
        assert_eq!(second["items"][1]["event_id"], message.id);
        assert_eq!(second["more"], false);
        let context = hub
            .thread_event_context(HUB_THREAD, &tool.id, 1, 1)
            .unwrap();
        assert_eq!(context["event"]["id"], tool.id);
        assert_eq!(
            context["events"]
                .as_array()
                .unwrap()
                .iter()
                .map(|event| event["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![message.id.as_str(), tool.id.as_str(), decision.id.as_str()]
        );
        assert!(hub
            .thread_event_context(HUB_THREAD, "missing", 1, 1)
            .is_err());
        assert!(hub.conversation_search(" ", None, None, 30).is_err());
    }

    #[tokio::test]
    async fn archived_search_deduplicates_and_exact_context_reads_removed_hot_output() {
        use twr_core::{config::Config, event::*, Source};
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(super::super::HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        let event = hub
            .append(
                HUB_THREAD,
                None,
                Source::Hub,
                EventBody::AgentText(AgentText {
                    text: "archived needle".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        let path = dir.path().join("archive.db");
        let archive = crate::archive::open(&path).unwrap();
        archive
            .execute(
                "ATTACH DATABASE ?1 AS hot",
                [dir.path().join("twr.db").to_str().unwrap()],
            )
            .unwrap();
        archive
            .execute(
                "INSERT INTO events SELECT * FROM hot.events WHERE id=?1",
                [&event.id],
            )
            .unwrap();
        archive.execute("DETACH DATABASE hot", []).unwrap();
        *hub.inner.archive_reader.lock().unwrap() = crate::archive::open_reader(&path);
        assert_eq!(
            hub.conversation_search("archived needle", None, None, 10)
                .unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        // Delete only the archived agent event, as a normal archive compaction does.
        hub.inner
            .core()
            .store
            .conn()
            .execute("DELETE FROM events WHERE id=?1", [&event.id])
            .unwrap();
        assert_eq!(
            hub.conversation_search("archived needle", None, None, 10)
                .unwrap()["items"][0]["event_id"],
            event.id
        );
        assert_eq!(
            hub.thread_event_context(HUB_THREAD, &event.id, 0, 0)
                .unwrap()["event"]["id"],
            event.id
        );
    }

    #[test]
    fn search_excerpt_handles_unicode_case_expansion_and_literal_symbols() {
        assert!(excerpt("İ permission → needle", "needle")
            .unwrap()
            .contains("needle"));
        assert!(excerpt("SELECT 100%_safe", "100%_").is_some());
        assert!(excerpt("unrelated", "missing").is_none());
    }
    #[test]
    fn context_display_limits_preserve_event_identity_and_utf8() {
        let mut event = json!({"id":"exact","seq":52,"data":{"text":"界".repeat(100)}});
        bounded_json(&mut event["data"], &mut 27);
        assert_eq!(event["id"], "exact");
        assert_eq!(event["seq"], 52);
        assert!(event["data"]["text"].as_str().unwrap().starts_with("界界"));
        assert!(event["data"]["text"]
            .as_str()
            .unwrap()
            .contains("display truncated"));
    }
}

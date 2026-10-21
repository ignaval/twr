//! `hub.glance`: how the hub is, in one cheap call, for the TUI's workspace sidebar (it asks
//! every running workspace's hub every 5 seconds). Nothing here is recorded: it is a view of
//! the registry, the open attention items and the orchestrator's latest reply as stored.

use std::collections::HashSet;

use serde_json::{json, Value};
use twr_core::event::{AttentionReason, EventBody};
use twr_core::proto::RpcError;
use twr_core::reduce::{ThreadStatus, ThreadView};

use super::{internal, Hub};
use crate::store;

/// Most characters of the orchestrator's latest reply `hub.glance` gives as `preview`.
pub const GLANCE_PREVIEW_CHARS: usize = 300;

impl Hub {
    /// `hub.glance`: `{workspace, threads, running, permissions, questions, active,
    /// orchestrator}`.
    ///
    /// `threads` counts the worker threads (every thread whose role is not `orchestrator`,
    /// as `thread.list` lists them), `running` those of them running or in the background,
    /// `permissions` and `questions` the open items of those reasons (every one, as
    /// `attention.list {all: true}`). `active`: the current orchestrator or a thread that is
    /// not parked is at work ([`working`]). `orchestrator` is null without one, else `{id,
    /// name, status, preview, preview_seq}`: `preview` is its latest final `agent.text` of any
    /// turn as stored (a capped one's preview, no blob read), cut to
    /// [`GLANCE_PREVIEW_CHARS`] characters with nothing added, and `preview_seq` that event's
    /// seq; both null until it replied.
    pub fn glance(&self) -> Result<Value, RpcError> {
        let workspace = self.workspace().to_string();
        let core = self.inner.core();
        let orch = core.orch.thread.as_deref();
        let views = core.registry.list();
        let workers: Vec<&ThreadView> = views
            .iter()
            .copied()
            .filter(|v| !v.is_orchestrator())
            .collect();
        let running = workers
            .iter()
            .filter(|v| matches!(v.status(), ThreadStatus::Running | ThreadStatus::Background))
            .count();
        let items = core.attention.ordered(&HashSet::new());
        let of = |reason: AttentionReason| items.iter().filter(|i| i.reason == reason).count();
        let active = views
            .iter()
            .any(|v| (orch == Some(v.id.as_str()) || !v.parked) && working(v));
        let orchestrator = match orch.and_then(|id| core.registry.get(id)) {
            None => Value::Null,
            Some(v) => {
                let reply = store::latest_final_text(core.store.conn(), &v.id)
                    .map_err(internal)?
                    .and_then(|e| match e.body {
                        EventBody::AgentText(t) => Some((preview(&t.text).to_string(), e.seq)),
                        _ => None,
                    });
                json!({
                    "id": v.id,
                    "name": v.name,
                    "status": v.status().as_str(),
                    "preview": reply.as_ref().map(|r| &r.0),
                    "preview_seq": reply.as_ref().map(|r| r.1),
                })
            }
        };
        Ok(json!({
            "workspace": workspace,
            "threads": workers.len(),
            "running": running,
            "permissions": of(AttentionReason::Permission),
            "questions": of(AttentionReason::Question),
            "active": active,
            "orchestrator": orchestrator,
        }))
    }
}

/// Whether thread `v` is at work: a turn open that waits on no answer of the user's, or
/// background work of its own (a runtime task or a job, not a `!` command of the user's).
fn working(v: &ThreadView) -> bool {
    (v.current_turn.is_some() && !v.is_blocked()) || v.own_background()
}

/// The first [`GLANCE_PREVIEW_CHARS`] characters of `text`.
fn preview(text: &str) -> &str {
    text.char_indices()
        .nth(GLANCE_PREVIEW_CHARS)
        .map_or(text, |(i, _)| &text[..i])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::{twr_ext_map, HubOptions};
    use twr_core::config::Config;
    use twr_core::event::*;
    use twr_core::Source;

    async fn open(dir: &tempfile::TempDir) -> Hub {
        Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap()
    }

    fn created(hub: &Hub, id: &str, name: &str, twr: Value) {
        hub.append(
            id,
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: name.into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ext: twr_ext_map(twr),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    }

    fn append(hub: &Hub, id: &str, turn: Option<u64>, source: Source, body: EventBody) -> Event {
        hub.append(id, turn, source, body, None).unwrap()
    }

    fn adapter() -> Source {
        Source::Adapter("fake".into())
    }

    fn reply(text: &str, is_final: bool) -> EventBody {
        EventBody::AgentText(AgentText {
            text: text.into(),
            is_final,
            delta: !is_final,
            ..Default::default()
        })
    }

    fn raise(attn_id: &str, reason: AttentionReason) -> EventBody {
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: attn_id.into(),
            reason,
            summary: attn_id.into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        })
    }

    fn task(task_id: &str, running: bool, ext: Option<Value>) -> EventBody {
        EventBody::TaskUpdated(TaskUpdated {
            task_id: task_id.into(),
            running,
            description: "cargo test".into(),
            ext: ext.and_then(twr_ext_map),
            ..Default::default()
        })
    }

    /// The counts are those the TUI's tabs made of `thread.list` and `attention.list {all:
    /// true}`: worker threads (retired orchestrators left out with the current one), those
    /// running or in the background, open permissions and questions. A hub without an
    /// orchestrator has none.
    #[tokio::test]
    async fn counts_workers_items_and_no_orchestrator() {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(&dir).await;
        let empty = hub.glance().unwrap();
        assert_eq!(
            empty,
            json!({
                "workspace": "default",
                "threads": 0, "running": 0, "permissions": 0, "questions": 0,
                "active": false,
                "orchestrator": null,
            })
        );
        for (id, name) in [("t1", "api"), ("t2", "web"), ("t3", "docs")] {
            created(&hub, id, name, json!({}));
        }
        let g = hub.glance().unwrap();
        assert_eq!(
            (g["threads"].clone(), g["orchestrator"].clone()),
            (json!(3), Value::Null)
        );
        append(
            &hub,
            "t1",
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted::default()),
        );
        append(
            &hub,
            "t2",
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted::default()),
        );
        append(
            &hub,
            "t2",
            Some(1),
            adapter(),
            raise("p1", AttentionReason::Permission),
        );
        append(
            &hub,
            "t3",
            None,
            Source::Hub,
            raise("q1", AttentionReason::Question),
        );
        append(
            &hub,
            "t3",
            None,
            Source::Hub,
            raise("e1", AttentionReason::Error),
        );
        let g = hub.glance().unwrap();
        // t1 runs; t2 waits on its permission.
        assert_eq!(g["running"], 1, "{g}");
        assert_eq!(
            (g["permissions"].clone(), g["questions"].clone()),
            (json!(1), json!(1))
        );
        // A first orchestrator, then a second one in its place: neither is a worker.
        created(&hub, "o1", "orchestrator", json!({"role": "orchestrator"}));
        created(&hub, "o2", "orchestrator", json!({"role": "orchestrator"}));
        let g = hub.glance().unwrap();
        assert_eq!(g["threads"], 3, "{g}");
        assert_eq!(g["orchestrator"]["id"], "o2", "{g}");
        let listed = hub.list_threads();
        let workers = listed
            .iter()
            .filter(|t| t["role"] != "orchestrator")
            .count();
        assert_eq!(workers, 3);
        hub.shutdown().await;
    }

    /// `active`: a turn open that waits on nobody, or background work of the thread's own (a
    /// job too, not the user's `!` command), of the orchestrator or a thread not parked.
    #[tokio::test]
    async fn active_is_work_of_the_orchestrator_or_a_thread_not_parked() {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(&dir).await;
        let active = |hub: &Hub| hub.glance().unwrap()["active"].clone();
        created(&hub, "o1", "orchestrator", json!({"role": "orchestrator"}));
        created(&hub, "t1", "api", json!({}));
        assert_eq!(active(&hub), false);
        // The orchestrator's turn.
        append(
            &hub,
            "o1",
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted::default()),
        );
        assert_eq!(active(&hub), true);
        append(
            &hub,
            "o1",
            Some(1),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        assert_eq!(active(&hub), false);
        // A worker's turn, then the same turn blocked on a permission.
        append(
            &hub,
            "t1",
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted::default()),
        );
        assert_eq!(active(&hub), true);
        append(
            &hub,
            "t1",
            Some(1),
            adapter(),
            raise("p1", AttentionReason::Permission),
        );
        assert_eq!(active(&hub), false);
        assert_eq!(hub.glance().unwrap()["permissions"], 1);
        let resolved = EventBody::AttentionResolved(AttentionResolved {
            attn_id: "p1".into(),
            by: ResolvedBy::User,
            ..Default::default()
        });
        append(&hub, "t1", Some(1), Source::User("test".into()), resolved);
        assert_eq!(active(&hub), true);
        append(
            &hub,
            "t1",
            Some(1),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        assert_eq!(active(&hub), false);
        // A runtime's own background task.
        append(&hub, "t1", None, adapter(), task("bg1", true, None));
        assert_eq!(active(&hub), true);
        append(&hub, "t1", None, adapter(), task("bg1", false, None));
        assert_eq!(active(&hub), false);
        // The user's `!` command is the user's work: the thread shows in the background
        // (counted as running, as the tabs count it) but is not at work.
        let shell = Some(json!({"job": {"shell": true}}));
        append(&hub, "t1", None, Source::Hub, task("job:s1", true, shell));
        let g = hub.glance().unwrap();
        assert_eq!(
            (g["active"].clone(), g["running"].clone()),
            (json!(false), json!(1))
        );
        append(&hub, "t1", None, Source::Hub, task("job:s1", false, None));
        // A job is the thread's work, until the thread is parked.
        append(&hub, "t1", None, Source::Hub, task("job:j1", true, None));
        assert_eq!(active(&hub), true);
        append(
            &hub,
            "t1",
            None,
            Source::Hub,
            EventBody::ThreadParked(SessionPayload::default()),
        );
        assert!(hub
            .inner
            .core()
            .registry
            .get("t1")
            .unwrap()
            .own_background());
        let g = hub.glance().unwrap();
        assert_eq!(
            (g["active"].clone(), g["running"].clone()),
            (json!(false), json!(0))
        );
        hub.shutdown().await;
    }

    /// The orchestrator's status, and its latest final reply of any turn as stored: a later
    /// turn's partial text changes nothing, a long reply is cut at 300 characters on a
    /// character boundary, and a reply capped in the log gives the preview the log keeps (no
    /// blob read).
    #[tokio::test]
    async fn orchestrator_status_and_latest_reply() {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(&dir).await;
        created(&hub, "o1", "orchestrator", json!({"role": "orchestrator"}));
        let orch = |hub: &Hub| hub.glance().unwrap()["orchestrator"].clone();
        assert_eq!(
            orch(&hub),
            json!({"id": "o1", "name": "orchestrator", "status": "idle", "preview": null,
                   "preview_seq": null})
        );
        let turn = |hub: &Hub, n: u64, texts: &[(&str, bool)]| -> Vec<Event> {
            append(
                hub,
                "o1",
                Some(n),
                Source::Hub,
                EventBody::TurnStarted(TurnStarted::default()),
            );
            texts
                .iter()
                .map(|(t, f)| append(hub, "o1", Some(n), adapter(), reply(t, *f)))
                .collect()
        };
        let first = turn(
            &hub,
            1,
            &[("thinking", false), ("All three are done.", true)],
        );
        assert_eq!(orch(&hub)["status"], "running");
        assert_eq!(orch(&hub)["preview"], "All three are done.");
        assert_eq!(orch(&hub)["preview_seq"], first[1].seq);
        append(
            &hub,
            "o1",
            Some(1),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        // A turn with only a partial text: the reply before it stays.
        turn(&hub, 2, &[("still looking", false)]);
        append(
            &hub,
            "o1",
            Some(2),
            adapter(),
            EventBody::TurnEnded(TurnEnded::default()),
        );
        let o = orch(&hub);
        assert_eq!(
            (o["status"].clone(), o["preview"].clone()),
            (json!("idle"), json!("All three are done."))
        );
        assert_eq!(o["preview_seq"], first[1].seq);
        // 400 four-byte characters (not capped): the first 300.
        let wide = "😀".repeat(400);
        let third = turn(&hub, 3, &[(&wide, true)]);
        let o = orch(&hub);
        assert_eq!(o["preview"], "😀".repeat(300));
        assert_eq!(o["preview_seq"], third[0].seq);
        // A reply too long for the log: its stored preview (512 bytes, 256 two-byte
        // characters), where the whole reply would give 300.
        let long = "é".repeat(3000);
        let fourth = turn(&hub, 4, &[(&long, true)]);
        let EventBody::AgentText(stored) =
            &store::event_at(hub.inner.core().store.conn(), "o1", fourth[0].seq)
                .unwrap()
                .unwrap()
                .body
        else {
            panic!()
        };
        assert!(
            stored.text.len() <= twr_core::blob::PREVIEW_MAX,
            "the reply is capped"
        );
        let o = orch(&hub);
        assert_eq!(o["preview"], "é".repeat(256));
        assert_eq!(o["preview_seq"], fourth[0].seq);
        hub.shutdown().await;
    }
}
